//! End-to-end simulations: real zbus calls on a private bus, fake HTTP/RAUC.
//! No bypass is enabled in production; every test explicitly sets Options.simulate.
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
    Router,
};
use device_core::{
    common::{token, Events, Gate},
    options::Options,
    update::{Hooks, Release, Settings, Status, Updater},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::watch;
use zbus::{
    fdo,
    zvariant::{OwnedValue, Type},
    Connection, Proxy,
};

const RAUC: &str = "de.pengutronix.rauc";
const INSTALLER: &str = "de.pengutronix.rauc.Installer";
const UPDATES: &str = "io.github.guilhem.DeviceCore1.Updates";
const PATH: &str = "/io/github/guilhem/DeviceCore1/Updates";
const ASSET: &str = "board.raucb";

struct PrivateBus {
    child: Child,
    dir: PathBuf,
    address: String,
}
impl PrivateBus {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("updates-test-{}", token().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        let socket = dir.join("bus");
        let address = format!("unix:path={}", socket.display());
        let child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", &format!("--address={address}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("dbus-daemon required");
        tokio::time::timeout(Duration::from_secs(3), async {
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("private bus failed to start");
        Self {
            child,
            dir,
            address,
        }
    }
    async fn connect(&self) -> Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }
}
impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Default)]
struct FakeRaucData {
    operation: String,
    slot: String,
    progress: i32,
    error: String,
    // 0 success; 1 refusal; 2 lost method reply; 3 lost Completed; 4 delayed completion.
    mode: usize,
    emit_stale_on_read: bool,
    installed: Vec<Vec<u8>>,
}
#[derive(Clone)]
struct FakeRauc(Arc<Mutex<FakeRaucData>>);
#[zbus::interface(name = "de.pengutronix.rauc.Installer")]
impl FakeRauc {
    async fn install_bundle(
        &self,
        source: &str,
        _args: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        let body = tokio::fs::read(source)
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let mode = {
            let mut d = self.0.lock().unwrap();
            if d.operation != "idle" {
                return Err(fdo::Error::Failed("busy".into()));
            }
            if d.mode == 1 {
                d.error = "signature verification failed".into();
                return Err(fdo::Error::Failed(d.error.clone()));
            }
            d.installed.push(body);
            d.operation = "installing".into();
            d.progress = 20;
            d.mode
        };
        let data = self.0.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(if mode == 4 { 1300 } else { 100 })).await;
            {
                let mut d = data.lock().unwrap();
                d.operation = "idle".into();
                d.progress = 100;
            }
            if mode != 3 {
                connection
                    .emit_signal(None::<&str>, "/", INSTALLER, "Completed", &0i32)
                    .await
                    .unwrap();
            }
        });
        if mode == 2 {
            Err(fdo::Error::NoReply(
                "method reply lost after acceptance".into(),
            ))
        } else {
            Ok(())
        }
    }
    #[zbus(property)]
    async fn operation(&self, #[zbus(connection)] connection: &Connection) -> String {
        let (operation, stale) = {
            let d = self.0.lock().unwrap();
            (
                d.operation.clone(),
                d.emit_stale_on_read && d.installed.is_empty(),
            )
        };
        if stale && operation == "idle" {
            connection
                .emit_signal(None::<&str>, "/", INSTALLER, "Completed", &0i32)
                .await
                .unwrap();
        }
        operation
    }
    #[zbus(property)]
    fn boot_slot(&self) -> String {
        self.0.lock().unwrap().slot.clone()
    }
    #[zbus(property)]
    fn progress(&self) -> (i32, String, i32) {
        (self.0.lock().unwrap().progress, "install".into(), 0)
    }
    #[zbus(property)]
    fn last_error(&self) -> String {
        self.0.lock().unwrap().error.clone()
    }
}

