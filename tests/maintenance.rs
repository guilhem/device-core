use device_core::{
    common::{Events, Gate, ROOT},
    maintenance::Coordinator,
    manager::Manager,
    options::Options,
};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use zbus::{fdo, message::Header, zvariant::OwnedObjectPath, Connection, Proxy};

#[derive(Default)]
struct State {
    held: Option<(String, String)>,
    released: Option<String>,
    aborted: Vec<String>,
}
struct Agent {
    expected_sender: String,
    incarnation: String,
    state: Arc<Mutex<State>>,
    lost_reply: bool,
}
impl Agent {
    fn authenticate(&self, header: &Header<'_>) -> fdo::Result<()> {
        if header.sender().map(|sender| sender.as_str()) != Some(self.expected_sender.as_str()) {
            return Err(fdo::Error::AccessDenied("wrong daemon".into()));
        }
        Ok(())
    }
}
#[zbus::interface(name = "io.github.guilhem.DeviceCore1.Agent")]
impl Agent {
    fn acquire(&self, operation: &str, #[zbus(header)] header: Header<'_>) -> fdo::Result<String> {
        self.authenticate(&header)?;
        let mut state = self.state.lock().unwrap();
        let token = format!("{}:{operation}", self.incarnation);
        state.held = Some((operation.into(), token.clone()));
        if self.lost_reply {
            Err(fdo::Error::Failed("accepted but reply lost".into()))
        } else {
            Ok(token)
        }
    }
    fn release(&self, token: &str, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.authenticate(&header)?;
        let mut state = self.state.lock().unwrap();
        if state.released.as_deref() == Some(token) {
            return Ok(());
        }
        if !state.held.as_ref().is_some_and(|(_, held)| held == token) {
            return Err(fdo::Error::AccessDenied(
                "token from wrong incarnation".into(),
            ));
        }
        state.held = None;
        state.released = Some(token.into());
        Ok(())
    }
    fn abort(&self, operation: &str, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.authenticate(&header)?;
        if self.lost_reply {
            return Err(fdo::Error::Failed("agent disappearing".into()));
        }
        let mut state = self.state.lock().unwrap();
        if state
            .held
            .as_ref()
            .is_some_and(|(held, _)| held != operation)
        {
            return Err(fdo::Error::AccessDenied("wrong operation".into()));
        }
        state.held = None;
        state.aborted.push(operation.into());
        Ok(())
    }
}
fn current_user() -> String {
    String::from_utf8(
        std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .into()
}
async fn connection(address: &str) -> Connection {
    zbus::connection::Builder::address(address)
        .unwrap()
        .build()
        .await
        .unwrap()
}
async fn agent(
    address: &str,
    server: &Connection,
    name: &str,
    lost_reply: bool,
) -> (Connection, Arc<Mutex<State>>) {
    let bus = connection(address).await;
    let state: Arc<Mutex<State>> = Arc::default();
    bus.object_server()
        .at(
            "/agent",
            Agent {
                expected_sender: server.unique_name().unwrap().to_string(),
                incarnation: name.into(),
                state: state.clone(),
                lost_reply,
            },
        )
        .await
        .unwrap();
    (bus, state)
}
async fn register(server: &Connection, agent: &Connection) {
    Proxy::new(
        agent,
        server.unique_name().unwrap(),
        ROOT,
        "io.github.guilhem.DeviceCore1.Manager",
    )
    .await
    .unwrap()
    .call::<_, _, ()>(
        "RegisterAgent",
        &(OwnedObjectPath::try_from("/agent").unwrap(),),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn reservations_and_failed_acquisitions_follow_authorized_agent_restarts() {
    let mut bus = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let address = BufReader::new(bus.stdout.take().unwrap())
        .lines()
        .next_line()
        .await
        .unwrap()
        .unwrap();
    let server = connection(&address).await;
    let mut options = Options::from_env(true);
    options.maintenance_users = vec![current_user()];
    let gate = Gate::default();
    let coordinator = Coordinator::new(
        server.clone(),
        options.clone(),
        gate.clone(),
        Events::default(),
    );
    server
        .object_server()
        .at(
            ROOT,
            Manager {
                coordinator: coordinator.clone(),
                options,
                gate: gate.clone(),
                events: Events::default(),
                instance: "test".into(),
                ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        )
        .await
        .unwrap();
    let (old, old_state) = agent(&address, &server, "old", false).await;
    register(&server, &old).await;
    let mut reservation = coordinator.acquire("install-1").await.unwrap();
    assert!(old_state.lock().unwrap().held.is_some());
    let old_sender = old.unique_name().unwrap().to_string();
    old.close().await.unwrap();
    coordinator.lost(&old_sender);
    assert!(reservation.release().await.is_err());
    assert!(gate.blocked());
    let (new, new_state) = agent(&address, &server, "new", false).await;
    register(&server, &new).await;
    // Delayed owner-loss handling for the old unique name must retain its replacement.
    coordinator.lost(&old_sender);
    reservation.release().await.unwrap();
    assert!(!gate.blocked());
    assert_eq!(
        new_state.lock().unwrap().released.as_deref(),
        Some("new:install-1")
    );
    drop(reservation);
    let new_sender = new.unique_name().unwrap().to_string();
    new.close().await.unwrap();
    coordinator.lost(&new_sender);

    let (failing, _) = agent(&address, &server, "failing", true).await;
    register(&server, &failing).await;
    assert!(coordinator.acquire("install-2").await.is_err());
    assert!(gate.blocked());
    let sender = failing.unique_name().unwrap().to_string();
    failing.close().await.unwrap();
    coordinator.lost(&sender);
    assert!(coordinator.abort_pending("install-2").await.is_err());
    let (replacement, replacement_state) = agent(&address, &server, "replacement", false).await;
    register(&server, &replacement).await;
    coordinator.abort_pending("install-2").await.unwrap();
    coordinator.abort_pending("install-2").await.unwrap();
    assert_eq!(replacement_state.lock().unwrap().aborted, ["install-2"]);
    // Only the updater, after a conclusive RAUC probe, may reopen this gate.
    assert!(gate.blocked());
    // This is one account with successive connections, not two service identities.
    let mismatch = if unsafe { libc::geteuid() } == 0 {
        "nobody"
    } else {
        "root"
    };
    assert_ne!(device_core::auth::user_uid(mismatch).unwrap(), unsafe {
        libc::geteuid()
    });
    for users in [
        vec![],
        vec![mismatch.into()],
        vec![current_user(), "device-core-no-such-account".into()],
        vec![current_user(), String::new()],
        vec![current_user(), current_user()],
    ] {
        let mut options = Options::from_env(true);
        options.maintenance_users = users;
        let restricted =
            Coordinator::new(server.clone(), options, Gate::default(), Events::default());
        let denied = restricted
            .register(
                replacement.unique_name().unwrap().as_str(),
                "/agent".try_into().unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(denied, fdo::Error::AccessDenied(_)), "{denied}");
    }
    assert!(coordinator
        .register(
            "io.github.guilhem.DeviceCore1",
            "/agent".try_into().unwrap()
        )
        .await
        .is_err());
    bus.kill().await.unwrap();
}
