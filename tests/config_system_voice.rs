use device_core::{common, config, options, system, voice};

use common::{Events, Gate};
use config::{Settings, Store};
use futures_util::{SinkExt, StreamExt};
use options::Options;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use system::System;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::{sleep, timeout},
};
use zbus::{
    fdo,
    object_server::SignalEmitter,
    zvariant::{OwnedObjectPath, Type},
    Connection,
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("device-core-csv-{}", common::token().unwrap()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn config_schema_cas_restart_and_crash_recovery() {
    let temp = Temp::new();
    let path = temp.0.join("settings.json");
    let store = Store::new(&path, Events::default()).unwrap();
    let mut changes = store.subscribe();
    let (initial, defaults) = store.read().unwrap();
    assert_eq!(defaults, Settings::default());
    assert_eq!(defaults.volume, 100);
    assert_eq!(Settings::SIGNATURE.to_string(), "(ssub(bs(uu)(uu))b)");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(store.update(&initial, defaults.clone()).unwrap(), initial);
    assert!(!changes.has_changed().unwrap());
    let mut next = defaults.clone();
    next.volume = 42;
    next.voice_enabled = true;
    let revision = store.update(&initial, next.clone()).unwrap();
    assert_ne!(initial, revision);
    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow_and_update(), next);
    assert!(store.update(&initial, defaults.clone()).is_err());
    assert!(temp.0.join("voice-enabled").exists());
    assert_eq!(
        fs::metadata(temp.0.join("voice-enabled"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let document: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(document["version"], 1);
    assert_eq!(document["settings"]["volume"], 42);
    assert_eq!(
        fs::read_dir(&temp.0).unwrap().count(),
        2,
        "temporary files leaked"
    );
    // Recover a power loss between document commit and derived flag commit.
    fs::remove_file(temp.0.join("voice-enabled")).unwrap();
    let reopened = Store::new(&path, Events::default()).unwrap();
    assert!(temp.0.join("voice-enabled").exists());
    let (incarnation, persisted) = reopened.read().unwrap();
    assert_eq!(persisted, next);
    assert_ne!(incarnation, revision);
    assert!(reopened.update(&revision, next.clone()).is_err());
    // Returning to the same content must not resurrect an old revision (ABA).
    let back = reopened.update(&incarnation, defaults.clone()).unwrap();
    let again = reopened.update(&back, next).unwrap();
    assert_ne!(again, incarnation);
    assert!(reopened.update(&incarnation, defaults).is_err());
}

#[test]
fn config_concurrent_cas_and_validation_preserve_document() {
    let temp = Temp::new();
    let path = temp.0.join("settings.json");
    let store = Store::new(&path, Events::default()).unwrap();
    let (revision, settings) = store.read().unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = [21, 22]
        .into_iter()
        .map(|volume| {
            let store = store.clone();
            let revision = revision.clone();
            let mut settings = settings.clone();
            settings.volume = volume;
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.update(&revision, settings)
            })
        })
        .collect();
    barrier.wait();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().ok())
            .count(),
        1
    );
    let before = fs::read(&path).unwrap();
    let (revision, settings) = store.read().unwrap();
    for bad in 0..6 {
        let mut next = settings.clone();
        match bad {
            0 => next.volume = 101,
            1 => next.locale = "../../etc".into(),
            2 => next.timezone = "../bad".into(),
            3 => next.updates.start.hour = 24,
            4 => next.updates.end = next.updates.start,
            _ => {
                next.updates.automatic = true;
                next.auto_check_updates = false;
            }
        }
        assert!(store.update(&revision, next).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    let mut document: serde_json::Value = serde_json::from_slice(&before).unwrap();
    document["settings"]["application"] = serde_json::json!({"secret": true});
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(Store::new(&path, Events::default()).is_err());
    document["settings"]
        .as_object_mut()
        .unwrap()
        .remove("application");
    document["version"] = serde_json::json!(99);
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(Store::new(&path, Events::default()).is_err());
}

#[test]
fn config_failed_replace_never_advances_revision() {
    let temp = Temp::new();
    let path = temp.0.join("settings.json");
    let store = Store::new(&path, Events::default()).unwrap();
    let (revision, settings) = store.read().unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let mut next = settings.clone();
    next.volume = 33;
    assert!(store.update(&revision, next).is_err());
    assert_eq!(store.read().unwrap(), (revision, settings));
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 1);
}

#[derive(Default)]
struct MockState {
    ntp: AtomicBool,
    fail_time: AtomicBool,
    time: AtomicI64,
    fail_jobs: AtomicBool,
    delay_job: AtomicBool,
    subscribed: AtomicBool,
    sequence: AtomicU32,
    units: Mutex<std::collections::HashMap<String, bool>>,
    jobs: Mutex<Vec<String>>,
}
struct Manager(Arc<MockState>);
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.systemd1")]
enum ManagerError {
    AlreadySubscribed(String),
}
#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl Manager {
    fn subscribe(&self) -> Result<(), ManagerError> {
        if self.0.subscribed.swap(true, Ordering::SeqCst) {
            Err(ManagerError::AlreadySubscribed(
                "Client is already subscribed".into(),
            ))
        } else {
            Ok(())
        }
    }
    fn get_unit(&self, unit: &str) -> OwnedObjectPath {
        unit_path(unit)
    }
    async fn start_unit(
        &self,
        unit: &str,
        mode: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> OwnedObjectPath {
        assert_eq!(mode, "replace");
        self.job(unit, true, emitter).await
    }
    async fn stop_unit(
        &self,
        unit: &str,
        mode: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> OwnedObjectPath {
        assert_eq!(mode, "replace");
        self.job(unit, false, emitter).await
    }
    #[zbus(signal)]
    async fn job_removed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        job: OwnedObjectPath,
        unit: &str,
        result: &str,
    ) -> zbus::Result<()>;
}
impl Manager {
    async fn job(&self, unit: &str, start: bool, emitter: SignalEmitter<'_>) -> OwnedObjectPath {
        let id = self.0.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let path =
            OwnedObjectPath::try_from(format!("/org/freedesktop/systemd1/job/{id}")).unwrap();
        self.0
            .jobs
            .lock()
            .unwrap()
            .push(format!("{} {unit}", if start { "start" } else { "stop" }));
        let state = self.0.clone();
        let unit = unit.to_owned();
        let job = path.clone();
        let emitter = emitter.to_owned();
        let failed = state.fail_jobs.load(Ordering::SeqCst);
        let delayed = state.delay_job.load(Ordering::SeqCst);
        tokio::spawn(async move {
            sleep(Duration::from_millis(if delayed { 100 } else { 5 })).await;
            if !failed {
                if unit == "ntp.service" {
                    state.ntp.store(start, Ordering::SeqCst);
                }
                state.units.lock().unwrap().insert(unit.clone(), start);
            }
            Manager::job_removed(
                &emitter,
                id,
                job,
                &unit,
                if failed { "failed" } else { "done" },
            )
            .await
            .unwrap();
        });
        path
    }
}
struct Unit(Arc<MockState>, String);
#[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
impl Unit {
    #[zbus(property)]
    fn active_state(&self) -> String {
        let active = if self.1 == "ntp.service" {
            self.0.ntp.load(Ordering::SeqCst)
        } else {
            *self.0.units.lock().unwrap().get(&self.1).unwrap_or(&false)
        };
        if active { "active" } else { "inactive" }.into()
    }
}
struct Clock(Arc<MockState>);
#[zbus::interface(name = "org.freedesktop.timedate1")]
impl Clock {
    fn set_time(&self, value: i64, relative: bool, interactive: bool) -> fdo::Result<()> {
        assert!(!relative && !interactive);
        assert!(
            !self.0.ntp.load(Ordering::SeqCst),
            "clock called before stop job finished"
        );
        if self.0.fail_time.load(Ordering::SeqCst) {
            return Err(fdo::Error::Failed("fake clock failure".into()));
        }
        self.0.time.store(value, Ordering::SeqCst);
        Ok(())
    }
}
fn unit_path(unit: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!(
        "/org/freedesktop/systemd1/unit/{}",
        unit.replace(['.', '-'], "_")
    ))
    .unwrap()
}

