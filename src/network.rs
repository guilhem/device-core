//! Autonomous Wi-Fi control; no MQTT, HTTP, settings file or secret output.
//! NetworkManager owns all persistent state. The controller serializes radio
//! operations while D-Bus getters and physical presence stay responsive.

mod guard;
mod nm;
#[cfg(test)]
mod tests;

use crate::{
    common::{monotonic, Events},
    options::Options,
};
use nm::{bounded, Nm};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedFd, Type};
use zbus::{fdo, Connection};

#[cfg(test)]
const SERVICE: &str = crate::common::SERVICE;
pub const PATH: &str = "/io/github/guilhem/DeviceCore1/Network";
pub const INTERFACE: &str = "io.github.guilhem.DeviceCore1.Network";
const GRACE: Duration = Duration::from_secs(90);
const RETRY: Duration = Duration::from_secs(300);
const LEASE: Duration = Duration::from_secs(300);

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
    pub mode: String,
    pub generation: String,
    pub ready: bool,
    pub address: String,
    pub ssid: Vec<u8>,
    pub profile_uuid: String,
    pub attempt_id: u64,
    pub phase: String,
    pub error: String,
}

impl Status {
    fn reconnecting() -> Self {
        Self {
            mode: "reconnecting".into(),
            generation: String::new(),
            ready: false,
            address: String::new(),
            ssid: Vec::new(),
            profile_uuid: String::new(),
            attempt_id: 0,
            phase: "idle".into(),
            error: "".into(),
        }
    }
    fn unavailable() -> Self {
        Self {
            mode: "unavailable".into(),
            ..Self::reconnecting()
        }
    }
}

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
pub struct NetworkInfo {
    pub ssid: Vec<u8>,
    pub strength: u8,
    pub security: String,
}
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
pub struct Profile {
    pub uuid: String,
    pub ssid: Vec<u8>,
}

struct Reservation {
    token: String,
    created: Duration,
    last_press: Duration,
    expires: Duration,
    authorized_until: Option<Duration>,
}

struct Shared {
    status: Status,
    networks: Vec<NetworkInfo>,
    profiles: Vec<Profile>,
    reservation: Option<Reservation>,
    active: Option<(u64, Arc<AtomicBool>)>,
    committing: bool,
    next_attempt: u64,
    incarnation: String,
    generation: u64,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            status: Status::unavailable(),
            networks: Vec::new(),
            profiles: Vec::new(),
            reservation: None,
            active: None,
            committing: false,
            next_attempt: 0,
            incarnation: crate::common::token().expect("network incarnation entropy"),
            generation: 0,
        }
    }
}

impl Shared {
    fn reserved(&self, now: Duration) -> bool {
        self.reservation.as_ref().is_some_and(|r| now < r.expires)
    }

    pub fn authorized(&self, token: &str, now: Duration) -> bool {
        self.status.mode == "hotspot"
            && !token.is_empty()
            && self.reservation.as_ref().is_some_and(|r| {
                r.token == token
                    && now < r.expires
                    && r.authorized_until.is_some_and(|expires| now < expires)
            })
    }

    fn press(&mut self, edge: Duration, now: Duration) {
        if self.status.mode != "hotspot"
            || self.active.is_some()
            || edge > now
            || now.saturating_sub(edge) >= LEASE
        {
            return;
        }
        if let Some(r) = self.reservation.as_mut() {
            if edge > r.created
                && edge > r.last_press
                && edge < r.expires
                && now < r.expires
                && !r.authorized_until.is_some_and(|expires| now < expires)
            {
                r.authorized_until = Some(edge + LEASE);
                r.last_press = edge;
            }
        }
    }

    fn observe(&mut self, mut status: Status) {
        status.attempt_id = self.status.attempt_id;
        status.phase = self.status.phase.clone();
        status.error = self.status.error.clone();
        // Presence and reservation cannot survive a completed radio transition.
        if self.status.mode == "hotspot" && status.mode == "client" {
            self.reservation = None;
        }
        if (
            self.status.mode.as_str(),
            &self.status.address,
            &self.status.profile_uuid,
        ) != (status.mode.as_str(), &status.address, &status.profile_uuid)
        {
            self.generation += 1;
        }
        status.generation = format!("{}:{}", self.incarnation, self.generation);
        status.ready = false;
        self.status = status;
    }