struct FakeHttpData {
    base: String,
    releases: Vec<Value>,
    bundles: HashMap<String, Vec<u8>>,
    sums: HashMap<String, Vec<u8>>,
    ranges: Vec<String>,
    fail_page: usize,
    cut_once: bool,
    bad_range: bool,
    ignore_range: bool,
    oversized_bundle: bool,
    redirect: Option<String>,
}
impl FakeHttpData {
    fn add(&mut self, tag: &str) {
        let body = format!("signed-bundle-{tag}").repeat(500).into_bytes();
        let sum = format!("{:x}", Sha256::digest(&body));
        let base = format!("{}/owner/repo/releases/download/{tag}", self.base);
        self.sums
            .insert(tag.into(), format!("{sum}  ./{ASSET}\n").into_bytes());
        self.releases.push(json!({"tag_name":tag,"body":format!("notes {tag}"),"draft":false,"prerelease":false,
            "published_at":"2026-09-01T10:00:00Z", "assets":[
                {"name":ASSET,"state":"uploaded","size":body.len(),"browser_download_url":format!("{base}/{ASSET}")},
                {"name":"SHA256SUMS","state":"uploaded","size":100,"browser_download_url":format!("{base}/SHA256SUMS")}
            ]}));
        self.bundles.insert(tag.into(), body);
    }
}
async fn fake_http(
    State(state): State<Arc<Mutex<FakeHttpData>>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let mut d = state.lock().unwrap();
    let path = uri.path();
    if path == "/repos/owner/repo/releases" {
        let page = uri
            .query()
            .unwrap_or("")
            .split('&')
            .find_map(|p| p.strip_prefix("page="))
            .unwrap_or("1")
            .parse::<usize>()
            .unwrap();
        if page == d.fail_page {
            return Response::builder().status(502).body(Body::empty()).unwrap();
        }
        let start = ((page - 1) * 100).min(d.releases.len());
        let end = (start + 100).min(d.releases.len());
        return Response::new(Body::from(
            serde_json::to_vec(&d.releases[start..end]).unwrap(),
        ));
    }
    if let Some(tag) = path.strip_prefix("/repos/owner/repo/releases/tags/") {
        return match d.releases.iter().find(|r| r["tag_name"] == tag) {
            Some(r) => Response::new(Body::from(serde_json::to_vec(r).unwrap())),
            None => Response::builder().status(404).body(Body::empty()).unwrap(),
        };
    }
    if let Some(asset) = path.strip_prefix("/owner/repo/releases/download/") {
        let (tag, name) = asset.split_once('/').unwrap();
        if name == "SHA256SUMS" {
            return Response::new(Body::from(d.sums.get(tag).cloned().unwrap_or_default()));
        }
        let mut body = d.bundles.get(tag).cloned().unwrap_or_default();
        if let Some(location) = &d.redirect {
            return Response::builder()
                .status(302)
                .header("Location", location)
                .body(Body::empty())
                .unwrap();
        }
        if d.cut_once {
            d.cut_once = false;
            let len = body.len();
            let prefix = body[..len / 2].to_vec();
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(prefix);
                tokio::time::sleep(Duration::from_millis(50)).await;
                yield Err(std::io::Error::other("intentional connection loss"));
            };
            return Response::builder()
                .header("Content-Length", len)
                .body(Body::from_stream(stream))
                .unwrap();
        }
        let range = headers
            .get("Range")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_string();
        d.ranges.push(range.clone());
        if d.oversized_bundle {
            body.push(b'x');
        }
        if !range.is_empty() && !d.ignore_range {
            let offset = range
                .strip_prefix("bytes=")
                .unwrap()
                .trim_end_matches('-')
                .parse::<usize>()
                .unwrap();
            let content_range = if d.bad_range {
                d.bad_range = false;
                format!("bytes 0-{}/{}", body.len() - 1, body.len())
            } else {
                format!("bytes {offset}-{}/{}", body.len() - 1, body.len())
            };
            return Response::builder()
                .status(206)
                .header("Content-Range", content_range)
                .body(Body::from(body[offset..].to_vec()))
                .unwrap();
        }
        return Response::new(Body::from(body));
    }
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap()
}

