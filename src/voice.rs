//! LVA peripheral transport. Config is the only enablement writer.
use crate::{
    common::{Events, Gate},
    config::{failed, Store},
    options::Options,
    system::System,
};
use futures_util::{future::poll_fn, Sink, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};
use zbus::{fdo, object_server::SignalEmitter};

const SEND_LIMIT: Duration = Duration::from_secs(3);
const CONNECT_LIMIT: Duration = Duration::from_secs(5);
const MAX_MESSAGE: usize = 64 * 1024;

struct Request {
    command: String,
    result: oneshot::Sender<fdo::Result<()>>,
}

#[derive(Default)]
struct Runtime {
    task: Option<JoinHandle<()>>,
    commands: Option<mpsc::Sender<Request>>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct Voice {
    options: Arc<Options>,
    config: Store,
    system: System,
    events: Events,
    gate: Gate,
    runtime: Arc<Mutex<Runtime>>,
    state: watch::Sender<String>,
}

impl Voice {
    pub fn new(
        options: Options,
        config: Store,
        system: System,
        events: Events,
        gate: Gate,
    ) -> Self {
        let (state, _) = watch::channel(
            if options.lva_unit.is_empty() {
                "unsupported"
            } else {
                "disabled"
            }
            .into(),
        );
        Self {
            options: Arc::new(options),
            config,
            system,
            events,
            gate,
            runtime: Arc::new(Mutex::new(Runtime::default())),
            state,
        }
    }

