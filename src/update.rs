//! One updater actor shared by D-Bus, HTTP and the automatic scheduler.
#[path = "update/catalog.rs"]
mod catalog;
#[path = "update/journal.rs"]
mod journal;
#[path = "update/manual.rs"]
mod manual;
#[path = "update/rauc.rs"]
mod rauc;
#[path = "update/schedule.rs"]
mod schedule;

use crate::{
    common::{token, Events, Gate},
    options::Options,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    future::Future,
    os::fd::AsFd,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex};
use zbus::{
    fdo,
    zvariant::{Fd, OwnedValue, Type, Value},
    Connection,
};

pub type HookFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
pub type Hook = Arc<dyn Fn(String) -> HookFuture + Send + Sync>;
#[derive(Clone)]
pub struct Hooks {
    pub acquire: Hook,
    pub release: Hook,
    pub reboot: Hook,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub auto_check_updates: bool,
    pub automatic: bool,
    pub channel: String,
    pub start_hour: u32,
    pub start_min: u32,
    pub end_hour: u32,
    pub end_min: u32,
    pub timezone: String,
    pub time_reliable: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_check_updates: true,
            automatic: false,
            channel: "stable".into(),
            start_hour: 3,
            start_min: 0,
            end_hour: 5,
            end_min: 0,
            timezone: "UTC".into(),
            time_reliable: false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, Type)]
pub struct Release {
    pub tag: String,
    pub bundle_url: String,
    pub size: u64,
    pub sums_url: String,
    pub notes: String,
    pub published: String,
    pub prerelease: bool,
    pub ready: bool,
    pub problem: String,
    pub blocked: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, Type, Value, OwnedValue)]
pub struct Status {
    pub current: String,
    pub state: String,
    pub progress: i32,
    pub error: String,
    pub checked: String,
    pub checking: bool,
    pub check_error: String,
    pub target: String,
    pub last_result: String,
    pub suspended: bool,
    pub retry_required: bool,
    pub pending_auto: bool,
    pub pending_channel: String,
    pub last_window: String,
    pub operation_id: String,
}

