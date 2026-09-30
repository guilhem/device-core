//! System settings only. The parent owns runtime application of watch updates.
use crate::common::{token, Events};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
use zbus::{fdo, zvariant::Type};

pub const VERSION: u32 = 1;
const MAX_FILE: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
#[serde(deny_unknown_fields)]
pub struct HM {
    pub hour: u32,
    pub min: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
#[serde(deny_unknown_fields)]
pub struct Updates {
    pub automatic: bool,
    pub channel: String,
    pub start: HM,
    pub end: HM,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[zvariant(crate = "zbus::zvariant")]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub locale: String,
    pub timezone: String,
    pub volume: u32,
    pub auto_check_updates: bool,
    pub updates: Updates,
    pub voice_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            locale: "fr_FR".into(),
            timezone: "Europe/Paris".into(),
            volume: 100,
            auto_check_updates: true,
            updates: Updates {
                automatic: false,
                channel: "stable".into(),
                start: HM { hour: 3, min: 0 },
                end: HM { hour: 5, min: 0 },
            },
            voice_enabled: false,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> fdo::Result<()> {
        let locale = self.locale.as_bytes();
        if locale.len() != 5
            || !locale[..2].iter().all(u8::is_ascii_lowercase)
            || locale[2] != b'_'
            || !locale[3..].iter().all(u8::is_ascii_uppercase)
        {
            return Err(invalid("locale must have the form fr_FR"));
        }
        if self.timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(invalid("unknown timezone"));
        }
        if self.volume > 100 {
            return Err(invalid("volume must be 0-100"));
        }
        let u = &self.updates;
        if !matches!(u.channel.as_str(), "stable" | "test") {
            return Err(invalid("unknown update channel"));
        }
        if u.start.hour >= 24
            || u.end.hour >= 24
            || u.start.min >= 60
            || u.end.min >= 60
            || u.start == u.end
        {
            return Err(invalid("invalid update window"));
        }
        if u.automatic && !self.auto_check_updates {
            return Err(invalid("automatic installation requires update checks"));
        }
        Ok(())
    }

    pub fn from_env() -> fdo::Result<Self> {
        let mut s = Self::default();
        macro_rules! env {
            ($name:literal, $field:expr) => {
                if let Ok(value) = std::env::var(concat!("DEVICE_CORE_DEFAULT_", $name)) {
                    $field = value
                        .parse()
                        .map_err(|_| invalid(concat!("invalid default ", $name)))?;
                }
            };
        }
        env!("LOCALE", s.locale);
        env!("TIMEZONE", s.timezone);
        env!("VOLUME", s.volume);
        env!("AUTO_CHECK_UPDATES", s.auto_check_updates);
        env!("UPDATES_AUTOMATIC", s.updates.automatic);
        env!("UPDATES_CHANNEL", s.updates.channel);
        env!("VOICE_ENABLED", s.voice_enabled);
        for (name, hm) in [("START", &mut s.updates.start), ("END", &mut s.updates.end)] {
            if let Ok(value) = std::env::var(format!("DEVICE_CORE_DEFAULT_UPDATES_{name}")) {
                let (hour, min) = value
                    .split_once(':')
                    .ok_or_else(|| invalid("default time must be HH:MM"))?;
                *hm = HM {
                    hour: hour.parse().map_err(|_| invalid("invalid default hour"))?,
                    min: min.parse().map_err(|_| invalid("invalid default minute"))?,
                };
            }
        }
        s.validate()?;
        Ok(s)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    settings: Settings,
}

struct State {
    settings: Settings,
    counter: u64,
    revision: String,
}

#[derive(Clone)]
pub struct Store {
    path: Arc<PathBuf>,
    incarnation: Arc<String>,
    state: Arc<Mutex<State>>,
    changes: watch::Sender<Settings>,
    events: Events,
}

impl Store {
    /// `path` is the settings.json file, not its directory.
    pub fn new(path: impl AsRef<Path>, events: Events) -> fdo::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let settings = match read_document(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let s = Settings::from_env()?;
                write_settings(&path, &s).map_err(failed)?;
                s
            }
            Err(e) => return Err(failed(e)),
        };
        sync_voice_flag(&path, settings.voice_enabled).map_err(failed)?;
        let incarnation = token().map_err(failed)?;
        let revision = revision(&incarnation, 0, &settings);
        let (changes, _) = watch::channel(settings.clone());
        Ok(Self {
            path: Arc::new(path),
            incarnation: Arc::new(incarnation),
            state: Arc::new(Mutex::new(State {
                settings,
                counter: 0,
                revision,
            })),
            changes,
            events,
        })
    }

    pub fn read(&self) -> fdo::Result<(String, Settings)> {
        let state = self.state.lock().map_err(failed)?;
        Ok((state.revision.clone(), state.settings.clone()))
    }

    pub fn subscribe(&self) -> watch::Receiver<Settings> {
        self.changes.subscribe()
    }

    /// Persistence and CAS share one lock; no domain callbacks under this lock.
    pub fn update(&self, expected_revision: &str, settings: Settings) -> fdo::Result<String> {
        settings.validate()?;
        let mut state = self.state.lock().map_err(failed)?;
        if expected_revision != state.revision {
            return Err(fdo::Error::Failed(
                "revision conflict; read current settings".into(),
            ));
        }
        if state.settings == settings {
            sync_voice_flag(&self.path, settings.voice_enabled).map_err(failed)?;
            return Ok(state.revision.clone());
        }
        let counter = state
            .counter
            .checked_add(1)
            .ok_or_else(|| failed("revision exhausted"))?;
        let write = write_settings(&self.path, &settings);
        // A rename followed by failed directory fsync is an uncertain commit.
        // Adopt the visible document, invalidate old CAS, and still return failure.
        let committed = write.is_ok() || read_document(&self.path).ok().as_ref() == Some(&settings);
        let flag = if committed {
            sync_voice_flag(&self.path, settings.voice_enabled)
        } else {
            Ok(())
        };
        if committed {
            state.revision = revision(&self.incarnation, counter, &settings);
            state.counter = counter;
            state.settings = settings.clone();
            self.changes.send_replace(settings.clone());
            self.events.emit(
                "config",
                &serde_json::json!({
                    "revision": state.revision, "settings": settings,
                }),
            );
        }
        write.map_err(failed)?;
        // settings.json is authoritative after a crash between these two files.
        flag.map_err(failed)?;
        Ok(state.revision.clone())
    }
}