struct Bus {
    daemon: tokio::process::Child,
    service: Connection,
    client: Connection,
    state: Arc<MockState>,
}
impl Bus {
    async fn new() -> Self {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(daemon.stdout.take().unwrap()).lines();
        let address = lines.next_line().await.unwrap().unwrap();
        let service = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let state = Arc::new(MockState::default());
        service
            .request_name("org.freedesktop.systemd1")
            .await
            .unwrap();
        service
            .request_name("org.freedesktop.timedate1")
            .await
            .unwrap();
        service
            .object_server()
            .at("/org/freedesktop/systemd1", Manager(state.clone()))
            .await
            .unwrap();
        for unit in ["ntp.service", "ssh.service", "voice.service"] {
            service
                .object_server()
                .at(unit_path(unit), Unit(state.clone(), unit.into()))
                .await
                .unwrap();
        }
        service
            .object_server()
            .at("/org/freedesktop/timedate1", Clock(state.clone()))
            .await
            .unwrap();
        Self {
            daemon,
            service,
            client,
            state,
        }
    }
    fn system(&self, temp: &Temp, simulate: bool) -> (System, Options) {
        let mut options = Options::from_env(simulate);
        options.data_dir = temp.0.clone();
        options.ntp_unit = "ntp.service".into();
        options.ssh_unit = "ssh.service".into();
        options.lva_unit = "voice.service".into();
        options.timesync_file = temp.0.join("synchronized");
        options.timesync_clock = temp.0.join("saved-clock");
        (
            System::new(self.client.clone(), options.clone(), Events::default()),
            options,
        )
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.start_kill();
    }
}

