use crate::{
    audio::Audio,
    common::{token, Events, Gate, ROOT, SERVICE},
    config::{Settings, Store},
    maintenance::{Coordinator, Reservation},
    manager::Manager,
    network::{self, Network},
    options::Options,
    system::System,
    update::{self, Hooks, Updater},
    voice::Voice,
};
use futures_util::StreamExt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{watch, Mutex};
use zbus::Connection;

#[derive(Clone)]
pub struct App {
    pub bus: Connection,
    pub manager: Manager,
    pub config: Store,
    pub network: Network,
    pub audio: Audio,
    pub system: System,
    pub voice: Voice,
    pub updates: Updater,
    pub events: Events,
}

fn update_settings(settings: &Settings, system: &System) -> update::Settings {
    update::Settings {
        auto_check_updates: settings.auto_check_updates,
        automatic: settings.updates.automatic,
        channel: settings.updates.channel.clone(),
        start_hour: settings.updates.start.hour,
        start_min: settings.updates.start.min,
        end_hour: settings.updates.end.hour,
        end_min: settings.updates.end.min,
        timezone: settings.timezone.clone(),
        time_reliable: system
            .clock()
            .is_ok_and(|(quality, _)| matches!(quality.as_str(), "ntp" | "manual")),
    }
}

pub async fn start(options: Options) -> Result<App, Box<dyn std::error::Error>> {
    let address = std::env::var("DEVICE_CORE_BUS_ADDRESS")
        .ok()
        .or_else(|| std::env::var("DBUS_SYSTEM_BUS_ADDRESS").ok());
    if options.simulate && address.is_none() {
        return Err("simulation requires an explicit private D-Bus address".into());
    }
    let builder = if let Some(ref address) = address {
        zbus::connection::Builder::address(address.as_str())?
    } else {
        zbus::connection::Builder::system()?
    };
    let bus = builder.build().await?;
    let events = Events::default();
    let gate = Gate::default();
    gate.set(true);
    let config = Store::new(options.data_dir.join("settings.json"), events.clone())?;
    let audio = Audio::new(options.clone(), gate.clone(), events.clone())?;
    let system = System::new(bus.clone(), options.clone(), events.clone());
    let voice = Voice::new(
        options.clone(),
        config.clone(),
        system.clone(),
        events.clone(),
        gate.clone(),
    );
    let coordinator = Coordinator::new(bus.clone(), options.clone(), gate.clone(), events.clone());
    let held: Arc<Mutex<Option<(String, Reservation)>>> = Arc::default();
    let acquire = {
        let (held, coordinator, audio, voice) = (
            held.clone(),
            coordinator.clone(),
            audio.clone(),
            voice.clone(),
        );
        Arc::new(move |id: String| -> update::HookFuture {
            let (held, coordinator, audio, voice) = (
                held.clone(),
                coordinator.clone(),
                audio.clone(),
                voice.clone(),
            );
            Box::pin(async move {
                let mut held = held.lock().await;
                if let Some((operation, _)) = held.as_ref() {
                    return if operation == &id {
                        Ok(())
                    } else {
                        Err("maintenance-busy".into())
                    };
                }
                if audio.status().state != "idle"
                    || matches!(
                        voice.status().as_str(),
                        "wake_word_detected"
                            | "listening"
                            | "thinking"
                            | "tts_speaking"
                            | "timer_ringing"
                            | "media_player_playing"
                    )
                {
                    return Err("device activity in progress".into());
                }
                voice
                    .enable_runtime(false)
                    .await
                    .map_err(|e| e.to_string())?;
                let reservation = coordinator.acquire(&id).await.map_err(|e| e.to_string())?;
                *held = Some((id, reservation));
                Ok(())
            })
        })
    };
    let release = {
        let (held, coordinator, config, voice) = (
            held.clone(),
            coordinator.clone(),
            config.clone(),
            voice.clone(),
        );
        Arc::new(move |id: String| -> update::HookFuture {
            let (held, coordinator, config, voice) = (
                held.clone(),
                coordinator.clone(),
                config.clone(),
                voice.clone(),
            );
            Box::pin(async move {
                let mut held = held.lock().await;
                coordinator
                    .abort_pending(&id)
                    .await
                    .map_err(|e| e.to_string())?;
                if let Some((operation, reservation)) = held.as_mut() {
                    if operation != &id {
                        return Err("wrong maintenance operation".into());
                    }
                    reservation.release().await.map_err(|e| e.to_string())?;
                    *held = None;
                }
                coordinator.gate.set(false);
                if let Ok((_, settings)) = config.read() {
                    voice
                        .enable_runtime(settings.voice_enabled)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            })
        })
    };
    let reboot = {
        let system = system.clone();
        Arc::new(move |_id: String| -> update::HookFuture {
            let system = system.clone();
            Box::pin(async move { system.reboot().await.map_err(|e| e.to_string()) })
        })
    };
    let (_, initial) = config.read()?;
    let (update_tx, update_rx) = watch::channel(update_settings(&initial, &system));
    let updates = Updater::new(
        bus.clone(),
        &options,
        events.clone(),
        gate.clone(),
        update_rx,
        Hooks {
            acquire,
            release,
            reboot,
        },
    )
    .await
    .map_err(std::io::Error::other)?;
    system.attach_updater(updates.clone())?;
    let manager = Manager {
        coordinator: coordinator.clone(),
        events: events.clone(),
        gate: gate.clone(),
        instance: token()?,
        ready: Arc::new(AtomicBool::new(false)),
        options: options.clone(),
    };
    bus.object_server().at(ROOT, manager.clone()).await?;
    bus.object_server()
        .at(format!("{ROOT}/Config"), config.clone())
        .await?;
    bus.object_server()
        .at(format!("{ROOT}/System"), system.clone())
        .await?;
    bus.object_server()
        .at(format!("{ROOT}/Voice"), voice.clone())
        .await?;
    bus.object_server()
        .at(format!("{ROOT}/Updates"), updates.clone())
        .await?;
    audio.serve(&bus).await?;
    let network = network::start(&bus, options, events.clone()).await?;
    let app = App {
        bus: bus.clone(),
        manager,
        config,
        network,
        audio,
        system,
        voice,
        updates,
        events,
    };
    spawn_events(&app).await?;
    {
        let (audio, voice, system, gate) = (
            app.audio.clone(),
            app.voice.clone(),
            app.system.clone(),
            gate,
        );
        let mut settings = app.config.subscribe();
        tokio::spawn(async move {
            let mut last_volume = None;
            let mut last_voice = None;
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                let desired = settings.borrow_and_update().clone();
                update_tx.send_replace(update_settings(&desired, &system));
                if last_volume != Some(desired.volume) {
                    match audio.set_volume(desired.volume).await {
                        Ok(()) => last_volume = Some(desired.volume),
                        Err(e) => crate::warn!("volume: {e}"),
                    }
                }
                let enabled = desired.voice_enabled && !gate.blocked();
                if last_voice != Some(enabled) {
                    match voice.enable_runtime(enabled).await {
                        Ok(()) => last_voice = Some(enabled),
                        Err(e) => crate::warn!("voice: {e}"),
                    }
                }
                tokio::select! {
                    changed = settings.changed() => if changed.is_err() { break; },
                    _ = ticker.tick() => (),
                }
            }
        });
    }
    // Publish the name only after every object and recovery task exists.
    bus.request_name(SERVICE).await?;
    app.manager.ready.store(true, Ordering::SeqCst);
    Ok(app)
}