    fn finish_attempt(&mut self, id: u64, result: nm::Result<()>, now: Duration) {
        if !self
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active == id)
        {
            return;
        }
        // Discard presence edges generated during the attempt, even if the reader
        // delivers them after failure or cancellation released the radio.
        if let Some(r) = self.reservation.as_mut() {
            r.last_press = now;
            r.authorized_until = None;
        }
        self.active = None;
        self.committing = false;
        match result {
            Ok(()) => {
                self.status.phase = "succeeded".into();
                self.status.error = "".into();
            }
            Err("cancelled") => {
                self.status.phase = "cancelled".into();
                self.status.error = "".into();
            }
            Err(error) => {
                self.status.phase = "failed".into();
                self.status.error = error.into();
            }
        }
    }
}

struct Request {
    ssid: Vec<u8>,
    security: String,
    password: String,
    uuid: String,
    setup: bool,
}

impl Request {
    fn validate(&self, hotspot_uuid: &str) -> fdo::Result<()> {
        if self.ssid.len() > 32 || self.password.len() > 64 || self.password.contains('\0') {
            return Err(fdo::Error::InvalidArgs("invalid-network".into()));
        }
        if !self.uuid.is_empty() {
            if valid_uuid(&self.uuid) && self.uuid != hotspot_uuid {
                return Ok(());
            }
            return Err(fdo::Error::InvalidArgs("invalid-profile".into()));
        }
        if self.ssid.is_empty() {
            return Err(fdo::Error::InvalidArgs("invalid-network".into()));
        }
        let size = self.password.len();
        let valid = match self.security.as_str() {
            "open" => size == 0,
            "wpa-psk" => {
                (8..=63).contains(&size)
                    || (size == 64 && self.password.bytes().all(|b| b.is_ascii_hexdigit()))
            }
            "sae" => (1..=63).contains(&size),
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(fdo::Error::InvalidArgs("invalid-security".into()))
        }
    }
}

enum Command {
    Scan,
    Connect {
        id: u64,
        request: Request,
        cancelled: Arc<AtomicBool>,
    },
    Forget {
        uuid: String,
        reply: oneshot::Sender<nm::Result<()>>,
    },
}

#[derive(Clone)]
pub struct Network {
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::Sender<Command>,
    options: Options,
    guard: guard::Guard,
}

#[zbus::interface(name = "io.github.guilhem.DeviceCore1.Network")]
impl Network {
    #[zbus(property)]
    pub fn status(&self) -> Status {
        self.shared.lock().unwrap().status.clone()
    }
    #[zbus(property)]
    pub fn networks(&self) -> Vec<NetworkInfo> {
        self.shared.lock().unwrap().networks.clone()
    }
    #[zbus(property)]
    pub fn profiles(&self) -> Vec<Profile> {
        self.shared.lock().unwrap().profiles.clone()
    }

    pub fn scan(&self) -> fdo::Result<()> {
        self.commands
            .try_send(Command::Scan)
            .map_err(|_| fdo::Error::Failed("network-busy".into()))
    }

    pub fn reserve(&self, token: &str) -> fdo::Result<String> {
        if token.len() > 64 {
            return Err(fdo::Error::InvalidArgs("invalid-reservation".into()));
        }
        let now = monotonic();
        let mut s = self.shared.lock().unwrap();
        if s.status.mode != "hotspot" || s.active.is_some() {
            return Err(fdo::Error::Failed("not-hotspot".into()));
        }
        if token.is_empty() {
            if s.reserved(now) {
                return Err(fdo::Error::Failed("network-busy".into()));
            }
            let token =
                random_token().map_err(|_| fdo::Error::Failed("random-unavailable".into()))?;
            s.reservation = Some(Reservation {
                token: token.clone(),
                created: now,
                last_press: now,
                expires: now + LEASE,
                authorized_until: None,
            });
            Ok(token)
        } else if let Some(r) = s
            .reservation
            .as_mut()
            .filter(|r| r.token == token && now < r.expires)
        {
            r.expires = now + LEASE;
            Ok(r.token.clone())
        } else {
            Err(fdo::Error::AccessDenied("invalid-reservation".into()))
        }
    }