#[tokio::test]
async fn system_clock_awaits_jobs_restores_ntp_and_survives_cancellation() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, _) = bus.system(&temp, false);
    for (active, fail) in [(true, false), (true, true), (false, false)] {
        bus.state.ntp.store(active, Ordering::SeqCst);
        bus.state.fail_time.store(fail, Ordering::SeqCst);
        bus.state.jobs.lock().unwrap().clear();
        let result = system.set_time(1_790_000_000_000_000).await;
        assert_eq!(result.is_err(), fail);
        assert_eq!(bus.state.ntp.load(Ordering::SeqCst), active);
        assert_eq!(
            *bus.state.jobs.lock().unwrap(),
            if active {
                vec!["stop ntp.service", "start ntp.service"]
            } else {
                vec![]
            }
        );
        if !fail {
            assert_eq!(system.clock().unwrap().0, "manual");
        }
    }
    assert!(system.set_time(-1).await.is_err());
    bus.state.ntp.store(true, Ordering::SeqCst);
    bus.state.delay_job.store(true, Ordering::SeqCst);
    let worker = {
        let system = system.clone();
        tokio::spawn(async move { system.set_time(1_790_000_001_000_000).await })
    };
    sleep(Duration::from_millis(20)).await;
    worker.abort();
    sleep(Duration::from_millis(250)).await;
    assert!(
        bus.state.ntp.load(Ordering::SeqCst),
        "request cancellation lost NTP restoration"
    );
    assert_eq!(bus.state.time.load(Ordering::SeqCst), 1_790_000_001_000_000);
    assert_eq!(system.clock().unwrap().0, "manual");
    fs::write(temp.0.join("synchronized"), "").unwrap();
    assert_eq!(system.clock().unwrap().0, "ntp");
    fs::remove_file(temp.0.join("synchronized")).unwrap();
    fs::write(temp.0.join("clock-manual"), "a previous boot").unwrap();
    assert_eq!(system.clock().unwrap().0, "unknown");
    fs::write(temp.0.join("saved-clock"), "").unwrap();
    assert_eq!(system.clock().unwrap().0, "restored");
}

#[tokio::test]
async fn system_simulation_and_job_failure_are_safe() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (simulated, _) = bus.system(&temp, true);
    simulated.set_time(1_700_000_000_000_000).await.unwrap();
    simulated.reboot().await.unwrap();
    simulated.power_off().await.unwrap();
    assert_eq!(simulated.connectivity().await.unwrap(), "offline");
    assert_eq!(simulated.clock().unwrap().0, "manual");
    assert_eq!(simulated.clock().unwrap().1, 1_700_000_000);
    assert!(bus.state.jobs.lock().unwrap().is_empty());
    assert_eq!(bus.state.time.load(Ordering::SeqCst), 0);
    let (system, _) = bus.system(&temp, false);
    bus.state.fail_jobs.store(true, Ordering::SeqCst);
    assert!(system.unit_job("ssh.service", true).await.is_err());
    assert!(system
        .unit_job("../../invalid.service", true)
        .await
        .is_err());
}