struct Data {
    status: Status,
    catalog: Vec<Release>,
    journal: journal::Journal,
    maintenance: Option<String>,
    rebooted: bool,
    recovered: bool,
    manual_cleanup: bool,
    helper_cleanup: bool,
    cleanup_owner: Option<String>,
}
struct Core {
    connection: Connection,
    options: Options,
    events: Events,
    gate: Gate,
    settings: watch::Receiver<Settings>,
    hooks: Hooks,
    http: reqwest::Client,
    data: Mutex<Data>,
    operation: Arc<AsyncMutex<()>>,
    checking: AsyncMutex<()>,
    unsupported: bool,
}
enum Command {
    Install {
        tag: String,
        channel: String,
        automatic: bool,
        retry: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    InstallBundle {
        file: File,
        ignore_certificate: bool,
        retry: bool,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Check(oneshot::Sender<Result<Vec<Release>, String>>),
    Reconcile(oneshot::Sender<Result<(), String>>),
    Power(Hook, oneshot::Sender<Result<(), String>>),
    Kick,
}

#[derive(Clone)]
pub struct Updater {
    core: Arc<Core>,
    commands: mpsc::Sender<Command>,
    kick: Arc<tokio::sync::Notify>,
}

impl Updater {
    pub async fn new(
        connection: Connection,
        options: &Options,
        events: Events,
        gate: Gate,
        settings: watch::Receiver<Settings>,
        hooks: Hooks,
    ) -> Result<Self, String> {
        let j = journal::Journal::load(&options.data_dir.join("updates"))
            .inspect_err(|_| gate.set(true))?;
        let configured = catalog::valid_repo(&options.update_repo)
            && catalog::valid_asset(&options.update_asset);
        if !options.update_prepare_unit.is_empty() {
            crate::system::validate_unit(&options.update_prepare_unit)
                .map_err(|e| e.to_string())?;
        }
        let unsupported = !configured
            && j.is_empty()
            && options.update_prepare_unit.is_empty()
            && !options.data_dir.join("updates/manual.raucb").exists()
            && matches!(rauc::available(&connection).await, Ok(false));
        // Optional absent updater configuration must not disable unrelated device services.
        // This constructor runs before exports: clear the parent's startup barrier if updates are absent.
        gate.set(!unsupported);
        let http = catalog::client(options)?;
        let core = Arc::new(Core {
            connection,
            options: options.clone(),
            events,
            gate,
            settings,
            hooks,
            http,
            data: Mutex::new(Data {
                status: Status {
                    current: options.image_version.clone(),
                    state: if unsupported {
                        "unsupported"
                    } else {
                        "uncertain"
                    }
                    .into(),
                    ..Status::default()
                },
                catalog: vec![],
                journal: j,
                maintenance: None,
                rebooted: false,
                recovered: unsupported,
                manual_cleanup: options.data_dir.join("updates/manual.raucb").exists(),
                helper_cleanup: !options.update_prepare_unit.is_empty(),
                cleanup_owner: None,
            }),
            operation: Arc::new(AsyncMutex::new(())),
            checking: AsyncMutex::new(()),
            unsupported,
        });
        let (commands, mut receive) = mpsc::channel(32);
        let updater = Self {
            core: core.clone(),
            commands,
            kick: Arc::new(tokio::sync::Notify::new()),
        };
        let actor = updater.clone();
        tokio::spawn(async move {
            while let Some(command) = receive.recv().await {
                match command {
                    Command::Install {
                        tag,
                        channel,
                        automatic,
                        retry,
                        reply,
                    } => {
                        let result = core
                            .validate(&tag, &channel, automatic, retry)
                            .and_then(|()| {
                                core.operation.clone().try_lock_owned().map_err(|_| {
                                    "an update operation is already in progress".into()
                                })
                            })
                            .and_then(|guard| {
                                token().map(|id| (guard, id)).map_err(|e| e.to_string())
                            });
                        match result {
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                            Ok((guard, id)) => {
                                core.set(|s| {
                                    s.operation_id = id.clone();
                                    s.error.clear();
                                    s.progress = 0;
                                    s.target = tag.clone();
                                });
                                let job = core.clone();
                                // Spawn before replying. A disappearing caller never cancels acceptance.
                                let operation_id = id.clone();
                                tokio::spawn(async move {
                                    let _guard = guard;
                                    if let Err(e) = job
                                        .install(&tag, &channel, automatic, retry, &operation_id)
                                        .await
                                    {
                                        job.set(|s| {
                                            s.error = e;
                                            if s.state == "downloading" {
                                                s.state = "error".into();
                                            }
                                        });
                                    }
                                    // Probe even after a preparation error: another RAUC may have started.
                                    if let Err(e) = job.recover().await {
                                        job.set(|s| s.error = e);
                                    }
                                });
                                let _ = reply.send(Ok(id));
                            }
                        }
                    }
                    Command::InstallBundle {
                        file,
                        ignore_certificate,
                        retry,
                        reply,
                    } => {
                        let result = core
                            .validate_manual(ignore_certificate)
                            .and_then(|()| {
                                core.operation.clone().try_lock_owned().map_err(|_| {
                                    "an update operation is already in progress".to_string()
                                })
                            })
                            .and_then(|guard| {
                                token().map(|id| (guard, id)).map_err(|e| e.to_string())
                            });
                        match result {
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                            Ok((guard, id)) => {
                                core.set(|s| {
                                    s.operation_id = id.clone();
                                    s.error.clear();
                                    s.progress = 0;
                                    s.target.clear();
                                });
                                let job = core.clone();
                                let operation_id = id.clone();
                                tokio::spawn(async move {
                                    let _guard = guard;
                                    if let Err(e) = job
                                        .install_manual(
                                            file,
                                            ignore_certificate,
                                            retry,
                                            &operation_id,
                                        )
                                        .await
                                    {
                                        job.set(|s| {
                                            s.error = e;
                                            if s.state == "downloading" {
                                                s.state = "error".into();
                                            }
                                        });
                                    }
                                    if let Err(e) = job.recover().await {
                                        job.set(|s| s.error = e);
                                    }
                                });
                                let _ = reply.send(Ok(id));
                            }
                        }
                    }
                    Command::Check(reply) => {
                        let job = core.clone();
                        tokio::spawn(async move {
                            let _ = reply.send(job.check().await);
                        });
                    }
                    Command::Reconcile(reply) => {
                        let job = core.clone();
                        tokio::spawn(async move {
                            let result = if let Ok(_guard) = job.operation.try_lock() {
                                job.recover().await
                            } else {
                                Ok(())
                            };
                            let _ = reply.send(result);
                        });
                    }
                    Command::Power(action, reply) => {
                        match core.operation.clone().try_lock_owned() {
                            Err(_) => {
                                let _ = reply
                                    .send(Err("an update operation is already in progress".into()));
                            }
                            Ok(guard) => {
                                let job = core.clone();
                                tokio::spawn(async move {
                                    let _guard = guard;
                                    let _ = reply.send(job.power(action).await);
                                });
                            }
                        }
                    }
                    Command::Kick => actor.kick.notify_one(),
                }
            }
        });
        let scheduler = updater.clone();
        tokio::spawn(async move {
            scheduler.schedule_loop().await;
        });
        // Return a handle even when RAUC is unavailable. Recovery keeps trying with the gate closed.
        if let Err(e) = updater.reconcile().await {
            updater.core.set(|s| s.error = e);
        }
        Ok(updater)
    }

    pub fn configured(&self) -> bool {
        self.core.configured()
    }
    pub fn status(&self) -> Status {
        self.core.status()
    }
    pub fn releases(&self, channel: &str) -> Vec<Release> {
        self.core.releases(channel)
    }
    pub async fn check(&self) -> Result<Vec<Release>, String> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::Check(send))
            .await
            .map_err(|e| e.to_string())?;
        receive.await.map_err(|e| e.to_string())?
    }
    pub async fn install(
        &self,
        tag: &str,
        channel: &str,
        automatic: bool,
        retry: bool,
    ) -> Result<String, String> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Install {
                tag: tag.into(),
                channel: channel.into(),
                automatic,
                retry,
                reply,
            })
            .await
            .map_err(|e| e.to_string())?;
        receive.await.map_err(|e| e.to_string())?
    }
    /// Duplicate and validate the received descriptor before handing it to the actor.
    pub async fn install_bundle(
        &self,
        fd: Fd<'_>,
        ignore_certificate: bool,
        retry: bool,
    ) -> Result<String, String> {
        let file = File::from(fd.as_fd().try_clone_to_owned().map_err(|e| e.to_string())?);
        manual::validate_file(&file)?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::InstallBundle {
                file,
                ignore_certificate,
                retry,
                reply,
            })
            .await
            .map_err(|e| e.to_string())?;
        receive.await.map_err(|e| e.to_string())?
    }
    pub async fn reconcile(&self) -> Result<(), String> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(Command::Reconcile(send))
            .await
            .map_err(|e| e.to_string())?;
        receive.await.map_err(|e| e.to_string())?
    }
    pub async fn kick(&self) -> Result<(), String> {
        self.commands
            .send(Command::Kick)
            .await
            .map_err(|e| e.to_string())
    }
    /// Serialize reboot/power-off with installs. Use the parent's raw native power hook.
    pub async fn power(&self, action: Hook) -> Result<(), String> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Power(action, reply))
            .await
            .map_err(|e| e.to_string())?;
        receive.await.map_err(|e| e.to_string())?
    }
}

