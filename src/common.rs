use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock, RwLockReadGuard,
};
use std::time::Duration;
use tokio::sync::broadcast;

pub const SERVICE: &str = "io.github.guilhem.DeviceCore1";
pub const ROOT: &str = "/io/github/guilhem/DeviceCore1";

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub domain: String,
    pub data: serde_json::Value,
}

#[derive(Clone)]
pub struct Events(pub broadcast::Sender<Event>);
impl Default for Events {
    fn default() -> Self {
        Self(broadcast::channel(256).0)
    }
}
impl Events {
    pub fn emit(&self, domain: &str, value: &impl Serialize) {
        if let Ok(data) = serde_json::to_value(value) {
            let _ = self.0.send(Event {
                domain: domain.into(),
                data,
            });
        }
    }
}

#[derive(Clone, Default)]
pub struct Gate {
    blocked: Arc<AtomicBool>,
    admission: Arc<RwLock<()>>,
}
impl Gate {
    pub fn blocked(&self) -> bool {
        self.blocked.load(Ordering::SeqCst)
    }
    pub fn set(&self, blocked: bool) {
        let _guard = self.admission.write().unwrap();
        self.blocked.store(blocked, Ordering::SeqCst);
    }
    /// Hold only across the synchronous acceptance step, never an await.
    pub fn admit(&self) -> zbus::fdo::Result<RwLockReadGuard<'_, ()>> {
        let guard = self.admission.read().unwrap();
        if self.blocked() {
            Err(zbus::fdo::Error::Failed("maintenance".into()))
        } else {
            Ok(guard)
        }
    }
}

pub fn monotonic() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return Duration::ZERO;
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

pub fn token() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