#[tokio::test]
async fn system_ssh_real_parser_rejects_mixed_private_and_injection() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, _) = bus.system(&temp, false);
    let (public, private) = ssh_keypair(&temp).await;
    let initial_revision = system.get_ssh_keys().await.unwrap().0;
    let valid = format!("# keys\r\nrestrict {}\r\n", public.trim());
    let revision = system
        .set_ssh_keys(&initial_revision, &valid)
        .await
        .unwrap();
    assert_eq!(
        system.get_ssh_keys().await.unwrap(),
        (revision, valid.replace("\r\n", "\n"))
    );
    let before = system.get_ssh_keys().await.unwrap();
    for bad in [
        "broken".into(),
        private,
        format!("{public}broken\n"),
        "x".repeat(system::MAX_SSH_KEYS + 1),
        format!("{public}\0"),
    ] {
        assert!(system.set_ssh_keys(&before.0, &bad).await.is_err());
        assert_eq!(system.get_ssh_keys().await.unwrap(), before);
    }
    let marker = temp.0.join("injection");
    let comment = format!("{} $(touch {})\n", public.trim(), marker.display());
    let revision = system.set_ssh_keys(&before.0, &comment).await.unwrap();
    assert!(!marker.exists(), "key comment executed by a shell");
    assert_eq!(
        fs::metadata(temp.0.join("ssh/authorized_keys"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(temp.0.join("ssh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let revision = system.set_ssh_keys(&revision, "# none\n").await.unwrap();
    assert_eq!(
        system.get_ssh_keys().await.unwrap(),
        (revision, String::new())
    );
    assert!(!system.unit_active("ssh.service").await.unwrap());
}

async fn ssh_keypair(temp: &Temp) -> (String, String) {
    let path = temp.0.join("test-client");
    assert!(Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "device-core test",
            "-f"
        ])
        .arg(&path)
        .status()
        .await
        .unwrap()
        .success());
    (
        fs::read_to_string(path.with_extension("pub")).unwrap(),
        fs::read_to_string(&path).unwrap(),
    )
}

#[tokio::test]
async fn system_ssh_cas_concurrent_aba_and_restart() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, options) = bus.system(&temp, false);
    let initial = system.get_ssh_keys().await.unwrap();
    assert!(initial.1.is_empty());
    assert!(
        !temp.0.join("ssh").exists(),
        "read created persistent state"
    );
    assert_eq!(system.clone().get_ssh_keys().await.unwrap(), initial);
    let (public, _) = ssh_keypair(&temp).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut writes = Vec::new();
    for name in ["one", "two"] {
        let (system, barrier, expected) = (system.clone(), barrier.clone(), initial.0.clone());
        let keys = format!("# {name}\n{public}");
        writes.push(tokio::spawn(async move {
            barrier.wait().await;
            system.set_ssh_keys(&expected, &keys).await
        }));
    }
    barrier.wait().await;
    let mut revisions = Vec::new();
    for write in writes {
        if let Ok(revision) = write.await.unwrap() {
            revisions.push(revision);
        }
    }
    assert_eq!(revisions.len(), 1, "concurrent stale writer was accepted");
    let current = system.get_ssh_keys().await.unwrap();
    assert_eq!(current.0, revisions[0]);
    let path = temp.0.join("ssh/authorized_keys");
    assert_eq!(fs::read_to_string(&path).unwrap(), current.1);
    assert_eq!(bus.state.jobs.lock().unwrap().len(), 1);
    assert!(system.set_ssh_keys(&initial.0, "").await.is_err());
    assert_eq!(system.get_ssh_keys().await.unwrap(), current);
    assert_eq!(fs::read_to_string(&path).unwrap(), current.1);
    assert_eq!(
        bus.state.jobs.lock().unwrap().len(),
        1,
        "CAS conflict started a job"
    );
    let empty = system.set_ssh_keys(&current.0, "").await.unwrap();
    assert_ne!(empty, initial.0, "empty contents resurrected old revision");
    let restored = system.set_ssh_keys(&empty, &current.1).await.unwrap();
    assert_ne!(restored, current.0, "ABA resurrected old revision");
    assert!(system.set_ssh_keys(&current.0, "").await.is_err());
    let unchanged = system.set_ssh_keys(&restored, &current.1).await.unwrap();
    assert_ne!(unchanged, restored, "accepted write reused its token");
    let hash = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(current.1.as_bytes())
    );
    assert_eq!(unchanged.split_once(':').unwrap().1, hash);
    let restarted = System::new(bus.client.clone(), options, Events::default());
    let fresh = restarted.get_ssh_keys().await.unwrap();
    assert_eq!(fresh.1, current.1);
    assert_ne!(fresh.0, unchanged);
    assert!(restarted.set_ssh_keys(&unchanged, "").await.is_err());
    assert_eq!(restarted.get_ssh_keys().await.unwrap(), fresh);
}