#[zbus::interface(name = "io.github.guilhem.DeviceCore1.Updates")]
impl Updater {
    #[zbus(name = "Check")]
    async fn dbus_check(&self) -> fdo::Result<Vec<Release>> {
        self.check().await.map_err(fdo::Error::Failed)
    }
    #[zbus(name = "Releases")]
    fn dbus_releases(&self, channel: &str) -> Vec<Release> {
        self.releases(channel)
    }
    #[zbus(name = "Install")]
    async fn dbus_install(
        &self,
        tag: &str,
        channel: &str,
        automatic: bool,
        retry: bool,
    ) -> fdo::Result<String> {
        self.install(tag, channel, automatic, retry)
            .await
            .map_err(fdo::Error::Failed)
    }
    #[zbus(name = "InstallBundle")]
    async fn dbus_install_bundle(
        &self,
        fd: Fd<'_>,
        ignore_certificate: bool,
        retry: bool,
    ) -> fdo::Result<String> {
        self.install_bundle(fd, ignore_certificate, retry)
            .await
            .map_err(fdo::Error::Failed)
    }
    #[zbus(name = "Reconcile")]
    async fn dbus_reconcile(&self) -> fdo::Result<()> {
        self.reconcile().await.map_err(fdo::Error::Failed)
    }
    #[zbus(property, name = "Status")]
    fn dbus_status(&self) -> Status {
        self.status()
    }
}

