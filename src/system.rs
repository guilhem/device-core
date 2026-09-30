//! Bounded OS primitives on the parent's connection. Never changes enablement.
use crate::{
    common::{token, Events},
    config::{ensure_dir, failed, read_bounded, write_atomic},
    options::Options,
    update::Updater,
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, process::Command, sync::Mutex, time::timeout};
use zbus::{fdo, zvariant::OwnedObjectPath, Connection, Proxy};

const DBUS_LIMIT: Duration = Duration::from_secs(15);
pub const MAX_SSH_KEYS: usize = 64 * 1024;
const KEY_LIMIT: Duration = Duration::from_secs(10);
const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MANAGER: &str = "org.freedesktop.systemd1.Manager";

#[derive(Default)]
struct Simulation {
    time: Option<(i64, std::time::Instant)>,
    units: std::collections::HashMap<String, bool>,
}

#[derive(Clone)]
pub struct System {
    connection: Connection,
    options: Arc<Options>,
    events: Events,
    clock_lock: Arc<Mutex<()>>,
    ssh_lock: Arc<Mutex<Option<(String, String)>>>,
    simulation: Arc<StdMutex<Simulation>>,
    power_policy: Arc<OnceLock<Updater>>,
}

impl System {
    pub fn new(connection: Connection, options: Options, events: Events) -> Self {
        Self {
            connection,
            options: Arc::new(options),
            events,
            clock_lock: Arc::new(Mutex::new(())),
            ssh_lock: Arc::new(Mutex::new(None)),
            simulation: Arc::new(StdMutex::new(Simulation::default())),
            power_policy: Arc::default(),
        }
    }

    pub async fn reboot(&self) -> fdo::Result<()> {
        self.power("Reboot").await
    }
    pub async fn power_off(&self) -> fdo::Result<()> {
        self.power("PowerOff").await
    }

    pub fn attach_updater(&self, updater: Updater) -> fdo::Result<()> {
        self.power_policy
            .set(updater)
            .map_err(|_| failed("power policy already attached"))
    }

    /// Both transports serialize power requests with update acceptance and recovery.
    pub async fn request_power(&self, off: bool) -> fdo::Result<()> {
        let updater = self
            .power_policy
            .get()
            .ok_or_else(|| failed("power policy not ready"))?;
        let system = self.clone();
        updater
            .power(Arc::new(move |_operation| {
                let system = system.clone();
                Box::pin(async move {
                    if off {
                        system.power_off().await
                    } else {
                        system.reboot().await
                    }
                    .map_err(|e| e.to_string())
                })
            }))
            .await
            .map_err(failed)
    }

    async fn power(&self, method: &str) -> fdo::Result<()> {
        if !self.options.simulate {
            timeout(DBUS_LIMIT, async {
                let proxy = Proxy::new(
                    &self.connection,
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1",
                    "org.freedesktop.login1.Manager",
                )
                .await?;
                proxy.call::<_, _, ()>(method, &(false,)).await
            })
            .await
            .map_err(failed)?
            .map_err(failed)?;
        }
        self.events.emit(
            "system",
            &serde_json::json!({"operation": method, "simulated": self.options.simulate}),
        );
        Ok(())
    }

    pub async fn unit_active(&self, unit: &str) -> fdo::Result<bool> {
        validate_unit(unit)?;
        if self.options.simulate {
            return Ok(*self
                .simulation
                .lock()
                .map_err(failed)?
                .units
                .get(unit)
                .unwrap_or(&false));
        }
        timeout(DBUS_LIMIT, async {
            let manager = Proxy::new(&self.connection, SYSTEMD, SYSTEMD_PATH, SYSTEMD_MANAGER)
                .await
                .map_err(failed)?;
            let path: OwnedObjectPath = match manager.call("GetUnit", &(unit,)).await {
                Ok(path) => path,
                Err(zbus::Error::MethodError(name, _, _))
                    if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
                {
                    return Ok(false)
                }
                Err(e) => return Err(failed(e)),
            };
            let proxy = Proxy::new(
                &self.connection,
                SYSTEMD,
                path,
                "org.freedesktop.systemd1.Unit",
            )
            .await
            .map_err(failed)?;
            let active: String = proxy.get_property("ActiveState").await.map_err(failed)?;
            Ok(matches!(
                active.as_str(),
                "active" | "reloading" | "activating"
            ))
        })
        .await
        .map_err(failed)?
    }