#[tokio::test]
async fn system_ssh_serializes_reads_and_adopts_commit_on_job_failure() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, _) = bus.system(&temp, false);
    let (public, _) = ssh_keypair(&temp).await;
    let initial = system.get_ssh_keys().await.unwrap();
    bus.state.delay_job.store(true, Ordering::SeqCst);
    let writer = {
        let (system, revision, keys) = (system.clone(), initial.0.clone(), public.clone());
        tokio::spawn(async move { system.set_ssh_keys(&revision, &keys).await })
    };
    timeout(Duration::from_secs(1), async {
        while bus.state.jobs.lock().unwrap().is_empty() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_millis(20), system.get_ssh_keys())
            .await
            .is_err(),
        "GetSSHKeys observed a snapshot while its runtime job was pending"
    );
    writer.abort(); // The accepted background job must still complete.
    let current = system.get_ssh_keys().await.unwrap();
    assert_eq!(current.1, public);
    assert_ne!(current.0, initial.0);
    assert!(system.unit_active("ssh.service").await.unwrap());
    bus.state.fail_jobs.store(true, Ordering::SeqCst);
    assert!(system.set_ssh_keys(&current.0, "").await.is_err());
    let committed = system.get_ssh_keys().await.unwrap();
    assert!(committed.1.is_empty());
    assert_ne!(committed.0, current.0);
    assert_eq!(fs::read(temp.0.join("ssh/authorized_keys")).unwrap(), b"");
    assert!(system.set_ssh_keys(&current.0, &public).await.is_err());
    assert_eq!(system.get_ssh_keys().await.unwrap(), committed);
    // Before-rename failure must keep the prior revision and cached contents.
    let path = temp.0.join("ssh/authorized_keys");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(system.set_ssh_keys(&committed.0, &public).await.is_err());
    assert_eq!(system.get_ssh_keys().await.unwrap(), committed);
}

#[tokio::test]
async fn system_ssh_directory_fsync_failure_keeps_visible_revision() {
    const CHILD: &str = "DEVICE_CORE_SSH_FSYNC_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // A real libc fault in an isolated subprocess: no production injection
        // knob and no changed persistence helper outside this worker's ownership.
        let temp = Temp::new();
        let source = temp.0.join("fsync.c");
        let library = temp.0.join("fsync.so");
        fs::write(
            &source,
            r#"
#include <errno.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
int fsync(int fd) {
    struct stat st;
    const char *flag = getenv("DEVICE_CORE_TEST_FAIL_DIR_FSYNC");
    if (flag && access(flag, F_OK) == 0 && fstat(fd, &st) == 0 && S_ISDIR(st.st_mode)) {
        errno = EIO;
        return -1;
    }
    return syscall(SYS_fsync, fd);
}
"#,
        )
        .unwrap();
        assert!(Command::new("cc")
            .args(["-shared", "-fPIC", "-o"])
            .arg(&library)
            .arg(&source)
            .status()
            .await
            .unwrap()
            .success());
        assert!(Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "system_ssh_directory_fsync_failure_keeps_visible_revision",
                "--nocapture"
            ])
            .env(CHILD, "1")
            .env("LD_PRELOAD", &library)
            .status()
            .await
            .unwrap()
            .success());
        return;
    }
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, _) = bus.system(&temp, true);
    let (public, _) = ssh_keypair(&temp).await;
    let path = temp.0.join("ssh/authorized_keys");
    fs::create_dir(path.parent().unwrap()).unwrap();
    fs::write(&path, &public).unwrap();
    let initial = system.get_ssh_keys().await.unwrap();
    let marker = temp.0.join("fail-dir-fsync");
    std::env::set_var("DEVICE_CORE_TEST_FAIL_DIR_FSYNC", &marker);
    fs::write(&marker, "").unwrap();
    let error = system.set_ssh_keys(&initial.0, "").await.unwrap_err();
    assert!(
        error.to_string().contains("Input/output error"),
        "wrong failure: {error}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"", "rename did not commit");
    let committed = system.get_ssh_keys().await.unwrap();
    assert!(committed.1.is_empty());
    assert_ne!(
        committed.0, initial.0,
        "old revision survived visible commit"
    );
    assert!(system.set_ssh_keys(&initial.0, &public).await.is_err());
    assert_eq!(system.get_ssh_keys().await.unwrap(), committed);
    fs::remove_file(marker).unwrap();
    let next = system.set_ssh_keys(&committed.0, &public).await.unwrap();
    assert_eq!(system.get_ssh_keys().await.unwrap(), (next, public));
}