impl Core {
    fn dir(&self) -> std::path::PathBuf {
        self.options.data_dir.join("updates")
    }
    fn configured(&self) -> bool {
        catalog::valid_repo(&self.options.update_repo)
            && catalog::valid_asset(&self.options.update_asset)
    }
    fn status(&self) -> Status {
        let d = self.data.lock().unwrap();
        let mut s = d.status.clone();
        let j = &d.journal;
        s.last_result = j.last_result.clone();
        s.last_window = j.last_window.clone();
        s.suspended = !j.suspended.is_empty() || catalog::version(&s.current).is_none();
        s.retry_required = !j.suspended.is_empty() || j.blocked.contains_key(&j.target);
        if s.target.is_empty() {
            s.target = j.target.clone();
        }
        s.pending_auto = false;
        s.pending_channel.clear();
        if let Some(p) = &j.pending {
            s.target = p.tag.clone();
            s.pending_auto = p.automatic;
            s.pending_channel = p.channel.clone();
            if s.operation_id.is_empty() {
                s.operation_id = p.operation_id.clone();
            }
        }
        s
    }
    fn publish(&self) {
        self.events.emit("updates", &self.status());
    }
    fn set(&self, change: impl FnOnce(&mut Status)) {
        change(&mut self.data.lock().unwrap().status);
        self.publish();
    }
    fn commit(&self, change: impl FnOnce(&mut journal::Journal)) -> Result<(), String> {
        let result = {
            let mut d = self.data.lock().unwrap();
            d.journal.commit(
                &self.dir().join("state.json"),
                change,
                journal::write_atomic,
            )
        };
        self.publish();
        result
    }
    fn releases(&self, channel: &str) -> Vec<Release> {
        let d = self.data.lock().unwrap();
        d.catalog
            .iter()
            .filter(|r| catalog::allowed(r, channel) && newer(&r.tag, &self.options.image_version))
            .map(|r| {
                let mut r = r.clone();
                r.blocked = d.journal.blocked.get(&r.tag).cloned().unwrap_or_default();
                r
            })
            .collect()
    }
    fn validate(
        &self,
        tag: &str,
        channel: &str,
        automatic: bool,
        retry: bool,
    ) -> Result<(), String> {
        if !self.configured() {
            return Err("updates are not configured on this image".into());
        }
        if catalog::version(tag).is_none() {
            return Err(format!("invalid release tag {tag:?}"));
        }
        if !newer(tag, &self.options.image_version) {
            return Err(format!(
                "{tag} is not newer than {}",
                self.options.image_version
            ));
        }
        if !matches!(channel, "stable" | "test" | "edge") {
            return Err("unknown update channel".into());
        }
        if automatic && retry {
            return Err("only a manual installation can be retried".into());
        }
        if automatic && catalog::version(&self.options.image_version).is_none() {
            return Err("automatic updates need a released image version".into());
        }
        Ok(())
    }
    async fn acquire(&self, id: &str) -> Result<(), String> {
        self.gate.set(true);
        let held = self.data.lock().unwrap().maintenance.clone();
        if let Some(other) = held {
            if other != id {
                return Err(format!("maintenance held by {other}"));
            }
        }
        // Remote agents may have accepted before a reply/timeout. Preserve ownership for rollback.
        self.data.lock().unwrap().maintenance = Some(id.into());
        (self.hooks.acquire)(id.into()).await?;
        Ok(())
    }
    async fn release_idle(&self, boot: &rauc::BootState) -> Result<(), String> {
        if boot.operation != "idle" {
            self.gate.set(true);
            return Ok(());
        }
        self.release_held().await
    }
    async fn release_held(&self) -> Result<(), String> {
        let held = self.data.lock().unwrap().maintenance.clone();
        if let Some(id) = held {
            (self.hooks.release)(id).await?;
            self.data.lock().unwrap().maintenance = None;
        }
        // Only the coordinator can reopen the global gate: it knows all reservations and rollbacks.
        Ok(())
    }
    async fn recover(&self) -> Result<(), String> {
        if self.unsupported {
            // No RAUC or journal exists, including after a failed optional-domain power request.
            return self.release_held().await;
        }
        let needs_reservation = {
            let d = self.data.lock().unwrap();
            !d.recovered && d.maintenance.is_none()
        };
        if needs_reservation {
            self.acquire("updates-recovery").await?;
        }
        let boot = match rauc::probe(&self.connection, &self.options).await {
            Ok(b) => b,
            Err(e) => {
                self.gate.set(true);
                self.data.lock().unwrap().recovered = false;
                self.set(|s| {
                    s.state = "uncertain".into();
                    s.error = e.clone();
                });
                return Err(e);
            }
        };
        self.reconcile_boot(&boot)?;
        if boot.operation != "idle" {
            self.gate.set(true);
            if self.data.lock().unwrap().maintenance.is_none() {
                self.acquire("updates-recovery").await?;
            }
            return Ok(());
        }
        self.cleanup_manual(&boot).await?;
        self.release_idle(&boot).await?;
        self.data.lock().unwrap().recovered = true;
        Ok(())
    }
    async fn power_ready(&self) -> Result<(), String> {
        if self.unsupported {
            return Ok(());
        }
        let boot = rauc::probe(&self.connection, &self.options)
            .await
            .inspect_err(|_| {
                self.gate.set(true);
                self.data.lock().unwrap().recovered = false;
            })?;
        self.reconcile_boot(&boot)?;
        if boot.operation != "idle" {
            self.gate.set(true);
            return Err("RAUC is busy: power request refused".into());
        }
        let d = self.data.lock().unwrap();
        // A successful explicit retry may retain the old suspension until the new boot is healthy.
        let installed = d
            .journal
            .pending
            .as_ref()
            .is_some_and(|p| p.phase == "installed" && p.boot_id == boot.boot_id);
        if !installed && (d.journal.pending.is_some() || !d.journal.suspended.is_empty()) {
            return Err("the installation result is uncertain: power request refused".into());
        }
        Ok(())
    }
    async fn power(&self, action: Hook) -> Result<(), String> {
        self.recover().await?;
        self.power_ready().await?;
        let id = format!("updates-power-{}", token().map_err(|e| e.to_string())?);
        let result = async {
            self.acquire(&id).await?;
            // The operation guard remains held across agents, the fresh probe and the native call.
            self.power_ready().await?;
            action(id).await
        }
        .await;
        let recovered = self.recover().await;
        result.and(recovered)
    }
    async fn admit(
        &self,
        tag: &str,
        automatic: bool,
        retry: bool,
    ) -> Result<rauc::BootState, String> {
        let boot = rauc::probe(&self.connection, &self.options)
            .await
            .inspect_err(|_| self.gate.set(true))?;
        self.reconcile_boot(&boot)?;
        let d = self.data.lock().unwrap();
        let j = &d.journal;
        if boot.boot_id.is_empty() || journal::other_slot(&boot.slot).is_empty() {
            return Err("unknown boot identity or A/B slot".into());
        }
        if boot.operation != "idle" {
            self.gate.set(true);
            return Err("RAUC is busy".into());
        }
        if boot.health != "good" && (automatic || boot.health != "stranded") {
            return Err("the running system is not confirmed healthy yet".into());
        }
        if let Some(p) = &j.pending {
            if p.phase == "installed" && p.boot_id == boot.boot_id {
                return Err(format!("{} is installed: restart to finish", p.tag));
            }
            if !retry {
                return Err("the installation is unresolved: retry explicitly".into());
            }
        }
        if !j.suspended.is_empty() && (automatic || !retry) {
            return Err("the previous result is uncertain: retry explicitly".into());
        }
        if j.blocked.contains_key(tag) && !retry {
            return Err(format!("{tag} is blocked: retry explicitly"));
        }
        Ok(boot)
    }
    async fn before(&self, channel: &str, automatic: bool, id: &str) -> Result<String, String> {
        let settings = self.settings.borrow().clone();
        if settings.channel != channel {
            return Err("channel changed: confirm installation again".into());
        }
        let window = if automatic {
            schedule::policy(&settings, channel, Utc::now())?
        } else {
            String::new()
        };
        self.acquire(id).await?;
        // Recheck after asynchronous agent acquisitions, which can outlast the window.
        let settings = self.settings.borrow().clone();
        if settings.channel != channel {
            return Err("channel changed: confirm installation again".into());
        }
        if automatic {
            return schedule::policy(&settings, channel, Utc::now());
        }
        Ok(window)
    }
    async fn install(
        &self,
        tag: &str,
        channel: &str,
        automatic: bool,
        retry: bool,
        id: &str,
    ) -> Result<(), String> {
        self.recover().await?;
        self.admit(tag, automatic, retry).await?;
        let window = self.before(channel, automatic, id).await?;
        if automatic && !self.claim_window(&window)? {
            return Err("an attempt already happened in this window".into());
        }
        self.set(|s| {
            s.state = "downloading".into();
            s.error.clear();
            s.progress = 0;
        });
        let release = self.fetch_release(tag).await?;
        if !catalog::allowed(&release, channel) {
            return Err("preview release outside selected channel".into());
        }
        if !release.ready {
            return Err(format!("release {tag}: {}", release.problem));
        }
        let sum = self.checksum(&release).await?;
        // Metadata fetches can outlive boot/RAUC admission. Do not clean bundles used by another RAUC.
        self.admit(tag, automatic, retry).await?;
        let path = self.download(&release, &sum).await?;
        let final_window = self.before(channel, automatic, id).await?;
        if automatic && final_window != window {
            return Err("automatic window changed during download".into());
        }
        let boot = self.admit(tag, automatic, retry).await?;
        self.install_prepared(tag, channel, automatic, false, id, &path, &sum, &boot)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn install_prepared(
        &self,
        tag: &str,
        channel: &str,
        automatic: bool,
        local: bool,
        id: &str,
        path: &std::path::Path,
        sum: &str,
        boot: &rauc::BootState,
    ) -> Result<(), String> {
        self.commit(|j| {
            j.pending = Some(journal::Pending {
                tag: tag.into(),
                sha256: sum.into(),
                from: self.options.image_version.clone(),
                from_slot: boot.slot.clone(),
                to_slot: journal::other_slot(&boot.slot).into(),
                channel: channel.into(),
                boot_id: boot.boot_id.clone(),
                phase: "installing".into(),
                automatic,
                local,
                operation_id: id.into(),
            });
            j.target = tag.into();
        })
        .map_err(|e| format!("cannot record the installation: {e}"))?;
        self.set(|s| {
            s.state = "installing".into();
            s.progress = 0;
        });
        let result = rauc::install(&self.connection, path, &boot.owner, |p| {
            self.set(|s| s.progress = p)
        })
        .await;
        match result {
            rauc::Outcome::Success => {
                self.commit(|j| {
                    if let Some(p) = &mut j.pending {
                        p.phase = "installed".into();
                    }
                    j.last_result = format!("{tag} installed, restart to finish");
                })
                .map_err(|e| format!("{tag} installed but not recorded: {e}"))?;
                self.set(|s| {
                    s.state = "reboot".into();
                    s.progress = 100;
                });
            }
            rauc::Outcome::Refused(e) => {
                // A known failure still does not prove that RAUC has stopped reading the bundle.
                let after = rauc::probe(&self.connection, &self.options).await?;
                if after.operation != "idle" || after.owner != boot.owner {
                    return Err(format!("RAUC result cannot be reconciled: {e}"));
                }
                self.commit(|j| {
                    j.pending = None;
                    j.blocked.insert(tag.into(), e.clone());
                    j.last_result = format!("RAUC refused {tag}: {e}");
                })?;
                self.set(|s| {
                    s.state = "error".into();
                    s.error = e.clone();
                });
                if !local {
                    let _ = tokio::fs::remove_file(path).await;
                }
                return Err(e);
            }
            rauc::Outcome::Unknown(e) => {
                return Err(e);
            }
        }
        let after = rauc::probe(&self.connection, &self.options).await?;
        if !local && after.operation == "idle" && after.owner == boot.owner {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    async fn check(&self) -> Result<Vec<Release>, String> {
        if !self.configured() {
            return Err("updates are not configured on this image".into());
        }
        let _guard = self
            .checking
            .try_lock()
            .map_err(|_| "a release check is already in progress".to_string())?;
        self.set(|s| s.checking = true);
        let result = tokio::time::timeout(Duration::from_secs(60), self.fetch_catalog())
            .await
            .map_err(|_| "release catalogue timed out".to_string())
            .and_then(|r| r);
        {
            let mut d = self.data.lock().unwrap();
            d.status.checking = false;
            match &result {
                Ok(releases) => {
                    d.catalog = releases.clone();
                    d.status.checked = Utc::now().to_rfc3339();
                    d.status.check_error.clear();
                }
                Err(e) => d.status.check_error = e.clone(),
            }
        }
        self.publish();
        result.map(|mut releases| {
            let d = self.data.lock().unwrap();
            for r in &mut releases {
                r.blocked = d.journal.blocked.get(&r.tag).cloned().unwrap_or_default();
            }
            releases
        })
    }
    fn checked_recently(&self, now: DateTime<Utc>) -> bool {
        let s = self.status();
        !s.checking
            && s.check_error.is_empty()
            && DateTime::parse_from_rfc3339(&s.checked)
                .map(|t| {
                    now.signed_duration_since(t).num_seconds() >= 0
                        && now.signed_duration_since(t).num_seconds() <= 86400
                })
                .unwrap_or(false)
    }
}

/// Strict release precedence; build metadata does not make a release newer.
pub fn newer(tag: &str, current: &str) -> bool {
    let Some(tag) = catalog::version(tag) else {
        return false;
    };
    catalog::version(current)
        .map(|current| tag.cmp_precedence(&current).is_gt())
        .unwrap_or(true)
}
