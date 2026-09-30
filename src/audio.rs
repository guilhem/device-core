//! Single audio slot extracted from the original mpg123/aplay player. Each
//! completion and cancellation belongs to one ID, never to a global high-water mark.
use crate::{
    common::{Events, Gate},
    options::Options,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::File,
    io,
    net::IpAddr,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{oneshot, watch},
};
use zbus::zvariant::Type;
use zbus::{fdo, message::Header, Connection};

pub const PATH: &str = "/io/github/guilhem/DeviceCore1/Audio";
pub const INTERFACE: &str = "io.github.guilhem.DeviceCore1.Audio";
const HISTORY: usize = 256;

#[derive(
    Clone,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Type,
    zbus::zvariant::Value,
    zbus::zvariant::OwnedValue,
)]
pub struct Status {
    pub id: String,
    pub state: String,
    pub volume: u32,
}

struct Playback {
    id: String,
    owner: Option<String>,
    kill: Option<oneshot::Sender<String>>,
    outcome: watch::Sender<Option<String>>,
}
struct State {
    next: u64,
    current: Option<String>,
    volume: u32,
    plays: VecDeque<Playback>,
}
struct Inner {
    options: Options,
    gate: Gate,
    events: Events,
    incarnation: String,
    state: Mutex<State>,
    operations: tokio::sync::Mutex<()>,
}
#[derive(Clone)]
pub struct Audio(Arc<Inner>);

// File descriptors are opened before preemption and passed directly to stdin.
// The canonical path is checked after open to avoid symlink replacement races.
enum Source {
    File { file: File, wav: bool },
    Stream(reqwest::Url),
}
impl Source {
    fn resolve(options: &Options, kind: &str, source: &str) -> fdo::Result<Self> {
        let invalid = || fdo::Error::InvalidArgs("invalid-source".into());
        if source.len() > 4096 || source.contains('\0') {
            return Err(invalid());
        }
        match kind {
            "file" => {
                use std::os::fd::AsRawFd;
                let path = Path::new(source).canonicalize().map_err(|_| invalid())?;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                    .open(&path)
                    .map_err(|_| invalid())?;
                if !file.metadata().map_err(|_| invalid())?.is_file() {
                    return Err(invalid());
                }
                let actual = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
                    .map_err(|_| invalid())?;
                if actual != path
                    || !options.audio_roots.iter().any(|root| {
                        root.canonicalize()
                            .is_ok_and(|root| actual.starts_with(root))
                    })
                {
                    return Err(invalid());
                }
                let ext = actual.extension().and_then(|s| s.to_str()).unwrap_or("");
                if !ext.eq_ignore_ascii_case("mp3") && !ext.eq_ignore_ascii_case("wav") {
                    return Err(invalid());
                }
                Ok(Self::File {
                    file,
                    wav: ext.eq_ignore_ascii_case("wav"),
                })
            }
            "stream" => {
                let url = reqwest::Url::parse(source).map_err(|_| invalid())?;
                let host = url.host_str().unwrap_or("").trim_matches(['[', ']']);
                if url.scheme() != "http"
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                    || !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
                {
                    return Err(invalid());
                }
                Ok(Self::Stream(url))
            }
            _ => Err(invalid()),
        }
    }
}