    pub fn authorized(&self, token: &str) -> bool {
        self.shared.lock().unwrap().authorized(token, monotonic())
    }

    pub fn release(&self, token: &str) -> fdo::Result<()> {
        let mut s = self.shared.lock().unwrap();
        if s.reservation
            .as_ref()
            .is_some_and(|r| !token.is_empty() && r.token == token)
        {
            s.reservation = None;
            Ok(())
        } else {
            Err(fdo::Error::AccessDenied("invalid-reservation".into()))
        }
    }

    pub fn connect(
        &self,
        ssid: Vec<u8>,
        security: &str,
        password: &str,
        uuid: &str,
        token: &str,
    ) -> fdo::Result<u64> {
        let request = Request {
            ssid,
            security: security.into(),
            password: password.into(),
            uuid: uuid.into(),
            setup: !token.is_empty(),
        };
        request.validate(&self.options.hotspot_uuid)?;
        let mut s = self.shared.lock().unwrap();
        if s.active.is_some() {
            return Err(fdo::Error::Failed("network-busy".into()));
        }
        if request.setup && !s.authorized(token, monotonic()) {
            return Err(fdo::Error::AccessDenied(
                "physical-confirmation-required".into(),
            ));
        }
        if s.status.mode == "unavailable" {
            return Err(fdo::Error::Failed("nm-unavailable".into()));
        }
        s.next_attempt = s
            .next_attempt
            .checked_add(1)
            .ok_or_else(|| fdo::Error::Failed("attempt-overflow".into()))?;
        let id = s.next_attempt;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.commands
            .try_send(Command::Connect {
                id,
                request,
                cancelled: cancelled.clone(),
            })
            .map_err(|_| fdo::Error::Failed("network-busy".into()))?;
        if !token.is_empty() {
            if let Some(r) = s.reservation.as_mut() {
                r.authorized_until = None;
            }
        }
        s.active = Some((id, cancelled));
        s.status.ready = false;
        s.generation += 1;
        s.status.generation = format!("{}:{}", s.incarnation, s.generation);
        s.committing = false;
        s.status.attempt_id = id;
        s.status.phase = "connecting".into();
        s.status.error = "".into();
        Ok(id)
    }

    pub fn cancel(&self, attempt_id: u64) -> fdo::Result<()> {
        let s = self.shared.lock().unwrap();
        if let Some((id, cancelled)) = &s.active {
            if *id == attempt_id {
                if s.committing {
                    return Err(fdo::Error::Failed("attempt-committing".into()));
                }
                cancelled.store(true, Ordering::SeqCst);
                return Ok(());
            }
        }
        Err(fdo::Error::InvalidArgs("unknown-attempt".into()))
    }