#[zbus::interface(name = "io.github.guilhem.DeviceCore1.Config")]
impl Store {
    #[zbus(name = "Read")]
    fn dbus_read(&self) -> fdo::Result<(String, Settings)> {
        self.read()
    }

    #[zbus(name = "Update")]
    fn dbus_update(&self, expected_revision: &str, settings: Settings) -> fdo::Result<String> {
        self.update(expected_revision, settings)
    }
}

fn revision(incarnation: &str, counter: u64, settings: &Settings) -> String {
    let hash = Sha256::digest(serde_json::to_vec(settings).expect("settings are serializable"));
    format!("{incarnation}:{counter}:{hash:x}")
}

fn read_document(path: &Path) -> io::Result<Settings> {
    let raw = read_bounded(path, MAX_FILE)?;
    let doc: Document = serde_json::from_slice(&raw).map_err(io::Error::other)?;
    if doc.version != VERSION {
        return Err(io::Error::other("unsupported settings version"));
    }
    doc.settings.validate().map_err(io::Error::other)?;
    Ok(doc.settings)
}

fn write_settings(path: &Path, settings: &Settings) -> io::Result<()> {
    let mut raw = serde_json::to_vec_pretty(&Document {
        version: VERSION,
        settings: settings.clone(),
    })?;
    raw.push(b'\n');
    write_atomic(path, &raw)
}

pub(crate) fn read_bounded(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut raw = Vec::new();
    file.take(max + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > max {
        return Err(io::Error::other("file too large"));
    }
    Ok(raw)
}

pub(crate) fn ensure_dir(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure_dir(parent)?;
    match fs::DirBuilder::new().mode(0o750).create(path) {
        Ok(()) => File::open(parent)?.sync_all(),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Same-directory replacement: private temp file, file fsync, rename, dir fsync.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure_dir(dir)?;
    let temp = dir.join(format!(".device-core-{}.tmp", token()?));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        File::open(dir)?.sync_all()
    })();
    let _ = fs::remove_file(temp);
    result
}

fn sync_voice_flag(settings: &Path, enabled: bool) -> io::Result<()> {
    let dir = settings
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let flag = dir.join("voice-enabled");
    if enabled {
        return write_atomic(&flag, b"");
    }
    match fs::remove_file(flag) {
        Ok(()) => File::open(dir)?.sync_all(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub(crate) fn failed(error: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(error.to_string())
}
fn invalid(message: &str) -> fdo::Error {
    fdo::Error::InvalidArgs(message.into())
}