impl Audio {
    pub fn new(options: Options, gate: Gate, events: Events) -> io::Result<Self> {
        Ok(Self(Arc::new(Inner {
            options,
            gate,
            events,
            incarnation: crate::common::token()?,
            state: Mutex::new(State {
                next: 0,
                current: None,
                volume: 100,
                plays: VecDeque::new(),
            }),
            operations: tokio::sync::Mutex::new(()),
        })))
    }
    pub async fn serve(&self, bus: &Connection) -> zbus::Result<()> {
        bus.object_server()
            .at(PATH, DbusAudio(self.clone()))
            .await?;
        Ok(())
    }
    pub fn status(&self) -> Status {
        let s = self.0.state.lock().unwrap();
        Status {
            id: s.current.clone().unwrap_or_default(),
            state: if s.current.is_some() {
                "playing"
            } else {
                "idle"
            }
            .into(),
            volume: s.volume,
        }
    }
    fn emit(&self) {
        self.0.events.emit("audio", &self.status());
    }
    pub async fn start(&self, kind: &str, source: &str) -> fdo::Result<String> {
        self.start_owned(kind, source, None).await
    }
    async fn start_owned(
        &self,
        kind: &str,
        source: &str,
        owner: Option<String>,
    ) -> fdo::Result<String> {
        drop(self.0.gate.admit()?);
        let source = Source::resolve(&self.0.options, kind, source)?;
        let _op = self.0.operations.lock().await;
        drop(self.0.gate.admit()?);
        let current = self.0.state.lock().unwrap().current.clone();
        if let Some(id) = current {
            self.stop_locked(&id, "preempted").await?;
        }
        let (kill, killed) = oneshot::channel();
        let (outcome, _) = watch::channel(None);
        let id = {
            let _admission = self.0.gate.admit()?;
            let mut s = self.0.state.lock().unwrap();
            s.next = s
                .next
                .checked_add(1)
                .ok_or_else(|| fdo::Error::Failed("playback-overflow".into()))?;
            let id = format!("{}:{}", self.0.incarnation, s.next);
            s.current = Some(id.clone());
            s.plays.push_back(Playback {
                id: id.clone(),
                owner,
                kill: Some(kill),
                outcome,
            });
            while s.plays.len() > HISTORY + 1
                && s.plays
                    .front()
                    .is_some_and(|p| p.outcome.borrow().is_some())
            {
                s.plays.pop_front();
            }
            id
        };
        self.emit();
        let me = self.clone();
        let playing_id = id.clone();
        tokio::spawn(async move {
            let outcome = me.play(source, killed).await;
            {
                let mut s = me.0.state.lock().unwrap();
                if s.current.as_deref() == Some(&playing_id) {
                    s.current = None;
                }
                if let Some(p) = s.plays.iter_mut().find(|p| p.id == playing_id) {
                    p.kill = None;
                    p.outcome.send_replace(Some(outcome));
                }
                while s.plays.len() > HISTORY
                    && s.plays
                        .front()
                        .is_some_and(|p| p.outcome.borrow().is_some())
                {
                    s.plays.pop_front();
                }
            }
            me.emit();
        });
        Ok(id)
    }
    fn receiver(&self, id: &str) -> fdo::Result<watch::Receiver<Option<String>>> {
        self.0
            .state
            .lock()
            .unwrap()
            .plays
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.outcome.subscribe())
            .ok_or_else(|| fdo::Error::InvalidArgs("unknown-playback".into()))
    }
    pub async fn wait(&self, id: &str) -> fdo::Result<String> {
        let mut rx = self.receiver(id)?;
        let value = rx
            .wait_for(|value| value.is_some())
            .await
            .map_err(|_| fdo::Error::Failed("playback-lost".into()))?;
        Ok(value.as_ref().unwrap().clone())
    }
    async fn stop_locked(&self, id: &str, reason: &str) -> fdo::Result<()> {
        let mut rx = self.receiver(id)?;
        {
            let mut s = self.0.state.lock().unwrap();
            if let Some(kill) = s
                .plays
                .iter_mut()
                .find(|p| p.id == id)
                .and_then(|p| p.kill.take())
            {
                let _ = kill.send(reason.into());
            }
        }
        rx.wait_for(|outcome| outcome.is_some())
            .await
            .map_err(|_| fdo::Error::Failed("playback-lost".into()))?;
        Ok(())
    }
    pub async fn stop(&self, id: &str) -> fdo::Result<()> {
        let _op = self.0.operations.lock().await;
        self.stop_locked(id, "stopped").await
    }
    pub async fn owner_lost(&self, owner: &str) {
        let _op = self.0.operations.lock().await;
        let id = {
            let s = self.0.state.lock().unwrap();
            s.plays
                .iter()
                .find(|p| p.owner.as_deref() == Some(owner) && s.current.as_ref() == Some(&p.id))
                .map(|p| p.id.clone())
        };
        if let Some(id) = id {
            let _ = self.stop_locked(&id, "owner-lost").await;
        }
    }
    pub async fn set_volume(&self, percent: u32) -> fdo::Result<()> {
        if percent > 100 {
            return Err(fdo::Error::InvalidArgs("invalid-volume".into()));
        }
        let _op = self.0.operations.lock().await;
        if !self.0.options.simulate {
            let volume = format!("{:.2}", f64::from(percent) / 100.0);
            let apply = async {
                loop {
                    let status = tokio::process::Command::new("wpctl")
                        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &volume])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .kill_on_drop(true)
                        .status()
                        .await;
                    if matches!(status, Ok(s) if s.success()) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            };
            tokio::time::timeout(Duration::from_secs(5), apply)
                .await
                .map_err(|_| fdo::Error::Failed("audio-volume-unavailable".into()))?;
        }
        self.0.state.lock().unwrap().volume = percent;
        self.emit();
        Ok(())
    }
    async fn play(&self, source: Source, mut killed: oneshot::Receiver<String>) -> String {
        if self.0.options.simulate {
            return tokio::select! {
                biased;
                reason = &mut killed => reason.unwrap_or_else(|_| "stopped".into()),
                _ = tokio::time::sleep(Duration::from_millis(self.0.options.sim_audio_ms)) => "completed".into(),
            };
        }
        // A preempted job must not launch after its cancellation was queued.
        if let Ok(reason) = killed.try_recv() {
            return reason;
        }
        let wav = matches!(&source, Source::File { wav: true, .. });
        let mut cmd = tokio::process::Command::new(if wav { "aplay" } else { "mpg123" });
        if wav {
            cmd.args(["-q", "-D", &self.0.options.alsa_device]);
        } else {
            cmd.args(["-q", "-o", "alsa", "-a", &self.0.options.alsa_device]);
        }
        let url = match source {
            Source::File { file, .. } => {
                cmd.stdin(Stdio::from(file));
                None
            }
            Source::Stream(url) => {
                cmd.stdin(Stdio::piped());
                Some(url)
            }
        };
        cmd.arg("-")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(_) => return "failed".into(),
        };
        let result = {
            let stdin = child.stdin.take();
            let playback = async {
                if let Some(url) = url {
                    let input = async {
                        let client = reqwest::Client::builder()
                            .no_proxy()
                            .redirect(reqwest::redirect::Policy::none())
                            .connect_timeout(Duration::from_secs(5))
                            .build()
                            .map_err(|_| ())?;
                        let mut response = client.get(url).send().await.map_err(|_| ())?;
                        if !response.status().is_success() {
                            return Err(());
                        }
                        let mut stdin = stdin.ok_or(())?;
                        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
                            stdin.write_all(&chunk).await.map_err(|_| ())?;
                        }
                        stdin.shutdown().await.map_err(|_| ())
                    };
                    tokio::select! {
                        status = child.wait() => return status.map_err(|_| ()),
                        done = input => done?,
                    }
                }
                child.wait().await.map_err(|_| ())
            };
            tokio::select! {
                biased;
                reason = &mut killed => Err(reason.unwrap_or_else(|_| "stopped".into())),
                status = playback => match status {
                    Ok(status) if status.success() => Ok(()),
                    _ => Err("failed".into()),
                },
            }
        };
        if result.is_err() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        match result {
            Ok(()) => "completed".into(),
            Err(reason) => reason,
        }
    }
}