    pub async fn forget(&self, uuid: &str) -> fdo::Result<()> {
        if !valid_uuid(uuid) || uuid == self.options.hotspot_uuid {
            return Err(fdo::Error::InvalidArgs("invalid-profile".into()));
        }
        if self.shared.lock().unwrap().active.is_some() {
            return Err(fdo::Error::Failed("network-busy".into()));
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .try_send(Command::Forget {
                uuid: uuid.into(),
                reply,
            })
            .map_err(|_| fdo::Error::Failed("network-busy".into()))?;
        tokio::time::timeout(Duration::from_secs(15), response)
            .await
            .map_err(|_| fdo::Error::Failed("nm-timeout".into()))?
            .map_err(|_| fdo::Error::Failed("nm-unavailable".into()))?
            .map_err(|error| fdo::Error::Failed(error.into()))
    }

    /// Deliberately D-Bus only: the header comes from the bus, not the caller.
    async fn report_presence(
        &self,
        monotonic_ns: u64,
        #[zbus(connection)] bus: &Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> fdo::Result<()> {
        crate::auth::authorize_unit(bus, &header, &self.options.presence_unit).await?;
        self.shared
            .lock()
            .unwrap()
            .press(Duration::from_nanos(monotonic_ns), monotonic());
        Ok(())
    }

    /// The returned FD already holds LOCK_SH. Keep it open until protected work
    /// ends; closing it, including on process death, releases its lock.
    pub fn acquire_guard(&self, expected_generation: &str) -> fdo::Result<OwnedFd> {
        let file = self
            .guard
            .shared()
            .map_err(|e| fdo::Error::Failed(e.into()))?;
        let s = self.shared.lock().unwrap();
        if !s.status.ready
            || s.status.mode != "client"
            || s.active.is_some()
            || s.status.generation != expected_generation
            || expected_generation.is_empty()
        {
            return Err(fdo::Error::Failed("network-generation-changed".into()));
        }
        Ok(std::os::fd::OwnedFd::from(file).into())
    }

    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>, status: Status) -> zbus::Result<()>;
}

fn json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("network JSON")
}

fn random_bytes() -> std::io::Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn random_token() -> std::io::Result<String> {
    Ok(random_bytes()?.iter().map(|b| format!("{b:02x}")).collect())
}

fn new_uuid() -> nm::Result<String> {
    let mut b = random_bytes().map_err(|_| "random-unavailable")?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7],b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15]))
}