async fn spawn_events(app: &App) -> zbus::Result<()> {
    let proxy = zbus::fdo::DBusProxy::new(&app.bus).await?;
    let mut owners = proxy.receive_name_owner_changed().await?;
    let (audio, coordinator) = (app.audio.clone(), app.manager.coordinator.clone());
    tokio::spawn(async move {
        while let Some(signal) = owners.next().await {
            if let Ok(args) = signal.args() {
                if args.new_owner().is_none() {
                    coordinator.lost(args.name().as_str());
                    audio.owner_lost(args.name().as_str()).await;
                }
            }
        }
    });
    let mut events = app.events.0.subscribe();
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = app
                        .bus
                        .emit_signal(
                            None::<&str>,
                            ROOT,
                            "io.github.guilhem.DeviceCore1.Manager",
                            "Resync",
                            &(),
                        )
                        .await;
                    continue;
                }
                Err(_) => break,
            };
            let json = event.data.to_string();
            let _ = app
                .bus
                .emit_signal(
                    None::<&str>,
                    ROOT,
                    "io.github.guilhem.DeviceCore1.Manager",
                    "Event",
                    &(event.domain.as_str(), json.as_str()),
                )
                .await;
            let domain = match event.domain.as_str() {
                "audio" => "Audio",
                "updates" => "Updates",
                "voice" => "Voice",
                "maintenance" => "Manager",
                _ => continue,
            };
            let path = if domain == "Manager" {
                ROOT.to_owned()
            } else {
                format!("{ROOT}/{domain}")
            };
            let iface = format!("io.github.guilhem.DeviceCore1.{domain}");
            let properties = if domain == "Manager" {
                vec!["Maintenance"]
            } else {
                vec!["Status"]
            };
            let empty: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
                Default::default();
            let _ = app
                .bus
                .emit_signal(
                    None::<&str>,
                    path.as_str(),
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                    &(iface.as_str(), empty, properties),
                )
                .await;
            if domain == "Audio" {
                if let Ok(status) =
                    serde_json::from_value::<crate::audio::Status>(event.data.clone())
                {
                    let _ = app
                        .bus
                        .emit_signal(
                            None::<&str>,
                            path.as_str(),
                            iface.as_str(),
                            "Changed",
                            &(status,),
                        )
                        .await;
                }
            }
            if domain == "Voice" {
                if let Some(name) = event.data.get("event").and_then(|v| v.as_str()) {
                    let data = event
                        .data
                        .get("data")
                        .unwrap_or(&serde_json::Value::Null)
                        .to_string();
                    let _ = app
                        .bus
                        .emit_signal(
                            None::<&str>,
                            path.as_str(),
                            iface.as_str(),
                            "Event",
                            &(name, data.as_str()),
                        )
                        .await;
                }
            }
        }
    });
    Ok(())
}