struct Fixture {
    bus: PrivateBus,
    connection: Connection,
    rauc_connection: Connection,
    rauc: FakeRauc,
    http: Arc<Mutex<FakeHttpData>>,
    http_task: tokio::task::JoinHandle<()>,
    options: Options,
    gate: Gate,
    settings: watch::Sender<Settings>,
    updater: Updater,
    acquired: Arc<Mutex<Vec<String>>>,
    released: Arc<Mutex<Vec<String>>>,
    fail_acquire: Arc<AtomicBool>,
    fail_release: Arc<AtomicBool>,
    reboots: Arc<AtomicUsize>,
}
impl Fixture {
    async fn new() -> Self {
        let bus = PrivateBus::new().await;
        let rauc = FakeRauc(Arc::new(Mutex::new(FakeRaucData {
            operation: "idle".into(),
            slot: "A".into(),
            ..FakeRaucData::default()
        })));
        let rauc_connection = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(RAUC)
            .unwrap()
            .serve_at("/", rauc.clone())
            .unwrap()
            .build()
            .await
            .unwrap();
        let connection = bus.connect().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut data = FakeHttpData {
            base: base.clone(),
            releases: vec![],
            bundles: HashMap::new(),
            sums: HashMap::new(),
            ranges: vec![],
            fail_page: 0,
            cut_once: false,
            bad_range: false,
            ignore_range: false,
            oversized_bundle: false,
            redirect: None,
        };
        data.add("v1.2.0");
        let http = Arc::new(Mutex::new(data));
        let router = Router::new()
            .fallback(get(fake_http))
            .with_state(http.clone());
        let http_task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut options = Options::from_env(true);
        options.data_dir = bus.dir.join("data");
        options.boot_health = bus.dir.join("health");
        options.update_repo = "owner/repo".into();
        options.update_asset = ASSET.into();
        options.image_version = "v1.1.0".into();
        options.github_api = base.clone();
        options.github_download = base;
        std::fs::write(&options.boot_health, "good A\n").unwrap();
        let gate = Gate::default();
        let (settings, receiver) = watch::channel(Settings::default());
        let acquired = Arc::new(Mutex::new(Vec::new()));
        let released = Arc::new(Mutex::new(Vec::new()));
        let fail_acquire = Arc::new(AtomicBool::new(false));
        let fail_release = Arc::new(AtomicBool::new(false));
        let reboots = Arc::new(AtomicUsize::new(0));
        let hooks = Self::hooks(
            &gate,
            &acquired,
            &released,
            &fail_acquire,
            &fail_release,
            &reboots,
        );
        let updater = Updater::new(
            connection.clone(),
            &options,
            Events::default(),
            gate.clone(),
            receiver,
            hooks,
        )
        .await
        .unwrap();
        connection
            .object_server()
            .at(PATH, updater.clone())
            .await
            .unwrap();
        Self {
            bus,
            connection,
            rauc_connection,
            rauc,
            http,
            http_task,
            options,
            gate,
            settings,
            updater,
            acquired,
            released,
            fail_acquire,
            fail_release,
            reboots,
        }
    }
    fn hooks(
        gate: &Gate,
        acquired: &Arc<Mutex<Vec<String>>>,
        released: &Arc<Mutex<Vec<String>>>,
        fail_acquire: &Arc<AtomicBool>,
        fail_release: &Arc<AtomicBool>,
        reboots: &Arc<AtomicUsize>,
    ) -> Hooks {
        let acquisitions = acquired.clone();
        let acq_fail = fail_acquire.clone();
        let close = gate.clone();
        let releases = released.clone();
        let rel_fail = fail_release.clone();
        let open = gate.clone();
        let boots = reboots.clone();
        Hooks {
            acquire: Arc::new(move |id| {
                let log = acquisitions.clone();
                let fail = acq_fail.clone();
                let gate = close.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(id);
                    gate.set(true);
                    if fail.load(Ordering::SeqCst) {
                        Err("remote acquisition reply lost".into())
                    } else {
                        Ok(())
                    }
                })
            }),
            release: Arc::new(move |id| {
                let log = releases.clone();
                let fail = rel_fail.clone();
                let gate = open.clone();
                Box::pin(async move {
                    log.lock().unwrap().push(id);
                    if fail.load(Ordering::SeqCst) {
                        Err("rollback incomplete".into())
                    } else {
                        gate.set(false);
                        Ok(())
                    }
                })
            }),
            reboot: Arc::new(move |_| {
                let boots = boots.clone();
                Box::pin(async move {
                    boots.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }),
        }
    }
    async fn fresh(&self, options: &Options) -> Updater {
        Updater::new(
            self.connection.clone(),
            options,
            Events::default(),
            self.gate.clone(),
            self.settings.subscribe(),
            Self::hooks(
                &self.gate,
                &self.acquired,
                &self.released,
                &self.fail_acquire,
                &self.fail_release,
                &self.reboots,
            ),
        )
        .await
        .unwrap()
    }
    async fn terminal(&self, id: &str) -> Status {
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                let s = self.updater.status();
                if s.operation_id == id && (s.state == "reboot" || !s.error.is_empty()) {
                    // Give completion recovery a chance to release its reservation.
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    return self.updater.status();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("installation never reached a terminal result")
    }
    async fn install(&self, retry: bool) -> Status {
        let id = self
            .updater
            .install("v1.2.0", "stable", false, retry)
            .await
            .unwrap();
        self.terminal(&id).await
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.http_task.abort();
    }
}

#[tokio::test]
async fn typed_bus_api_same_actor_and_client_disconnect() {
    let f = Fixture::new().await;
    f.rauc.0.lock().unwrap().mode = 4;
    let client = f.bus.connect().await;
    let proxy = Proxy::new(
        &client,
        f.connection.unique_name().unwrap().as_str(),
        PATH,
        UPDATES,
    )
    .await
    .unwrap();
    let catalogue: Vec<Release> = proxy.call("Check", &()).await.unwrap();
    assert_eq!(catalogue.len(), 1);
    assert_eq!(Release::SIGNATURE.to_string(), "(sstsssbbss)");
    assert_eq!(Status::SIGNATURE.to_string(), "(ssissbsssbbbsss)");
    let status: Status = proxy.get_property("Status").await.unwrap();
    assert_eq!(status.current, "v1.1.0");
    let id: String = proxy
        .call("Install", &("v1.2.0", "stable", false, false))
        .await
        .unwrap();
    assert_eq!(f.updater.status().operation_id, id);
    drop(proxy);
    client.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.rauc.0.lock().unwrap().installed.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(f
        .updater
        .install("v1.2.0", "stable", false, false)
        .await
        .is_err());
    // A forged Completed from another bus client cannot finish an accepted install.
    let attacker = f.bus.connect().await;
    attacker
        .emit_signal(None::<&str>, "/", INSTALLER, "Completed", &0i32)
        .await
        .unwrap();
    f.updater.check().await.unwrap();
    assert_eq!(f.updater.status().state, "installing");
    assert!(f.gate.blocked());
    let status = f.terminal(&id).await;
    assert_eq!(status.state, "reboot");
    assert_eq!(status.progress, 100);
    assert!(!status.pending_auto);
    assert!(!f.gate.blocked());
    assert_eq!(
        f.rauc.0.lock().unwrap().installed[0],
        f.http.lock().unwrap().bundles["v1.2.0"]
    );
    let acquisitions = f.acquired.lock().unwrap();
    assert_eq!(
        acquisitions
            .iter()
            .filter(|operation| *operation == &id)
            .count(),
        2
    );
    assert!(f.released.lock().unwrap().contains(&id));
}

#[tokio::test]
async fn lost_rauc_reply_still_observes_completion_and_lost_result_requires_retry() {
    let f = Fixture::new().await;
    f.rauc.0.lock().unwrap().mode = 2;
    assert_eq!(f.install(false).await.state, "reboot");
    let f = Fixture::new().await;
    f.rauc.0.lock().unwrap().mode = 3;
    let status = f.install(false).await;
    assert_eq!(status.state, "uncertain");
    assert!(status.retry_required);
    assert!(status.suspended);
    assert_eq!(
        std::fs::read_dir(f.options.data_dir.join("updates"))
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "raucb"))
            .count(),
        1
    );
    let again = f.fresh(&f.options).await;
    assert_eq!(again.status().state, "uncertain");
    assert_eq!(f.install(false).await.state, "uncertain");
    f.rauc.0.lock().unwrap().mode = 0;
    assert_eq!(f.install(true).await.state, "reboot");
}