async fn wait_status(changes: &mut tokio::sync::watch::Receiver<String>, status: &str) {
    timeout(Duration::from_secs(3), async {
        loop {
            if changes.borrow_and_update().as_str() == status {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn voice_websocket_persistence_gate_events_bounds_disconnect() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, mut options) = bus.system(&temp, false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    options.lva_url = format!("ws://{}", listener.local_addr().unwrap());
    let store = Store::new(temp.0.join("settings.json"), Events::default()).unwrap();
    let gate = Gate::default();
    let events = Events::default();
    let mut event_receiver = events.0.subscribe();
    let voice = voice::Voice::new(options, store.clone(), system, events, gate.clone());
    let mut state = voice.subscribe();
    assert_eq!(voice.status(), "disabled");
    assert!(voice.command("start_listening").await.is_err());
    voice.enable(true).await.unwrap();
    assert!(store.read().unwrap().1.voice_enabled);
    assert_eq!(
        voice.status(),
        "disabled",
        "Enable applied runtime before parent watch"
    );
    voice.enable_runtime(true).await.unwrap();
    let (tcp, _) = timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut upstream = tokio_tungstenite::accept_async(tcp).await.unwrap();
    wait_status(&mut state, "idle").await;
    upstream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            "{\"event\":\"listening\"}".into(),
        ))
        .await
        .unwrap();
    wait_status(&mut state, "listening").await;
    assert!(voice.command("bad\ncommand").await.is_err());
    voice.command("start_listening").await.unwrap();
    let raw = timeout(Duration::from_secs(1), upstream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(raw.to_text().unwrap()).unwrap(),
        serde_json::json!({"command":"start_listening"})
    );
    gate.set(true);
    assert!(voice.command("stop_pipeline").await.is_err());
    assert!(voice.enable_runtime(true).await.is_err());
    assert!(voice.enable(true).await.is_err());
    // Runtime maintenance stop leaves persistent desired state untouched.
    voice.enable_runtime(false).await.unwrap();
    assert_eq!(voice.status(), "disabled");
    assert!(store.read().unwrap().1.voice_enabled);
    gate.set(false);
    voice.enable_runtime(true).await.unwrap();
    let (tcp, _) = timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut upstream = tokio_tungstenite::accept_async(tcp).await.unwrap();
    wait_status(&mut state, "idle").await;
    // A frame beyond 64 KiB closes the transport; nothing is replayed later.
    let _ = upstream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            "x".repeat(65537).into(),
        ))
        .await;
    wait_status(&mut state, "disconnected").await;
    assert!(
        timeout(Duration::from_secs(1), voice.command("start_listening"))
            .await
            .unwrap()
            .is_err()
    );
    let mut saw_disconnected = false;
    while let Ok(event) = event_receiver.try_recv() {
        if event.data["event"] == "disconnected" {
            saw_disconnected = true;
        }
    }
    assert!(saw_disconnected);
    voice.enable(false).await.unwrap();
    voice.enable_runtime(false).await.unwrap();
    assert!(!temp.0.join("voice-enabled").exists());
}