    pub fn supported(&self) -> bool {
        !self.options.lva_unit.is_empty()
    }
    pub fn status(&self) -> String {
        self.state.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<String> {
        self.state.subscribe()
    }

    /// Only persists desired state; the parent's Config watch applies runtime.
    pub async fn enable(&self, enabled: bool) -> fdo::Result<()> {
        if enabled && !self.supported() {
            return Err(fdo::Error::NotSupported("voice unit not configured".into()));
        }
        let (revision, mut settings) = self.config.read()?;
        {
            let _admission = if enabled {
                Some(self.gate.admit()?)
            } else {
                None
            };
            settings.voice_enabled = enabled;
        }
        // Accepted desired-state persistence may finish during maintenance;
        // the parent still gates runtime application. Never fsync under Gate.
        self.config.update(&revision, settings)?;
        Ok(())
    }

    /// Apply desired configuration or temporarily stop it during maintenance.
    /// No Config calls under the runtime lock, and no persistence here.
    pub async fn enable_runtime(&self, enabled: bool) -> fdo::Result<()> {
        if enabled {
            {
                let _admission = self.gate.admit()?;
            }
            if !self.supported() {
                return Err(fdo::Error::NotSupported("voice unit not configured".into()));
            }
        }
        let mut runtime = timeout(Duration::from_secs(20), self.runtime.lock())
            .await
            .map_err(failed)?;
        if enabled {
            {
                let _admission = self.gate.admit()?;
            }
            if runtime
                .task
                .as_ref()
                .is_some_and(|task| !task.is_finished())
            {
                return Ok(());
            }
        }
        runtime.commands = None;
        if let Some(task) = runtime.task.take() {
            task.abort();
            let _ = timeout(Duration::from_secs(1), task).await;
        }
        if !self.supported() {
            self.set_status("unsupported");
            return Ok(());
        }
        if !enabled {
            self.set_status("disabled");
            return self.system.unit_job(&self.options.lva_unit, false).await;
        }
        self.set_status("connecting");
        if let Err(e) = self.system.unit_job(&self.options.lva_unit, true).await {
            self.set_status("disconnected");
            return Err(e);
        }
        // Gate may have closed while StartUnit was pending. Do not leave LVA up.
        let (commands, receiver) = mpsc::channel(8);
        let admission = {
            self.gate.admit().map(|_guard| {
                runtime.commands = Some(commands);
            })
        };
        if let Err(e) = admission {
            self.set_status("disabled");
            self.system.unit_job(&self.options.lva_unit, false).await?;
            return Err(e);
        }
        let options = self.options.clone();
        let events = self.events.clone();
        let gate = self.gate.clone();
        let state = self.state.clone();
        runtime.task = Some(tokio::spawn(async move {
            run(options, events, gate, state, receiver).await;
        }));
        Ok(())
    }

    pub async fn command(&self, command: &str) -> fdo::Result<()> {
        validate_command(command)?;
        {
            let _admission = self.gate.admit()?;
        }
        // Bound lock acquisition, enqueue and acknowledgement together.
        timeout(SEND_LIMIT, async {
            let sender = self
                .runtime
                .lock()
                .await
                .commands
                .clone()
                .ok_or_else(|| failed("voice assistant disabled"))?;
            let (result, response) = oneshot::channel();
            {
                let _admission = self.gate.admit()?;
                sender
                    .try_send(Request {
                        command: command.into(),
                        result,
                    })
                    .map_err(|_| failed("voice queue full or disconnected"))?;
            }
            response
                .await
                .map_err(|_| failed("voice assistant disconnected"))?
        })
        .await
        .map_err(|_| failed("voice command timed out"))?
    }

    fn set_status(&self, status: &str) {
        set_status(&self.state, &self.events, status);
    }
}

#[zbus::interface(name = "io.github.guilhem.DeviceCore1.Voice")]
impl Voice {
    #[zbus(property, name = "Supported")]
    fn dbus_supported(&self) -> bool {
        self.supported()
    }
    #[zbus(property, name = "Status")]
    fn dbus_status(&self) -> String {
        self.status()
    }
    #[zbus(name = "Enable")]
    async fn dbus_enable(&self, enabled: bool) -> fdo::Result<()> {
        self.enable(enabled).await
    }
    #[zbus(name = "Command")]
    async fn dbus_command(&self, command: &str) -> fdo::Result<()> {
        self.command(command).await
    }
    #[zbus(signal, name = "Event")]
    pub async fn event(
        emitter: &SignalEmitter<'_>,
        event: &str,
        data_json: &str,
    ) -> zbus::Result<()>;
}

fn set_status(state: &watch::Sender<String>, events: &Events, status: &str) {
    if state.send_if_modified(|current| {
        if current == status {
            return false;
        }
        *current = status.into();
        true
    }) {
        events.emit(
            "voice",
            &json!({"event": "status", "data": {"status": status}}),
        );
    }
}

pub fn validate_command(command: &str) -> fdo::Result<()> {
    if command.is_empty()
        || command.len() > 128
        || !command
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(fdo::Error::InvalidArgs(
            "invalid peripheral command name".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct PeripheralEvent {
    event: String,
    #[serde(default)]
    data: Value,
}

fn handle_event(raw: &[u8], state: &watch::Sender<String>, events: &Events) {
    let Ok(message) = serde_json::from_slice::<PeripheralEvent>(raw) else {
        return;
    };
    if validate_command(&message.event).is_err() {
        return;
    }
    let status = match message.event.as_str() {
        "wake_word_detected"
        | "listening"
        | "thinking"
        | "tts_speaking"
        | "timer_ringing"
        | "idle"
        | "media_player_playing"
        | "disconnected" => Some(message.event.as_str()),
        "tts_finished" | "pipeline_error" => Some("idle"),
        "snapshot" => Some(
            if message.data.get("muted").and_then(Value::as_bool) == Some(true) {
                "muted"
            } else {
                "idle"
            },
        ),
        "muted" => Some(
            if message
                .data
                .get("muted")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                "muted"
            } else {
                "idle"
            },
        ),
        "zeroconf" if message.data.get("status").and_then(Value::as_str) == Some("connected") => {
            Some("idle")
        }
        _ => None,
    };
    if let Some(status) = status {
        set_status(state, events, status);
    }
    events.emit(
        "voice",
        &json!({"event": message.event, "data": message.data}),
    );
}

async fn run(
    options: Arc<Options>,
    events: Events,
    gate: Gate,
    state: watch::Sender<String>,
    mut commands: mpsc::Receiver<Request>,
) {
    if options.simulate {
        set_status(&state, &events, "idle");
        while let Some(request) = commands.recv().await {
            if !request.result.is_closed() {
                let result = { gate.admit().map(|_| ()) };
                let _ = request.result.send(result);
            }
        }
        return;
    }
    // No queue survives a disconnect: commands must never replay after recovery.
    loop {
        set_status(&state, &events, "connecting");
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let connecting = timeout(
            CONNECT_LIMIT,
            connect_async_with_config(&options.lva_url, Some(config), false),
        );
        tokio::pin!(connecting);
        let connected = loop {
            tokio::select! {
                result = &mut connecting => break result,
                request = commands.recv() => match request {
                    Some(request) => { let _ = request.result.send(Err(failed("voice assistant not connected"))); }
                    None => return,
                }
            }
        };
        if let Ok(Ok((mut socket, _))) = connected {
            set_status(&state, &events, "idle");
            loop {
                tokio::select! {
                    request = commands.recv() => {
                        let Some(request) = request else { return; };
                        if request.result.is_closed() { continue; }
                        let raw = json!({"command": request.command}).to_string();
                        let sent = timeout(SEND_LIMIT, async {
                            poll_fn(|cx| std::pin::Pin::new(&mut socket).poll_ready(cx)).await.map_err(failed)?;
                            {
                                let _admission = gate.admit()?;
                                std::pin::Pin::new(&mut socket).start_send(Message::Text(raw.into())).map_err(failed)?;
                            }
                            socket.flush().await.map_err(failed)
                        }).await;
                        let result = match sent {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(failed("voice write timed out")),
                        };
                        let broken = result.is_err();
                        let _ = request.result.send(result);
                        if broken { break; }
                    }
                    received = socket.next() => match received {
                        Some(Ok(Message::Text(raw))) => handle_event(raw.as_bytes(), &state, &events),
                        Some(Ok(Message::Binary(raw))) => handle_event(&raw, &state, &events),
                        Some(Ok(Message::Ping(raw))) => {
                            if !matches!(timeout(SEND_LIMIT, socket.send(Message::Pong(raw))).await, Ok(Ok(()))) { break; }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
            }
        }
        set_status(&state, &events, "disconnected");
        events.emit("voice", &json!({"event": "disconnected", "data": null}));
        let retry = sleep(Duration::from_secs(3));
        tokio::pin!(retry);
        loop {
            tokio::select! {
                _ = &mut retry => break,
                request = commands.recv() => match request {
                    Some(request) => { let _ = request.result.send(Err(failed("voice assistant disconnected"))); }
                    None => return,
                }
            }
        }
    }
}