    /// Wait for systemd's actual JobRemoved result, with the match installed first.
    pub async fn unit_job(&self, unit: &str, start: bool) -> fdo::Result<()> {
        validate_unit(unit)?;
        if self.options.simulate {
            self.simulation
                .lock()
                .map_err(failed)?
                .units
                .insert(unit.into(), start);
            return Ok(());
        }
        timeout(DBUS_LIMIT, async {
            let proxy = Proxy::new(&self.connection, SYSTEMD, SYSTEMD_PATH, SYSTEMD_MANAGER)
                .await
                .map_err(failed)?;
            let mut removed = proxy.receive_signal("JobRemoved").await.map_err(failed)?;
            match proxy.call::<_, _, ()>("Subscribe", &()).await {
                Ok(()) => {}
                Err(zbus::Error::MethodError(name, _, _))
                    if name.as_str() == "org.freedesktop.systemd1.AlreadySubscribed" => {}
                Err(e) => return Err(failed(e)),
            }
            let job: OwnedObjectPath = proxy
                .call(
                    if start { "StartUnit" } else { "StopUnit" },
                    &(unit, "replace"),
                )
                .await
                .map_err(failed)?;
            while let Some(signal) = removed.next().await {
                let (_, path, name, result): (u32, OwnedObjectPath, String, String) =
                    signal.body().deserialize().map_err(failed)?;
                if path == job && name == unit {
                    return if result == "done" {
                        Ok(())
                    } else {
                        Err(failed(format!("{unit} job: {result}")))
                    };
                }
            }
            Err(failed("systemd disconnected while awaiting job"))
        })
        .await
        .map_err(|_| failed(format!("{unit} job timed out")))?
    }

    pub async fn set_time(&self, unix_microseconds: i64) -> fdo::Result<()> {
        // Accepted work outlives an HTTP disconnect so NTP restoration runs.
        let system = self.clone();
        tokio::spawn(async move { system.set_time_inner(unix_microseconds).await })
            .await
            .map_err(failed)?
    }

    async fn set_time_inner(&self, unix_microseconds: i64) -> fdo::Result<()> {
        // Reject nonsense before touching NTP; supported UTC years are 1970–9999.
        if !(0..=253_402_300_799_999_999).contains(&unix_microseconds) {
            return Err(fdo::Error::InvalidArgs(
                "time outside supported range".into(),
            ));
        }
        let _guard = timeout(DBUS_LIMIT, self.clock_lock.lock())
            .await
            .map_err(failed)?;
        if self.options.simulate {
            self.simulation.lock().map_err(failed)?.time =
                Some((unix_microseconds, std::time::Instant::now()));
        } else {
            let ntp = &self.options.ntp_unit;
            let active = !ntp.is_empty() && self.unit_active(ntp).await?;
            let operation = async {
                if active {
                    self.unit_job(ntp, false).await?;
                }
                timeout(DBUS_LIMIT, async {
                    let proxy = Proxy::new(
                        &self.connection,
                        "org.freedesktop.timedate1",
                        "/org/freedesktop/timedate1",
                        "org.freedesktop.timedate1",
                    )
                    .await?;
                    proxy
                        .call::<_, _, ()>("SetTime", &(unix_microseconds, false, false))
                        .await
                })
                .await
                .map_err(failed)?
                .map_err(failed)
            }
            .await;
            // Independent deadline: even SetTime failure/timeout must resume NTP.
            let restored = if active {
                self.unit_job(ntp, true).await
            } else {
                Ok(())
            };
            match (operation, restored) {
                (Err(e), Err(r)) => return Err(failed(format!("{e}; resume NTP: {r}"))),
                (Err(e), _) => return Err(e),
                (_, Err(e)) => return Err(failed(format!("resume NTP: {e}"))),
                _ => {}
            }
            let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(failed)?;
            if boot.trim().is_empty() {
                return Err(failed("missing boot identity"));
            }
            write_atomic(&self.options.data_dir.join("clock-manual"), boot.as_bytes())
                .map_err(failed)?;
        }
        self.events.emit(
            "system",
            &serde_json::json!({"operation": "SetTime", "unix_microseconds": unix_microseconds}),
        );
        Ok(())
    }