#[tokio::test]
async fn config_system_voice_dbus_wire_contract() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, options) = bus.system(&temp, true);
    let config = Store::new(temp.0.join("settings.json"), Events::default()).unwrap();
    let voice = voice::Voice::new(
        options,
        config.clone(),
        system.clone(),
        Events::default(),
        Gate::default(),
    );
    bus.service.request_name(common::SERVICE).await.unwrap();
    let root = common::ROOT;
    bus.service
        .object_server()
        .at(format!("{root}/Config"), config)
        .await
        .unwrap();
    bus.service
        .object_server()
        .at(format!("{root}/System"), system)
        .await
        .unwrap();
    bus.service
        .object_server()
        .at(format!("{root}/Voice"), voice)
        .await
        .unwrap();
    let proxy = zbus::Proxy::new(
        &bus.client,
        common::SERVICE,
        format!("{root}/Config"),
        "io.github.guilhem.DeviceCore1.Config",
    )
    .await
    .unwrap();
    let (revision, mut settings): (String, Settings) = proxy.call("Read", &()).await.unwrap();
    settings.volume = 84;
    let next: String = proxy
        .call("Update", &(revision.clone(), settings))
        .await
        .unwrap();
    assert_ne!(revision, next);
    let proxy = zbus::Proxy::new(
        &bus.client,
        common::SERVICE,
        format!("{root}/System"),
        "io.github.guilhem.DeviceCore1.System",
    )
    .await
    .unwrap();
    proxy
        .call::<_, _, ()>("SetTime", &(1_700_000_000_000_000i64,))
        .await
        .unwrap();
    let (quality, now): (String, i64) = proxy.call("Clock", &()).await.unwrap();
    assert_eq!((quality.as_str(), now), ("manual", 1_700_000_000));
    let (ssh_revision, keys): (String, String) = proxy.call("GetSSHKeys", &()).await.unwrap();
    assert!(keys.is_empty());
    let next: String = proxy
        .call("SetSSHKeys", &(ssh_revision.clone(), ""))
        .await
        .unwrap();
    assert_ne!(next, ssh_revision);
    assert!(proxy
        .call::<_, _, String>("SetSSHKeys", &(ssh_revision, ""))
        .await
        .is_err());
    let snapshot: (String, String) = proxy.call("GetSSHKeys", &()).await.unwrap();
    assert_eq!(snapshot, (next, String::new()));
    let proxy = zbus::Proxy::new(
        &bus.client,
        common::SERVICE,
        format!("{root}/Voice"),
        "io.github.guilhem.DeviceCore1.Voice",
    )
    .await
    .unwrap();
    assert!(proxy.get_property::<bool>("Supported").await.unwrap());
    assert_eq!(
        proxy.get_property::<String>("Status").await.unwrap(),
        "disabled"
    );
}

#[tokio::test]
async fn voice_gate_closing_during_start_job_stops_runtime() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, options) = bus.system(&temp, false);
    bus.state.delay_job.store(true, Ordering::SeqCst);
    let store = Store::new(temp.0.join("settings.json"), Events::default()).unwrap();
    let gate = Gate::default();
    let voice = voice::Voice::new(
        options,
        store,
        system.clone(),
        Events::default(),
        gate.clone(),
    );
    let activation = {
        let voice = voice.clone();
        tokio::spawn(async move { voice.enable_runtime(true).await })
    };
    timeout(Duration::from_secs(1), async {
        while bus.state.jobs.lock().unwrap().is_empty() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    gate.set(true);
    voice.enable_runtime(false).await.unwrap();
    assert!(activation.await.unwrap().is_err());
    assert_eq!(voice.status(), "disabled");
    assert!(!system.unit_active("voice.service").await.unwrap());
    assert!(voice.command("start_listening").await.is_err());
}

#[tokio::test]
async fn voice_handshake_has_a_deadline_and_commands_are_not_buffered() {
    let bus = Bus::new().await;
    let temp = Temp::new();
    let (system, mut options) = bus.system(&temp, false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    options.lva_url = format!("ws://{}", listener.local_addr().unwrap());
    let store = Store::new(temp.0.join("settings.json"), Events::default()).unwrap();
    let voice = voice::Voice::new(options, store, system, Events::default(), Gate::default());
    let mut changes = voice.subscribe();
    voice.enable_runtime(true).await.unwrap();
    let (_tcp, _) = timeout(Duration::from_secs(1), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert!(
        timeout(Duration::from_secs(1), voice.command("start_listening"))
            .await
            .unwrap()
            .is_err()
    );
    timeout(Duration::from_secs(6), async {
        loop {
            if changes.borrow_and_update().as_str() == "disconnected" {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    timeout(Duration::from_secs(1), voice.enable_runtime(false))
        .await
        .unwrap()
        .unwrap();
}
