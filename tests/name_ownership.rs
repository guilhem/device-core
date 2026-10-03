//! Real daemon startup on a private bus: public names cannot be replaced.
use device_core::common::{token, SERVICE};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
use zbus::{fdo::DBusProxy, Connection};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn daemon(address: &str, root: &Directory, name: &str) -> Process {
    Process(
        Command::new(env!("CARGO_BIN_EXE_device-core"))
            .arg("--simulate")
            .env("DEVICE_CORE_BUS_ADDRESS", address)
            .env("DEVICE_CORE_DATA_DIR", root.0.join(name))
            .env(
                "DEVICE_CORE_NETWORK_GUARD",
                root.0.join(format!("{name}-guard")),
            )
            .env("DEVICE_CORE_HTTP_ADDR", "")
            .env("DEVICE_CORE_MAINTENANCE_UNITS", "")
            .env("DEVICE_CORE_UPDATE_REPO", "")
            .env("DEVICE_CORE_UPDATE_ASSET", "")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}
async fn owner(bus: &Connection) -> Option<String> {
    DBusProxy::new(bus)
        .await
        .unwrap()
        .get_name_owner(SERVICE.try_into().unwrap())
        .await
        .ok()
        .map(|v| v.to_string())
}
async fn wait_owner(bus: &Connection) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(owner) = owner(bus).await {
                return owner;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn daemon_refuses_replacement_and_does_not_replace_an_existing_owner() {
    let root =
        Directory(std::env::temp_dir().join(format!("device-core-name-{}", token().unwrap())));
    fs::create_dir(&root.0).unwrap();
    let address = format!("unix:path={}", root.0.join("bus").display());
    let mut private = Process(
        Command::new("dbus-daemon")
            .args([
                "--session",
                "--nofork",
                "--print-address=1",
                "--address",
                &address,
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(private.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert!(ready.starts_with(&address));
    let contender = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let original = daemon(&address, &root, "original");
    let original_owner = wait_owner(&contender).await;
    assert!(
        contender.request_name(SERVICE).await.is_err(),
        "active daemon must refuse takeover"
    );
    assert_eq!(owner(&contender).await.unwrap(), original_owner);
    drop(original);
    tokio::time::timeout(Duration::from_secs(5), async {
        while owner(&contender).await.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // This owner explicitly allows replacement; the daemon still must not replace it.
    contender.request_name(SERVICE).await.unwrap();
    let mut duplicate = daemon(&address, &root, "duplicate");
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = duplicate.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !status.success(),
        "duplicate daemon must fail promptly, not queue or replace"
    );
    assert_eq!(
        owner(&contender).await.unwrap(),
        contender.unique_name().unwrap().as_str()
    );
    contender.release_name(SERVICE).await.unwrap();
    let _restarted = daemon(&address, &root, "restarted");
    assert_ne!(wait_owner(&contender).await, original_owner);
}
