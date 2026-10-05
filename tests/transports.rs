use device_core::{
    audio::Status,
    common::{token, ROOT, SERVICE},
    config::Settings,
    options::Options,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::{sleep, timeout},
};
use zbus::{Connection, Proxy};

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn daemon(address: &str, data: &Temp, http: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_device-core"));
    command
        .arg("--simulate")
        .env("DEVICE_CORE_BUS_ADDRESS", address)
        .env("DEVICE_CORE_DATA_DIR", data.0.join("state"))
        .env("DEVICE_CORE_NETWORK_GUARD", data.0.join("network.lock"))
        .env("DEVICE_CORE_AUDIO_ROOTS", &data.0)
        .env("DEVICE_CORE_SIM_AUDIO_MS", "10000")
        .env_remove("DEVICE_CORE_UPDATE_REPO")
        .env_remove("DEVICE_CORE_UPDATE_ASSET")
        .env_remove("DEVICE_CORE_LVA_UNIT")
        .env_remove("DEVICE_CORE_MAINTENANCE_USERS")
        .env_remove("DEVICE_CORE_PRESENCE_USER")
        .env_remove("DEVICE_CORE_HTTP_ADDR")
        .kill_on_drop(true);
    if let Some(http) = http {
        command.env("DEVICE_CORE_HTTP_ADDR", http);
    }
    command.spawn().unwrap()
}
async fn ready(connection: &Connection) {
    timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(proxy) = Proxy::new(
                connection,
                SERVICE,
                ROOT,
                "io.github.guilhem.DeviceCore1.Manager",
            )
            .await
            {
                if proxy.get_property::<bool>("Ready").await.unwrap_or(false) {
                    break;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon startup");
}

#[tokio::test]
async fn standalone_daemon_shared_transports_restart_and_owner_loss() {
    assert!(
        Options::from_env(true).http_addr.is_none(),
        "HTTP must be opt-in"
    );
    let temp = Temp(std::env::temp_dir().join(format!("device-transports-{}", token().unwrap())));
    fs::create_dir(&temp.0).unwrap();
    fs::write(temp.0.join("sample.mp3"), b"simulated-media").unwrap();
    let mut bus = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = BufReader::new(bus.stdout.take().unwrap()).lines();
    let address = output.next_line().await.unwrap().unwrap();
    let connection = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut process = daemon(&address, &temp, None);
    ready(&connection).await;
    let manager = Proxy::new(
        &connection,
        SERVICE,
        ROOT,
        "io.github.guilhem.DeviceCore1.Manager",
    )
    .await
    .unwrap();
    assert!(!manager.get_property::<bool>("Maintenance").await.unwrap());
    let initial_instance: String = manager.get_property("Instance").await.unwrap();
    let config = Proxy::new(
        &connection,
        SERVICE,
        format!("{ROOT}/Config"),
        "io.github.guilhem.DeviceCore1.Config",
    )
    .await
    .unwrap();
    let (revision, settings): (String, Settings) = config.call("Read", &()).await.unwrap();
    assert_eq!(settings.volume, 100);
    process.kill().await.unwrap();
    process.wait().await.unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let http_address = listener.local_addr().unwrap().to_string();
    drop(listener);
    process = daemon(&address, &temp, Some(&http_address));
    ready(&connection).await;
    // New proxy: cached properties from a previous owner must never be trusted.
    let manager: Proxy<'_> = zbus::proxy::Builder::new(&connection)
        .destination(SERVICE)
        .unwrap()
        .path(ROOT)
        .unwrap()
        .interface("io.github.guilhem.DeviceCore1.Manager")
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    assert_ne!(
        manager.get_property::<String>("Instance").await.unwrap(),
        initial_instance
    );
    let client = reqwest::Client::new();
    let url = format!("http://{http_address}/v1");
    let document: Value = client
        .get(format!("{url}/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(document["revision"], revision);
    let mut settings = document["settings"].clone();
    settings["volume"] = json!(31);
    let response = client
        .put(format!("{url}/config"))
        .json(&json!({"expected_revision": document["revision"], "settings": settings}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let (new_revision, changed): (String, Settings) = config.call("Read", &()).await.unwrap();
    assert_eq!(changed.volume, 31);
    let stale = client
        .put(format!("{url}/config"))
        .json(&json!({"expected_revision": document["revision"], "settings": settings}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let stale_dbus: zbus::Result<String> = config
        .call("Update", &(document["revision"].as_str().unwrap(), changed))
        .await;
    assert!(stale_dbus.is_err());
    assert_ne!(new_revision, revision);
    for endpoint in ["network/presence", "network/guard"] {
        assert_eq!(
            client
                .post(format!("{url}/{endpoint}"))
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    let network = Proxy::new(
        &connection,
        SERVICE,
        format!("{ROOT}/Network"),
        "io.github.guilhem.DeviceCore1.Network",
    )
    .await
    .unwrap();
    assert!(network
        .call::<_, _, ()>("ReportPresence", &(1u64,))
        .await
        .is_err());
    let ssh: Value = client
        .get(format!("{url}/system/ssh-keys"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!ssh["revision"].as_str().unwrap().is_empty());
    let stale = client
        .put(format!("{url}/system/ssh-keys"))
        .json(&json!({
            "expected_revision": "stale", "keys": "",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let system = Proxy::new(
        &connection,
        SERVICE,
        format!("{ROOT}/System"),
        "io.github.guilhem.DeviceCore1.System",
    )
    .await
    .unwrap();
    let (revision, keys): (String, String) = system.call("GetSSHKeys", &()).await.unwrap();
    assert_eq!(revision, ssh["revision"]);
    assert_eq!(keys, ssh["keys"]);
    assert!(system
        .call::<_, _, String>("SetSSHKeys", &("stale", ""))
        .await
        .is_err());
    let mut sse = client
        .get(format!("{url}/events"))
        .send()
        .await
        .unwrap()
        .bytes_stream();
    let first = timeout(Duration::from_secs(2), sse.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&first).contains("event: snapshot"));
    let audio = Proxy::new(
        &connection,
        SERVICE,
        format!("{ROOT}/Audio"),
        "io.github.guilhem.DeviceCore1.Audio",
    )
    .await
    .unwrap();
    assert!(audio
        .call::<_, _, String>("Start", &("file", "/etc/passwd"))
        .await
        .is_err());
    assert_eq!(
        client
            .post(format!("{url}/audio/playbacks"))
            .json(&json!({"kind": "file", "source": "/etc/passwd"}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let media = temp.0.join("sample.mp3").to_string_lossy().into_owned();
    let old: String = audio
        .call("Start", &("file", media.as_str()))
        .await
        .unwrap();
    let current: Value = client
        .post(format!("{url}/audio/playbacks"))
        .json(&json!({"kind": "file", "source": media}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let outcome: String = audio.call("Wait", &(old.as_str(),)).await.unwrap();
    assert_eq!(outcome, "preempted");
    audio
        .call::<_, _, ()>("Stop", &(old.as_str(),))
        .await
        .unwrap();
    let status: Status = audio.get_property("Status").await.unwrap();
    assert_eq!(status.id, current["operation_id"].as_str().unwrap());
    client
        .delete(format!("{url}/audio/playbacks/{}", status.id))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let owned_connection = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let owned_audio = Proxy::new(
        &owned_connection,
        SERVICE,
        format!("{ROOT}/Audio"),
        "io.github.guilhem.DeviceCore1.Audio",
    )
    .await
    .unwrap();
    let owned: String = owned_audio
        .call("Start", &("file", media.as_str()))
        .await
        .unwrap();
    drop(owned_audio);
    owned_connection.close().await.unwrap();
    let outcome: String = timeout(
        Duration::from_secs(3),
        audio.call("Wait", &(owned.as_str(),)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome, "owner-lost");
    client
        .post(format!("{url}/system/reboot"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(!manager.get_property::<bool>("Maintenance").await.unwrap());
    let system = Proxy::new(
        &connection,
        SERVICE,
        format!("{ROOT}/System"),
        "io.github.guilhem.DeviceCore1.System",
    )
    .await
    .unwrap();
    system.call::<_, _, ()>("PowerOff", &()).await.unwrap();
    assert!(!manager.get_property::<bool>("Maintenance").await.unwrap());
    process.kill().await.unwrap();
    process.wait().await.unwrap();
    bus.kill().await.unwrap();
}