#[derive(Clone)]
struct DbusAudio(Audio);
#[zbus::interface(name = "io.github.guilhem.DeviceCore1.Audio")]
impl DbusAudio {
    async fn start(
        &self,
        kind: &str,
        source: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] bus: &Connection,
    ) -> fdo::Result<String> {
        let owner = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("missing-sender".into()))?
            .to_string();
        let id = self
            .0
            .start_owned(kind, source, Some(owner.clone()))
            .await?;
        let dbus = zbus::fdo::DBusProxy::new(bus).await?;
        if !dbus
            .name_has_owner(
                owner
                    .as_str()
                    .try_into()
                    .map_err(|_| fdo::Error::AccessDenied("missing-sender".into()))?,
            )
            .await?
        {
            self.0.owner_lost(&owner).await;
            return Err(fdo::Error::Failed("owner-lost".into()));
        }
        Ok(id)
    }
    async fn stop(&self, id: &str) -> fdo::Result<()> {
        self.0.stop(id).await
    }
    async fn wait(&self, id: &str) -> fdo::Result<String> {
        self.0.wait(id).await
    }
    #[zbus(property)]
    fn status(&self) -> Status {
        self.0.status()
    }
    async fn set_volume(&self, percent: u32) -> fdo::Result<()> {
        self.0.set_volume(percent).await
    }
    #[zbus(signal)]
    async fn changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        status: Status,
    ) -> zbus::Result<()>;
}