#[tokio::test]
async fn refusal_is_durable_and_retry_is_explicit() {
    let f = Fixture::new().await;
    f.rauc.0.lock().unwrap().mode = 1;
    let status = f.install(false).await;
    assert_eq!(status.state, "error");
    assert!(!status.pending_auto);
    f.updater.check().await.unwrap();
    assert!(!f.updater.releases("stable")[0].blocked.is_empty());
    let again = f.fresh(&f.options).await;
    again.check().await.unwrap();
    assert!(!again.releases("stable")[0].blocked.is_empty());
    f.rauc.0.lock().unwrap().mode = 0;
    assert!(f.install(false).await.error.contains("blocked"));
    assert_eq!(f.install(true).await.state, "reboot");
}

#[tokio::test]
async fn interrupted_download_resumes_and_wrong_range_restarts() {
    for (bad_range, ignore_range) in [(false, false), (true, false), (false, true)] {
        let f = Fixture::new().await;
        f.http.lock().unwrap().cut_once = true;
        assert_eq!(f.install(false).await.state, "error");
        let entries: Vec<_> = std::fs::read_dir(f.options.data_dir.join("updates"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let part = entries
            .iter()
            .find(|p| p.extension().is_some_and(|x| x == "part"))
            .unwrap();
        let offset = std::fs::metadata(part).unwrap().len();
        assert!(offset > 0);
        {
            let mut d = f.http.lock().unwrap();
            d.bad_range = bad_range;
            d.ignore_range = ignore_range;
        }
        let status = f.install(false).await;
        assert!(f
            .http
            .lock()
            .unwrap()
            .ranges
            .contains(&format!("bytes={offset}-")));
        if bad_range {
            assert_eq!(status.state, "error");
            assert!(!part.exists());
            assert_eq!(f.install(false).await.state, "reboot");
        } else {
            assert_eq!(status.state, "reboot");
        }
        assert_eq!(f.rauc.0.lock().unwrap().installed.len(), 1);
    }
}

#[tokio::test]
async fn catalogue_pagination_precedence_and_previous_catalogue_on_failure() {
    let f = Fixture::new().await;
    {
        let mut d = f.http.lock().unwrap();
        for i in 0..150 {
            d.add(&format!("v0.9.{i}"));
        }
        for tag in [
            "v1.3.0-rc.2",
            "v1.3.0-rc.10",
            "v1.3.0",
            "v1.1.0+rebuild",
            "v01.5.0",
            "v1.5.0-01",
        ] {
            d.add(tag);
        }
        d.releases.last_mut().unwrap()["draft"] = json!(true);
    }
    let all = f.updater.check().await.unwrap();
    assert_eq!(all.len(), 155);
    let releases = f.updater.releases("test");
    assert_eq!(
        releases.iter().map(|r| r.tag.as_str()).collect::<Vec<_>>(),
        ["v1.3.0", "v1.3.0-rc.10", "v1.3.0-rc.2", "v1.2.0"]
    );
    assert_eq!(f.updater.releases("stable").len(), 2);
    let checked = f.updater.status().checked;
    f.http.lock().unwrap().fail_page = 2;
    assert!(f.updater.check().await.is_err());
    assert_eq!(f.updater.status().checked, checked);
    assert_eq!(f.updater.releases("test").len(), 4);
    {
        let mut d = f.http.lock().unwrap();
        d.fail_page = 0;
        for i in 0..900 {
            d.add(&format!("v0.8.{i}"));
        }
    }
    assert!(f.updater.check().await.unwrap_err().contains("incomplete"));
    assert_eq!(f.updater.releases("test").len(), 4);
}

#[tokio::test]
async fn untrusted_release_metadata_body_and_redirect_are_rejected() {
    for case in [
        "foreign-url",
        "upload",
        "duplicate",
        "oversized-sums-metadata",
        "oversized-sums-body",
        "conflicting-sums",
        "bad-checksum",
        "oversized-bundle",
        "redirect",
        "preview",
        "draft",
        "missing-bundle",
    ] {
        let f = Fixture::new().await;
        {
            let mut d = f.http.lock().unwrap();
            match case {
                "foreign-url" => {
                    d.releases[0]["assets"][0]["browser_download_url"] =
                        json!("https://evil.example/b")
                }
                "upload" => d.releases[0]["assets"][0]["state"] = json!("starter"),
                "duplicate" => {
                    let a = d.releases[0]["assets"][0].clone();
                    d.releases[0]["assets"].as_array_mut().unwrap().push(a);
                }
                "oversized-sums-metadata" => d.releases[0]["assets"][1]["size"] = json!(1 << 20),
                "oversized-sums-body" => {
                    d.sums.insert("v1.2.0".into(), vec![b' '; (64 << 10) + 1]);
                }
                "conflicting-sums" => d
                    .sums
                    .get_mut("v1.2.0")
                    .unwrap()
                    .extend_from_slice(format!("{}  {ASSET}\n", "0".repeat(64)).as_bytes()),
                "bad-checksum" => {
                    d.sums.insert(
                        "v1.2.0".into(),
                        format!("{}  {ASSET}\n", "0".repeat(64)).into_bytes(),
                    );
                }
                "oversized-bundle" => d.oversized_bundle = true,
                "redirect" => d.redirect = Some("https://evil.example/b".into()),
                "preview" => d.releases[0]["prerelease"] = json!(true),
                "draft" => d.releases[0]["draft"] = json!(true),
                "missing-bundle" => d.releases[0]["assets"] = json!([]),
                _ => unreachable!(),
            }
        }
        let s = f.install(false).await;
        assert_eq!(s.state, "error", "{case}: {s:?}");
        assert!(!s.error.is_empty(), "{case}");
        assert!(f.rauc.0.lock().unwrap().installed.is_empty(), "{case}");
    }
}

#[tokio::test]
async fn unknown_owner_keeps_gate_closed_and_failed_acquire_is_released_by_id() {
    let f = Fixture::new().await;
    f.fail_acquire.store(true, Ordering::SeqCst);
    f.fail_release.store(true, Ordering::SeqCst);
    let id = f
        .updater
        .install("v1.2.0", "stable", false, false)
        .await
        .unwrap();
    let s = f.terminal(&id).await;
    assert!(!s.error.is_empty());
    assert!(f.gate.blocked());
    assert!(f.released.lock().unwrap().contains(&id));
    f.fail_acquire.store(false, Ordering::SeqCst);
    f.fail_release.store(false, Ordering::SeqCst);
    f.updater.reconcile().await.unwrap();
    assert!(!f.gate.blocked());
    f.rauc_connection.clone().close().await.unwrap();
    assert!(f.updater.reconcile().await.is_err());
    assert!(f.gate.blocked());
    let before = f.released.lock().unwrap().len();
    assert!(f.updater.reconcile().await.is_err());
    assert_eq!(f.released.lock().unwrap().len(), before);
    let _returned_rauc = zbus::connection::Builder::address(f.bus.address.as_str())
        .unwrap()
        .name(RAUC)
        .unwrap()
        .serve_at("/", f.rauc.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    f.updater.reconcile().await.unwrap();
    assert!(!f.gate.blocked());
    assert!(f.updater.status().error.is_empty());
    assert_eq!(f.released.lock().unwrap().len(), before + 1);
}

#[tokio::test]
async fn completion_from_before_install_cannot_report_success() {
    let f = Fixture::new().await;
    {
        let mut r = f.rauc.0.lock().unwrap();
        r.mode = 3;
        r.emit_stale_on_read = true;
    }
    let status = f.install(false).await;
    assert_eq!(status.state, "uncertain");
    assert!(status.retry_required);
}

#[tokio::test]
async fn unsupported_domain_without_rauc_is_safe_and_pending_without_configuration_is_not() {
    let bus = PrivateBus::new().await;
    let connection = bus.connect().await;
    let mut options = Options::from_env(false);
    options.data_dir = bus.dir.join("data");
    options.update_repo.clear();
    options.update_asset.clear();
    options.github_api = "https://api.github.com".into();
    options.github_download = "https://github.com".into();
    let gate = Gate::default();
    gate.set(true); // parent's startup barrier
    let (_, settings) = watch::channel(Settings::default());
    let forbidden = Arc::new(AtomicUsize::new(0));
    let calls = forbidden.clone();
    let hook: device_core::update::Hook = Arc::new(move |_| {
        let c = calls.clone();
        Box::pin(async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    let hooks = Hooks {
        acquire: hook.clone(),
        release: hook.clone(),
        reboot: hook,
    };
    let updater = Updater::new(
        connection.clone(),
        &options,
        Events::default(),
        gate.clone(),
        settings.clone(),
        hooks.clone(),
    )
    .await
    .unwrap();
    assert_eq!(updater.status().state, "unsupported");
    assert!(!gate.blocked());
    assert_eq!(forbidden.load(Ordering::SeqCst), 0);
    assert!(updater.check().await.is_err());
    assert!(updater
        .install("v1.2.0", "stable", false, false)
        .await
        .is_err());
    std::fs::create_dir_all(options.data_dir.join("updates")).unwrap();
    std::fs::write(options.data_dir.join("updates/state.json"), b"{").unwrap();
    let pending = Updater::new(
        connection,
        &options,
        Events::default(),
        gate.clone(),
        settings,
        hooks,
    )
    .await
    .unwrap();
    assert!(gate.blocked());
    assert!(pending.status().suspended);
    assert_eq!(pending.status().state, "uncertain");
}

#[tokio::test]
async fn journal_health_version_and_rollback_reconciliation() {
    for (slot, version, health, expected) in [
        ("B", "v1.2.0", "", "confirming"),
        ("B", "v1.2.0", "good", "idle"),
        ("B", "v1.2.0", "stranded", "idle"),
        ("A", "v1.1.0", "good", "idle"),
        ("B", "v1.1.0", "good", "idle"),
    ] {
        let f = Fixture::new().await;
        assert_eq!(f.install(false).await.state, "reboot");
        let path = f.options.data_dir.join("updates/state.json");
        let mut journal: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        journal["pending"]["boot_id"] = json!("previous-boot");
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        f.rauc.0.lock().unwrap().slot = slot.into();
        std::fs::write(&f.options.boot_health, format!("{health} {slot}\n")).unwrap();
        let mut options = f.options.clone();
        options.image_version = version.into();
        let fresh = f.fresh(&options).await;
        let s = fresh.status();
        assert_eq!(s.state, expected, "{slot}/{version}/{health}: {s:?}");
        if health == "stranded" || version == "v1.1.0" && slot == "B" {
            assert!(s.suspended);
        }
        if slot == "A" {
            fresh.check().await.unwrap();
            assert_eq!(fresh.releases("stable")[0].blocked, "rolled back");
        }
        if slot == "B" && version == "v1.2.0" && health == "good" {
            assert!(s.last_result.contains("updated from"));
            assert!(!s.suspended);
        }
    }
}

#[tokio::test]
async fn automatic_scheduler_gates_claims_and_reboots_with_current_policy() {
    use chrono::Timelike;
    let f = Fixture::new().await;
    f.updater.check().await.unwrap();
    let hour = chrono::Utc::now().hour();
    let mut settings = Settings {
        automatic: true,
        start_hour: hour,
        end_hour: (hour + 2) % 24,
        ..Settings::default()
    };
    // Unreliable time and unconfirmed boot do not consume an automatic window.
    f.settings.send_replace(settings.clone());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(f.updater.status().last_window.is_empty());
    std::fs::write(&f.options.boot_health, "").unwrap();
    settings.time_reliable = true;
    f.settings.send_replace(settings.clone());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(f.updater.status().last_window.is_empty());
    assert!(f.rauc.0.lock().unwrap().installed.is_empty());
    // A remote acquisition failure is rolled back and also does not claim.
    std::fs::write(&f.options.boot_health, "good A\n").unwrap();
    f.fail_acquire.store(true, Ordering::SeqCst);
    f.settings.send_replace(settings.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.updater.status().last_window.is_empty());
    assert!(!f.gate.blocked());
    // Healthy boot, exact time, configured window and maintenance all pass.
    f.fail_acquire.store(false, Ordering::SeqCst);
    f.settings.send_replace(settings.clone());
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.updater.status().state != "reboot" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let status = f.updater.status();
    assert!(status.pending_auto);
    assert!(!status.last_window.is_empty());
    let raw: Value = serde_json::from_slice(
        &std::fs::read(f.options.data_dir.join("updates/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["last_window"], status.last_window);
    // Completion never reboots on its own: the next tick checks the current policy again.
    assert_eq!(f.reboots.load(Ordering::SeqCst), 0);
    settings.automatic = false;
    f.settings.send_replace(settings.clone());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(f.reboots.load(Ordering::SeqCst), 0);
    settings.automatic = true;
    settings.channel = "test".into();
    f.settings.send_replace(settings.clone());
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(f.reboots.load(Ordering::SeqCst), 0);
    settings.channel = "stable".into();
    settings.time_reliable = false;
    f.settings.send_replace(settings.clone());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(f.reboots.load(Ordering::SeqCst), 0);
    settings.time_reliable = true;
    settings.start_hour = (hour + 3) % 24;
    settings.end_hour = (hour + 4) % 24;
    f.settings.send_replace(settings.clone());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(f.reboots.load(Ordering::SeqCst), 0);
    settings.start_hour = hour;
    settings.end_hour = (hour + 2) % 24;
    f.settings.send_replace(settings.clone());
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.reboots.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    f.settings.send_replace(settings);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(f.reboots.load(Ordering::SeqCst), 1);
    assert_eq!(f.rauc.0.lock().unwrap().installed.len(), 1);
}

#[tokio::test]
async fn power_serializes_with_installation_and_refuses_unknown_rauc_and_journal() {
    let f = Fixture::new().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let action: device_core::update::Hook = Arc::new(move |_| {
        let counter = counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    f.rauc.0.lock().unwrap().mode = 4;
    let id = f
        .updater
        .install("v1.2.0", "stable", false, false)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.rauc.0.lock().unwrap().installed.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(f.updater.power(action.clone()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.terminal(&id).await.state, "reboot");
    f.updater.power(action.clone()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.rauc_connection.clone().close().await.unwrap();
    assert!(f.updater.power(action.clone()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(f.gate.blocked());

    let f = Fixture::new().await;
    f.rauc.0.lock().unwrap().mode = 3;
    assert_eq!(f.install(false).await.state, "uncertain");
    assert!(f
        .updater
        .power(action.clone())
        .await
        .unwrap_err()
        .contains("uncertain"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f.rauc.0.lock().unwrap().mode = 0;
    assert_eq!(f.install(true).await.state, "reboot");
    // The successful retry is known installed, even before health clears the prior suspension.
    assert!(f.updater.status().suspended);
    f.updater.power(action).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn accepted_power_hook_keeps_actor_guard_after_caller_disconnects() {
    let f = Fixture::new().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let (signal, wait) = (entered.clone(), finish.clone());
    let action: device_core::update::Hook = Arc::new(move |_| {
        let (signal, wait) = (signal.clone(), wait.clone());
        Box::pin(async move {
            signal.notify_one();
            wait.notified().await;
            Ok(())
        })
    });
    let updater = f.updater.clone();
    let request = tokio::spawn(async move { updater.power(action).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    request.abort();
    assert!(f
        .updater
        .install("v1.2.0", "stable", false, false)
        .await
        .is_err());
    assert!(f.gate.blocked());
    finish.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.gate.blocked() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn optional_domain_allows_power_and_releases_failed_hooks() {
    let f = Fixture::new().await;
    let mut options = f.options.clone();
    options.update_repo.clear();
    options.update_asset.clear();
    f.rauc_connection.clone().close().await.unwrap();
    let unsupported = f.fresh(&options).await;
    assert_eq!(unsupported.status().state, "unsupported");
    let calls = f.reboots.clone();
    let action: device_core::update::Hook = Arc::new(move |_| {
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    unsupported.power(action).await.unwrap();
    assert_eq!(f.reboots.load(Ordering::SeqCst), 1);
    assert!(!f.gate.blocked());
    let failing: device_core::update::Hook =
        Arc::new(move |_| Box::pin(async { Err("native power failed".into()) }));
    assert!(unsupported
        .power(failing)
        .await
        .unwrap_err()
        .contains("native power failed"));
    assert!(!f.gate.blocked());
}