    pub fn clock(&self) -> fdo::Result<(String, i64)> {
        if self.options.simulate {
            let state = self.simulation.lock().map_err(failed)?;
            return Ok(match state.time {
                Some((value, started)) => (
                    "manual".into(),
                    value / 1_000_000 + started.elapsed().as_secs() as i64,
                ),
                None => ("unknown".into(), unix_seconds()),
            });
        }
        let now = unix_seconds();
        let options = &self.options;
        let quality = if options.timesync_file.as_os_str().is_empty()
            || options.timesync_file == std::path::Path::new("none")
            || options.timesync_file.exists()
        {
            "ntp"
        } else if read_bounded(&options.data_dir.join("clock-manual"), 128)
            .ok()
            .and_then(|raw| String::from_utf8(raw).ok())
            .zip(fs::read_to_string("/proc/sys/kernel/random/boot_id").ok())
            .is_some_and(|(manual, boot)| !boot.trim().is_empty() && manual.trim() == boot.trim())
        {
            "manual"
        } else if fs::metadata(&options.timesync_clock)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .is_some_and(|time| now >= time.as_secs() as i64 - 60)
        {
            "restored"
        } else {
            "unknown"
        };
        Ok((quality.into(), now))
    }

    pub async fn connectivity(&self) -> fdo::Result<String> {
        if self.options.simulate {
            return Ok("offline".into());
        }
        if !has_address().map_err(failed)? {
            return Ok("offline".into());
        }
        let reachable = timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(&self.options.net_probe),
        )
        .await;
        Ok(if matches!(reachable, Ok(Ok(_))) {
            "ok"
        } else {
            "lan"
        }
        .into())
    }

    pub async fn get_ssh_keys(&self) -> fdo::Result<(String, String)> {
        let mut state = timeout(DBUS_LIMIT, self.ssh_lock.lock())
            .await
            .map_err(failed)?;
        self.ssh_snapshot(&mut state)
    }

    fn ssh_snapshot(&self, state: &mut Option<(String, String)>) -> fdo::Result<(String, String)> {
        if state.is_none() {
            let keys = self.read_ssh_file()?;
            *state = Some((ssh_revision(&keys)?, keys));
        }
        Ok(state.as_ref().expect("SSH snapshot initialized").clone())
    }

    fn read_ssh_file(&self) -> fdo::Result<String> {
        match read_bounded(
            &self.options.data_dir.join("ssh/authorized_keys"),
            MAX_SSH_KEYS as u64,
        ) {
            Ok(raw) => String::from_utf8(raw).map_err(failed),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(failed(e)),
        }
    }

    pub async fn set_ssh_keys(&self, expected_revision: &str, keys: &str) -> fdo::Result<String> {
        let keys = validate_ssh_keys(keys).await?;
        let expected_revision = expected_revision.to_owned();
        let system = self.clone();
        tokio::spawn(async move { system.set_ssh_keys_inner(expected_revision, keys).await })
            .await
            .map_err(failed)?
    }

    async fn set_ssh_keys_inner(
        &self,
        expected_revision: String,
        keys: String,
    ) -> fdo::Result<String> {
        let mut state = timeout(DBUS_LIMIT, self.ssh_lock.lock())
            .await
            .map_err(failed)?;
        if self.ssh_snapshot(&mut state)?.0 != expected_revision {
            return Err(failed("SSH revision conflict; read current keys"));
        }
        // Allocate before touching disk; reusing a content hash alone permits ABA.
        let revision = ssh_revision(&keys)?;
        let path = self.options.data_dir.join("ssh/authorized_keys");
        ensure_dir(&self.options.data_dir).map_err(failed)?;
        ensure_dir(path.parent().unwrap()).map_err(failed)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700))
            .map_err(failed)?;
        let write = write_atomic(&path, keys.as_bytes());
        // Rename can succeed before directory fsync fails. Invalidate the old
        // token whenever the proposed contents are visible, even on error.
        if write.is_ok() || self.read_ssh_file().ok().as_ref() == Some(&keys) {
            *state = Some((revision.clone(), keys.clone()));
        }
        write.map_err(failed)?;
        self.unit_job(&self.options.ssh_unit, !keys.is_empty())
            .await?;
        if !keys.is_empty() && !self.unit_active(&self.options.ssh_unit).await? {
            return Err(failed("keys saved but SSH did not become active"));
        }
        self.events.emit(
            "system",
            &serde_json::json!({"operation": "SetSSHKeys", "enabled": !keys.is_empty(), "revision": revision}),
        );
        Ok(revision)
    }
}