fn valid_uuid(uuid: &str) -> bool {
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Register on the parent's connection; the parent alone owns the service name.
pub async fn start(bus: &Connection, options: Options, events: Events) -> fdo::Result<Network> {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (commands, receiver) = mpsc::channel(8);
    let guard = guard::Guard::new(options.network_guard.clone())
        .map_err(|e| fdo::Error::Failed(format!("network-guard: {e}")))?;
    let api = Network {
        shared: shared.clone(),
        commands,
        options: options.clone(),
        guard: guard.clone(),
    };
    bus.object_server().at(PATH, api.clone()).await?;
    let controller = Controller::new(
        bus.clone(),
        shared.clone(),
        receiver,
        options,
        events,
        guard,
    );
    tokio::spawn(async move {
        if controller.run().await.is_err() {
            shared.lock().unwrap().observe(Status::unavailable());
        }
    });
    Ok(api)
}

struct Controller {
    bus: Connection,
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::Receiver<Command>,
    owner: String,
    grace: Option<Instant>,
    retry: Instant,
    last_emitted: String,
    options: Options,
    events: Events,
    guard: guard::Guard,
}

impl Controller {
    fn new(
        bus: Connection,
        shared: Arc<Mutex<Shared>>,
        commands: mpsc::Receiver<Command>,
        options: Options,
        events: Events,
        guard: guard::Guard,
    ) -> Self {
        Self {
            bus,
            options,
            events,
            guard,
            shared,
            commands,
            owner: String::new(),
            grace: Some(Instant::now() + GRACE),
            retry: Instant::now() + RETRY,
            last_emitted: String::new(),
        }
    }

    async fn run(mut self) -> nm::Result<()> {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => { self.reconcile().await; }
                command = self.commands.recv() => match command {
                    Some(Command::Connect { id, request, cancelled }) => {
                        self.emit().await?;
                        // Reply/progress reaches Go before the single radio switches.
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let result = self.attempt(&request, &cancelled).await;
                        let mut s = self.shared.lock().unwrap();
                        s.finish_attempt(id, result, monotonic());
                    }
                    Some(Command::Scan) => {
                        if self.options.simulate { self.emit().await?; continue; }
                        let Ok(_lock) = self.guard.exclusive() else { continue; };
                        if let Ok(nm) = Nm::discover(&self.bus, &self.options).await {
                            let _ = nm.scan().await;
                            self.refresh_networks(&nm).await;
                        }
                    }
                    Some(Command::Forget { uuid, reply }) => {
                        let result = self.forget(&uuid).await;
                        let _ = reply.send(result);
                    }
                    None => return Err("bus-unavailable"),
                }
            }
            self.emit().await?;
        }
    }

    async fn emit(&mut self) -> nm::Result<()> {
        let (status, networks, profiles) = {
            let s = self.shared.lock().unwrap();
            (s.status.clone(), s.networks.clone(), s.profiles.clone())
        };
        let all = json(&(&status, &networks, &profiles));
        if all == self.last_emitted {
            return Ok(());
        }
        self.events.emit(
            "network",
            &serde_json::json!({"status": status, "networks": networks, "profiles": profiles}),
        );
        let emitter = SignalEmitter::new(&self.bus, PATH).map_err(|_| "bus-unavailable")?;
        bounded(Network::changed(&emitter, status.clone())).await?;
        let properties = nm::Dict::new();
        bounded(self.bus.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                INTERFACE,
                properties,
                vec!["Status", "Networks", "Profiles"],
            ),
        ))
        .await?;
        self.last_emitted = all;
        Ok(())
    }

    async fn refresh_networks(&self, nm: &Nm) {
        if let Ok(Ok(networks)) = tokio::time::timeout(Duration::from_secs(10), nm.networks()).await
        {
            let mut s = self.shared.lock().unwrap();
            if !networks.is_empty() || s.networks.is_empty() {
                s.networks = networks;
            }
        }
    }

    async fn reconcile(&mut self) {
        let result = tokio::time::timeout(Duration::from_secs(15), self.reconcile_inner()).await;
        if matches!(result, Ok(Err("network-guard-busy"))) {
            if !self.options.simulate {
                if let Ok(nm) = Nm::discover(&self.bus, &self.options).await {
                    if let Ok(snapshot) = nm.snapshot().await {
                        let mut s = self.shared.lock().unwrap();
                        if nm.owner != self.owner
                            || snapshot.status.mode != s.status.mode
                            || snapshot.status.address != s.status.address
                            || snapshot.status.profile_uuid != s.status.profile_uuid
                        {
                            s.observe(snapshot.status);
                        }
                    } else {
                        self.shared.lock().unwrap().observe(Status::unavailable());
                    }
                } else {
                    self.shared.lock().unwrap().observe(Status::unavailable());
                }
            }
            return;
        }
        if !matches!(result, Ok(Ok(()))) {
            self.shared.lock().unwrap().observe(Status::unavailable());
        }
    }

    async fn reconcile_inner(&mut self) -> nm::Result<()> {
        let _lock = self.guard.exclusive()?;
        if self.options.simulate {
            return self.sim_reconcile();
        }
        self.reconcile_locked().await
    }

    async fn reconcile_locked(&mut self) -> nm::Result<()> {
        let nm = Nm::discover(&self.bus, &self.options).await?;
        let restarted = nm.owner != self.owner;
        if restarted {
            self.owner = nm.owner.clone();
            self.shared.lock().unwrap().generation += 1;
            self.grace = Some(Instant::now() + GRACE);
            self.retry = Instant::now() + RETRY;
            self.shared.lock().unwrap().reservation = None;
        }
        let checkpoints = nm.checkpoints().await?;
        let profiles = nm.profiles().await?;
        // An interrupted transaction remains protected by NM's own timer. Only
        // orphaned marked profiles created by this core are cleaned up, even
        // when preparation persisted them before NM lost its checkpoint.
        if checkpoints.is_empty() {
            for candidate in profiles.iter().filter(|p| p.candidate) {
                nm.discard_candidate(&candidate.path, &candidate.public.uuid)
                    .await?;
            }
        }
        self.shared.lock().unwrap().profiles = profiles
            .iter()
            .filter(|p| !p.candidate)
            .map(|p| p.public.clone())
            .collect();
        let snapshot = nm.snapshot().await?;
        let mode = snapshot.status.mode.clone();
        let previous = self.shared.lock().unwrap().status.mode.clone();
        self.shared.lock().unwrap().observe(snapshot.status);
        self.refresh_networks(&nm).await;
        if !checkpoints.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        if mode == "client" {
            {
                let mut s = self.shared.lock().unwrap();
                s.status.ready = s.active.is_none();
            }
            self.grace = None;
            self.retry = now + RETRY;
        } else if mode == "hotspot" {
            self.grace = None;
            let reserved = self.shared.lock().unwrap().reserved(monotonic());
            if now >= self.retry && !reserved && self.shared.lock().unwrap().active.is_none() {
                self.retry = now + RETRY;
                let networks = self.shared.lock().unwrap().networks.clone();
                // Prefer a visible native profile; otherwise let the first known
                // one try (hidden networks do not appear in scan results).
                let known = networks
                    .iter()
                    .find_map(|n| {
                        profiles
                            .iter()
                            .find(|p| !p.candidate && p.public.ssid == n.ssid)
                    })
                    .or_else(|| profiles.iter().find(|p| !p.candidate));
                if let Some(known) = known {
                    // Make the transition visible before awaiting NM. A new
                    // reservation cannot be granted after the retry has begun.
                    self.shared.lock().unwrap().observe(Status::reconnecting());
                    let _ = nm.activate(&known.path).await;
                    self.grace = Some(now + GRACE);
                }
            }
        } else {
            if self.grace.is_none() || previous == "client" {
                self.grace = Some(now + GRACE);
            }
            if self.grace.is_some_and(|deadline| now >= deadline) {
                nm.hotspot().await?;
                self.grace = Some(now + GRACE);
                self.retry = now + RETRY;
            }
        }
        Ok(())
    }

    fn sim_reconcile(&mut self) -> nm::Result<()> {
        let mut s = self.shared.lock().unwrap();
        if s.status.mode == "unavailable" {
            let mut status = Status::reconnecting();
            status.mode = "hotspot".into();
            status.profile_uuid = self.options.hotspot_uuid.clone();
            status.address = self.options.hotspot_address.clone();
            status.ssid = self.options.hotspot_prefix.as_bytes().to_vec();
            s.observe(status);
        }
        s.status.ready = s.status.mode == "client" && s.active.is_none();
        Ok(())
    }

    fn sim_attempt(&mut self, request: &Request, cancelled: &AtomicBool) -> nm::Result<()> {
        if cancelled.load(Ordering::SeqCst) {
            return Err("cancelled");
        }
        let mut s = self.shared.lock().unwrap();
        let uuid = if request.uuid.is_empty() {
            new_uuid()?
        } else {
            request.uuid.clone()
        };
        if !request.uuid.is_empty() && !s.profiles.iter().any(|p| p.uuid == uuid) {
            return Err("unknown-profile");
        }
        let mut status = Status::reconnecting();
        status.mode = "client".into();
        status.address = "192.0.2.1".into();
        status.ssid = request.ssid.clone();
        status.profile_uuid = uuid.clone();
        if request.uuid.is_empty() {
            s.profiles.push(Profile {
                uuid,
                ssid: request.ssid.clone(),
            });
        }
        s.observe(status);
        Ok(())
    }

    async fn forget(&mut self, uuid: &str) -> nm::Result<()> {
        let _lock = self.guard.exclusive()?;
        if self.options.simulate {
            self.shared
                .lock()
                .unwrap()
                .profiles
                .retain(|p| p.uuid != uuid);
            return Ok(());
        }
        if self.shared.lock().unwrap().active.is_some() {
            return Err("network-busy");
        }
        let nm = Nm::discover(&self.bus, &self.options).await?;
        if !nm.checkpoints().await?.is_empty() {
            return Err("network-busy");
        }
        let profiles = nm.profiles().await?;
        let profile = profiles
            .iter()
            .find(|p| !p.candidate && p.public.uuid == uuid)
            .ok_or("unknown-profile")?;
        nm.delete(&profile.path).await?;
        self.shared
            .lock()
            .unwrap()
            .profiles
            .retain(|p| p.uuid != uuid);
        Ok(())
    }

    async fn attempt(&mut self, request: &Request, cancelled: &AtomicBool) -> nm::Result<()> {
        let _lock = self.guard.exclusive()?;
        if self.options.simulate {
            return self.sim_attempt(request, cancelled);
        }
        if cancelled.load(Ordering::SeqCst) {
            return Err("cancelled");
        }
        let nm = Nm::discover(&self.bus, &self.options).await?;
        let before = nm.snapshot().await?;
        if request.setup && before.status.mode != "hotspot" {
            return Err("not-hotspot");
        }
        if !nm.checkpoints().await?.is_empty() {
            return Err("network-busy");
        }
        let existing = if request.uuid.is_empty() {
            None
        } else {
            let profiles = nm.profiles().await?;
            Some(
                profiles
                    .into_iter()
                    .find(|p| !p.candidate && p.public.uuid == request.uuid)
                    .ok_or("unknown-profile")?,
            )
        };
        if cancelled.load(Ordering::SeqCst) {
            return Err("cancelled");
        }
        let uuid = match &existing {
            Some(p) => p.public.uuid.clone(),
            None => new_uuid()?,
        };
        let mut settings = nm::wifi_settings(
            &uuid,
            &format!("{}{}", nm::CANDIDATE_PREFIX, uuid),
            &request.ssid,
            &request.security,
            &request.password,
        );
        let started = Instant::now();
        let checkpoint = nm.checkpoint().await?;
        let mut candidate = None;
        let mut promoting = false;
        let operation = async {
            let path = match existing {
                Some(ref saved) => saved.path.clone(),
                None => {
                    let path = nm.add(&settings, false).await?;
                    candidate = Some(path.clone());
                    path
                }
            };
            if cancelled.load(Ordering::SeqCst) {
                return Err("cancelled");
            }
            nm.activate(&path).await?;
            loop {
                if cancelled.load(Ordering::SeqCst) {
                    return Err("cancelled");
                }
                // Leave time for the bounded save and destroy calls before NM's
                // non-extendable 90-second rollback timer.
                if Instant::now() >= started + GRACE - Duration::from_secs(15) {
                    return Err("connection-timeout");
                }
                let snapshot = nm.snapshot().await?;
                let success =
                    snapshot.status.mode == "client" && snapshot.status.profile_uuid == uuid;
                self.shared.lock().unwrap().observe(snapshot.status);
                self.emit().await?;
                if snapshot.failed {
                    return Err(match snapshot.reason {
                        7..=11 => "wifi-authentication-failed",
                        5 | 6 | 15 | 16 | 17 => "no-address",
                        _ => "activation-failed",
                    });
                }
                if success {
                    if cancelled.load(Ordering::SeqCst) {
                        return Err("cancelled");
                    }
                    let version = if candidate.is_some() {
                        // Prepare durably, still marked and with autoconnect off.
                        // Native profiles are activated without any settings write.
                        nm.save(&path, &settings, 0).await?;
                        nm.version(&path).await?
                    } else {
                        0
                    };
                    if !nm.checkpoints().await?.contains(&checkpoint) {
                        return Err("checkpoint-expired");
                    }
                    // Re-read after persistence; a concurrent loss/foreign
                    // activation must not commit a previously good snapshot.
                    let current = nm.snapshot().await?;
                    if current.status.mode != "client" || current.status.profile_uuid != uuid {
                        return Err("connection-lost");
                    }
                    {
                        // Cancel uses the same mutex: every accepted cancellation
                        // precedes commit, including one during the last snapshot.
                        let mut s = self.shared.lock().unwrap();
                        if cancelled.load(Ordering::SeqCst) {
                            return Err("cancelled");
                        }
                        s.committing = true;
                    }
                    nm.destroy(&checkpoint).await?;
                    if candidate.is_some() {
                        let connection = settings.get_mut("connection").unwrap();
                        connection.insert("id".into(), nm::string(nm::COMMITTED_ID));
                        connection.insert("autoconnect".into(), true.into());
                        promoting = true;
                        nm.save(&path, &settings, version).await?;
                    }
                    self.shared.lock().unwrap().observe(current.status);
                    self.grace = None;
                    self.retry = Instant::now() + RETRY;
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        };
        let mut result =
            tokio::time::timeout_at(started + GRACE - Duration::from_secs(2), operation)
                .await
                .unwrap_or(Err("connection-timeout"));
        if result.is_err() && promoting {
            // A lost promotion reply may have committed. Resolve by UUID with a
            // fresh owner; never send an old object's path to a replacement NM.
            let resolution = async {
                let current_nm = Nm::discover(&self.bus, &self.options).await?;
                let profiles = current_nm.profiles().await?;
                if let Some(profile) = profiles.iter().find(|p| p.public.uuid == uuid) {
                    if !profile.candidate {
                        let saved: nm::Settings = current_nm
                            .call(profile.path.as_str(), nm::CONNECTION, "GetSettings", &())
                            .await?;
                        let autoconnect = saved
                            .get("connection")
                            .and_then(|s| s.get("autoconnect"))
                            .and_then(|v| bool::try_from(v).ok());
                        if nm::text(&saved, "connection", "id") != nm::COMMITTED_ID
                            || autoconnect != Some(true)
                            || current_nm
                                .property::<bool>(profile.path.as_str(), nm::CONNECTION, "Unsaved")
                                .await?
                        {
                            return Err("commit-unconfirmed");
                        }
                        let status = current_nm
                            .snapshot()
                            .await
                            .map(|s| s.status)
                            .unwrap_or_else(|_| Status::unavailable());
                        self.shared.lock().unwrap().observe(status);
                        return Ok(true);
                    }
                }
                if !current_nm.checkpoints().await?.is_empty() {
                    return Err("commit-unconfirmed");
                }
                if let Some(profile) = profiles.iter().find(|p| p.public.uuid == uuid) {
                    current_nm.discard_candidate(&profile.path, &uuid).await?;
                }
                // Destroy succeeded, so native rollback is no longer available.
                // Recover the prior radio mode without rewriting its profile.
                if before.status.mode == "hotspot" {
                    current_nm.hotspot().await?;
                } else if let Some(previous) = profiles
                    .iter()
                    .find(|p| !p.candidate && p.public.uuid == before.status.profile_uuid)
                {
                    current_nm.activate(&previous.path).await?;
                }
                let status = current_nm.snapshot().await?.status;
                self.shared.lock().unwrap().observe(status);
                Ok(false)
            }
            .await;
            match resolution {
                Ok(true) => result = Ok(()),
                Ok(false) => {}
                Err(_) => {
                    result = Err("commit-unconfirmed");
                    self.shared.lock().unwrap().observe(Status::unavailable());
                }
            }
            self.grace = Some(Instant::now() + GRACE);
            self.retry = Instant::now() + RETRY;
        } else if result.is_err() {
            // A failed rollback keeps the native timer armed. Never destroy a
            // checkpoint whose restoration was not confirmed.
            let restored = nm.rollback(&checkpoint).await.is_ok();
            if restored {
                let _ = nm.destroy(&checkpoint).await;
            }
            if let Some(path) = candidate {
                let _ = nm.discard_candidate(&path, &uuid).await;
            }
            // Client rollback starts asynchronous reactivation/DHCP. Let the
            // existing grace below finish that recovery before opening an AP.
            if restored
                && before.status.mode == "hotspot"
                && nm.checkpoints().await.is_ok_and(|c| c.is_empty())
            {
                if let Ok(snapshot) = nm.snapshot().await {
                    if snapshot.status.mode != "client" && snapshot.status.mode != "hotspot" {
                        let _ = nm.hotspot().await;
                    }
                }
            }
            if let Ok(snapshot) = nm.snapshot().await {
                self.shared.lock().unwrap().observe(snapshot.status);
            } else {
                self.shared.lock().unwrap().observe(Status::unavailable());
            }
            self.grace = Some(Instant::now() + GRACE);
        }
        if result.is_err() && cancelled.load(Ordering::SeqCst) {
            Err("cancelled")
        } else {
            result
        }
    }
}