#[zbus::interface(name = "io.github.guilhem.DeviceCore1.System")]
impl System {
    #[zbus(name = "Reboot")]
    async fn dbus_reboot(&self) -> fdo::Result<()> {
        self.request_power(false).await
    }
    #[zbus(name = "PowerOff")]
    async fn dbus_power_off(&self) -> fdo::Result<()> {
        self.request_power(true).await
    }
    #[zbus(name = "SetTime")]
    async fn dbus_set_time(&self, unix_microseconds: i64) -> fdo::Result<()> {
        self.set_time(unix_microseconds).await
    }
    #[zbus(name = "GetSSHKeys")]
    async fn dbus_get_ssh_keys(&self) -> fdo::Result<(String, String)> {
        self.get_ssh_keys().await
    }
    #[zbus(name = "SetSSHKeys")]
    async fn dbus_set_ssh_keys(&self, expected_revision: &str, keys: &str) -> fdo::Result<String> {
        self.set_ssh_keys(expected_revision, keys).await
    }
    #[zbus(name = "Clock")]
    fn dbus_clock(&self) -> fdo::Result<(String, i64)> {
        self.clock()
    }
    #[zbus(name = "Connectivity")]
    async fn dbus_connectivity(&self) -> fdo::Result<String> {
        self.connectivity().await
    }
}

fn ssh_revision(keys: &str) -> fdo::Result<String> {
    Ok(format!(
        "{}:{:x}",
        token().map_err(failed)?,
        Sha256::digest(keys.as_bytes())
    ))
}

pub async fn validate_ssh_keys(raw: &str) -> fdo::Result<String> {
    if raw.len() > MAX_SSH_KEYS || raw.contains('\0') {
        return Err(fdo::Error::InvalidArgs(
            "SSH keys exceed 64 KiB or contain NUL".into(),
        ));
    }
    timeout(KEY_LIMIT, async {
        let raw = raw.replace("\r\n", "\n");
        let raw = raw.trim();
        let mut count = 0;
        for (number, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            count += 1;
            if count > 256 || line.len() > 8192 || line.bytes().any(|b| b < 32 && b != b'\t') {
                return Err(fdo::Error::InvalidArgs(
                    "invalid SSH line or too many keys".into(),
                ));
            }
            // No shell, options or filename derived from the input; validate each
            // line because ssh-keygen ignores broken lines in multi-key input.
            let mut child = Command::new("ssh-keygen")
                .args(["-l", "-f", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(failed)?;
            let mut input = child
                .stdin
                .take()
                .ok_or_else(|| failed("ssh-keygen stdin unavailable"))?;
            input
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(failed)?;
            drop(input);
            if !child.wait().await.map_err(failed)?.success() {
                return Err(fdo::Error::InvalidArgs(format!(
                    "invalid public SSH key on line {}",
                    number + 1
                )));
            }
        }
        Ok(if count == 0 {
            String::new()
        } else {
            format!("{raw}\n")
        })
    })
    .await
    .map_err(|_| failed("SSH key validation timed out"))?
}

fn validate_unit(unit: &str) -> fdo::Result<()> {
    if unit.is_empty()
        || unit.len() > 255
        || !unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.@:\\".contains(&b))
        || !unit.ends_with(".service")
    {
        return Err(fdo::Error::InvalidArgs("invalid service unit".into()));
    }
    Ok(())
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn has_address() -> io::Result<bool> {
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut next = head;
        while !next.is_null() {
            let entry = unsafe { &*next };
            next = entry.ifa_next;
            if entry.ifa_addr.is_null()
                || entry.ifa_flags & libc::IFF_UP as u32 == 0
                || entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0
            {
                continue;
            }
            let family = unsafe { (*entry.ifa_addr).sa_family as i32 };
            let ip = match family {
                libc::AF_INET => {
                    let addr = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
                    IpAddr::V4(Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()))
                }
                libc::AF_INET6 => {
                    let addr = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in6) };
                    IpAddr::V6(Ipv6Addr::from(addr.sin6_addr.s6_addr))
                }
                _ => continue,
            };
            let usable = match ip {
                IpAddr::V4(ip) => {
                    !ip.is_unspecified()
                        && !ip.is_loopback()
                        && !ip.is_link_local()
                        && !ip.is_multicast()
                        && ip != Ipv4Addr::BROADCAST
                }
                IpAddr::V6(ip) => {
                    !ip.is_unspecified()
                        && !ip.is_loopback()
                        && !ip.is_unicast_link_local()
                        && !ip.is_multicast()
                }
            };
            if usable {
                return true;
            }
        }
        false
    })();
    unsafe { libc::freeifaddrs(head) };
    Ok(result)
}
