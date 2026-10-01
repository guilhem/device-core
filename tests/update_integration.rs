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

// Real RAUC manifests; install and systemd effects remain private-bus simulations.
fn manual_bundle(dir: &std::path::Path, version: &str) -> std::fs::File {
    let base = dir.join(format!("manual-fixture-{}", token().unwrap()));
    let content = base.join("content");
    std::fs::create_dir_all(&content).unwrap();
    std::fs::write(content.join("manifest.raucm"), format!(
        "[update]\ncompatible=test-board\nversion={version}\n\n[bundle]\nformat=verity\n\n[image.rootfs]\nfilename=rootfs.img\n"
    )).unwrap();
    let bytes: Vec<u8> = (0..16384).map(|_| fastrand::u8(..)).collect();
    std::fs::write(content.join("rootfs.img"), bytes).unwrap();
    let cert = base.join("cert.pem");
    let key = base.join("key.pem");
    let generated = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=device-core-test",
        ])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .expect("openssl required for manual RAUC checks");
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let path = base.join("misleading-filename-v999.raucb");
    let generated = Command::new("rauc")
        .arg("bundle")
        .arg(format!("--cert={}", cert.display()))
        .arg(format!("--key={}", key.display()))
        .arg("--mksquashfs-args=-processors 1 -comp gzip")
        .arg(content)
        .arg(&path)
        .output()
        .expect("RAUC (with verity/JSON support) and mksquashfs required for manual bundle checks");
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    std::fs::File::open(path).unwrap()
}

impl Fixture {
    async fn refresh(&mut self) {
        self.updater = self.fresh(&self.options).await;
        self.connection
            .object_server()
            .remove::<Updater, _>(PATH)
            .await
            .unwrap();
        self.connection
            .object_server()
            .at(PATH, self.updater.clone())
            .await
            .unwrap();
    }
    async fn manual(&self, version: &str, bypass: bool, retry: bool) -> Status {
        use std::os::fd::AsFd;
        let file = manual_bundle(&self.bus.dir, version);
        let id = self
            .updater
            .install_bundle(file.as_fd().into(), bypass, retry)
            .await
            .unwrap();
        drop(file);
        self.terminal(&id).await
    }
}

#[derive(Default)]
struct PrepareData {
    input: PathBuf,
    output: PathBuf,
    jobs: Vec<bool>,
    active: bool,
    fail_start: bool,
    fail_stop: bool,
}
#[derive(Clone)]
struct FakePrepare(Arc<Mutex<PrepareData>>);
impl FakePrepare {
    async fn job(
        &self,
        unit: &str,
        start: bool,
        connection: &Connection,
    ) -> fdo::Result<zbus::zvariant::OwnedObjectPath> {
        assert_eq!(unit, "manual-prepare.service");
        let (id, result) = {
            let mut d = self.0.lock().unwrap();
            d.jobs.push(start);
            if !start && d.fail_stop && d.active {
                return Err(fdo::Error::Failed("helper cleanup failed".into()));
            }
            let failed = start && d.fail_start;
            if !failed {
                if start {
                    std::fs::copy(&d.input, &d.output).unwrap();
                    d.active = true;
                } else {
                    match std::fs::remove_file(&d.output) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => panic!("{e}"),
                    }
                    d.active = false;
                }
            }
            (d.jobs.len() as u32, if failed { "failed" } else { "done" })
        };
        let path = zbus::zvariant::OwnedObjectPath::try_from(format!(
            "/org/freedesktop/systemd1/job/{id}"
        ))
        .unwrap();
        let emitted = path.clone();
        let unit = unit.to_string();
        let connection = connection.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            connection
                .emit_signal(
                    None::<&str>,
                    "/org/freedesktop/systemd1",
                    "org.freedesktop.systemd1.Manager",
                    "JobRemoved",
                    &(id, emitted, unit, result),
                )
                .await
                .unwrap();
        });
        Ok(path)
    }
}
#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl FakePrepare {
    fn subscribe(&self) {}
    async fn start_unit(
        &self,
        unit: &str,
        mode: &str,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<zbus::zvariant::OwnedObjectPath> {
        assert_eq!(mode, "replace");
        self.job(unit, true, connection).await
    }
    async fn stop_unit(
        &self,
        unit: &str,
        mode: &str,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<zbus::zvariant::OwnedObjectPath> {
        assert_eq!(mode, "replace");
        self.job(unit, false, connection).await
    }
}
async fn prepare_manager(f: &mut Fixture) -> (Connection, FakePrepare) {
    let output = f.bus.dir.join("root-owned-output.raucb");
    let prepare = FakePrepare(Arc::new(Mutex::new(PrepareData {
        input: f.options.data_dir.join("updates/manual.raucb"),
        output: output.clone(),
        ..PrepareData::default()
    })));
    let connection = zbus::connection::Builder::address(f.bus.address.as_str())
        .unwrap()
        .name("org.freedesktop.systemd1")
        .unwrap()
        .serve_at("/org/freedesktop/systemd1", prepare.clone())
        .unwrap()
        .build()
        .await
        .unwrap();
    f.options.update_prepare_unit = "manual-prepare.service".into();
    f.options.update_prepared_bundle = output;
    (connection, prepare)
}

#[tokio::test]
async fn manual_wire_fd_survives_disconnect_is_offline_and_uses_real_dev_manifest() {
    use std::io::{Seek, SeekFrom};
    use std::os::fd::AsFd;
    let mut f = Fixture::new().await;
    assert!(f.options.update_prepare_unit.is_empty());
    assert!(f.options.update_prepared_bundle.as_os_str().is_empty());
    f.options.update_repo.clear();
    f.options.update_asset.clear();
    f.options.image_version = "dev-test".into(); // Same version is intentionally allowed.
    f.refresh().await;
    assert_eq!(f.updater.status().state, "idle");
    assert!(!f.updater.configured());
    assert!(f.updater.check().await.is_err());
    f.rauc.0.lock().unwrap().mode = 4;
    let mut file = manual_bundle(&f.bus.dir, "dev-test");
    file.seek(SeekFrom::End(0)).unwrap();
    let expected = {
        use std::os::unix::fs::FileExt;
        let mut bytes = vec![0; file.metadata().unwrap().len() as usize];
        file.read_exact_at(&mut bytes, 0).unwrap();
        bytes
    };
    let client = f.bus.connect().await;
    let proxy = Proxy::new(
        &client,
        f.connection.unique_name().unwrap().as_str(),
        PATH,
        UPDATES,
    )
    .await
    .unwrap();
    let introspection = Proxy::new(
        &client,
        f.connection.unique_name().unwrap().as_str(),
        PATH,
        "org.freedesktop.DBus.Introspectable",
    )
    .await
    .unwrap();
    let xml: String = introspection.call("Introspect", &()).await.unwrap();
    let method = xml
        .split("<method name=\"InstallBundle\">")
        .nth(1)
        .unwrap()
        .split("</method>")
        .next()
        .unwrap();
    assert!(method.contains("type=\"h\""));
    assert_eq!(method.matches("type=\"b\"").count(), 2);
    let id: String = proxy
        .call(
            "InstallBundle",
            &(zbus::zvariant::Fd::from(file.as_fd()), false, false),
        )
        .await
        .unwrap();
    assert_eq!(file.stream_position().unwrap(), expected.len() as u64);
    drop(file);
    drop(proxy);
    client.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.rauc.0.lock().unwrap().installed.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let peer = f.options.data_dir.join("updates/online-peer.part");
    std::fs::write(&peer, b"online resume bytes").unwrap();
    let another = manual_bundle(&f.bus.dir, "v1.0.0");
    assert!(f
        .updater
        .install_bundle(another.as_fd().into(), false, false)
        .await
        .unwrap_err()
        .contains("in progress"));
    assert!(f
        .updater
        .power(Arc::new(|_| Box::pin(async { Ok(()) })))
        .await
        .is_err());
    let s = f.terminal(&id).await;
    assert_eq!(s.state, "reboot", "{s:?}");
    assert_eq!(s.target, "dev-test");
    assert!(!s.pending_auto);
    assert_eq!(f.reboots.load(Ordering::SeqCst), 0);
    assert_eq!(f.rauc.0.lock().unwrap().installed[0], expected);
    assert_eq!(std::fs::read(peer).unwrap(), b"online resume bytes");
    assert!(!f.options.data_dir.join("updates/manual.raucb").exists());
    let journal: Value = serde_json::from_slice(
        &std::fs::read(f.options.data_dir.join("updates/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(journal["pending"]["local"], true);
    assert_eq!(journal["pending"]["automatic"], false);
    assert_eq!(journal["pending"]["tag"], "dev-test");
    assert_eq!(
        journal["pending"]["sha256"],
        format!("{:x}", Sha256::digest(&expected))
    );
    let recovered = f.fresh(&f.options).await;
    assert_eq!(recovered.status().state, "reboot");
    assert!(!recovered.status().retry_required); // Non-SemVer local journal was retained.
    let retry = f.manual("dev-test", false, true).await;
    assert!(retry.error.contains("restart to finish"));
}

#[tokio::test]
async fn manual_fd_refusals_and_certificate_bypass_defaults_off() {
    use std::os::fd::AsFd;
    let f = Fixture::new().await;
    let client = f.bus.connect().await;
    let proxy = Proxy::new(
        &client,
        f.connection.unique_name().unwrap().as_str(),
        PATH,
        UPDATES,
    )
    .await
    .unwrap();
    let path = f.bus.dir.join("input");
    std::fs::write(&path, b"valid descriptor").unwrap();
    let regular = std::fs::File::open(&path).unwrap();
    assert!(proxy
        .call::<_, _, String>(
            "InstallBundle",
            &(zbus::zvariant::Fd::from(regular.as_fd()), true, false)
        )
        .await
        .is_err());
    let directory = std::fs::File::open(&f.bus.dir).unwrap();
    let write_only = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let (socket, _) = std::os::unix::net::UnixStream::pair().unwrap();
    for fd in [directory.as_fd(), write_only.as_fd(), socket.as_fd()] {
        assert!(proxy
            .call::<_, _, String>(
                "InstallBundle",
                &(zbus::zvariant::Fd::from(fd), false, false)
            )
            .await
            .is_err());
    }
    for size in [0, (2u64 << 30) + 1] {
        write_only.set_len(size).unwrap();
        assert!(proxy
            .call::<_, _, String>(
                "InstallBundle",
                &(zbus::zvariant::Fd::from(regular.as_fd()), false, false)
            )
            .await
            .is_err());
    }
    assert!(f.rauc.0.lock().unwrap().installed.is_empty());
    assert!(!f.options.data_dir.join("updates/manual.raucb").exists());
    assert!(!f.gate.blocked());
}

#[tokio::test]
async fn manual_downgrade_and_local_journal_reconcile_exact_version_and_slot() {
    for (slot, version, health, updated) in [
        ("B", "dev-test", "good", true),
        ("B", "other-dev", "good", false),
        ("A", "v1.1.0", "good", false),
    ] {
        let f = Fixture::new().await;
        assert_eq!(f.manual("dev-test", false, false).await.state, "reboot");
        let path = f.options.data_dir.join("updates/state.json");
        let mut j: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        j["pending"]["boot_id"] = json!("previous-boot");
        std::fs::write(path, serde_json::to_vec(&j).unwrap()).unwrap();
        f.rauc.0.lock().unwrap().slot = slot.into();
        std::fs::write(&f.options.boot_health, format!("{health} {slot}\n")).unwrap();
        let mut options = f.options.clone();
        options.image_version = version.into();
        options.update_repo.clear();
        options.update_asset.clear();
        let recovered = f.fresh(&options).await;
        assert_eq!(
            recovered.status().last_result.contains("updated from"),
            updated
        );
        assert_eq!(recovered.status().state, "idle");
    }
    let f = Fixture::new().await;
    assert_eq!(f.manual("v1.0.0", false, false).await.state, "reboot");
}

#[tokio::test]
async fn manual_helper_lifecycle_keeps_normal_installs_signed_and_recovers_orphans() {
    let mut f = Fixture::new().await;
    let (_manager, helper) = prepare_manager(&mut f).await;
    // Startup must stop a helper from an earlier daemon before deleting its input.
    std::fs::create_dir_all(f.options.data_dir.join("updates")).unwrap();
    std::fs::write(
        f.options.data_dir.join("updates/manual.raucb"),
        b"orphan input",
    )
    .unwrap();
    std::fs::write(&f.options.update_prepared_bundle, b"orphan output").unwrap();
    helper.0.lock().unwrap().active = true;
    f.refresh().await;
    assert_eq!(helper.0.lock().unwrap().jobs, [false]);
    assert!(!f.options.update_prepared_bundle.exists());
    assert!(!f.options.data_dir.join("updates/manual.raucb").exists());
    let s = f.manual("dev-test", true, false).await;
    assert_eq!(s.state, "reboot", "{s:?}");
    assert_eq!(helper.0.lock().unwrap().jobs, [false, false, true, false]);
    assert!(!helper.0.lock().unwrap().active);
    assert!(!f.options.update_prepared_bundle.exists());
    assert!(!f.gate.blocked());
    let mut normal = Fixture::new().await;
    let (_manager, helper) = prepare_manager(&mut normal).await;
    normal.refresh().await;
    helper.0.lock().unwrap().jobs.clear();
    normal.rauc.0.lock().unwrap().mode = 1;
    let status = normal.manual("dev-test", false, false).await;
    assert!(status.error.contains("signature verification failed"));
    assert!(helper.0.lock().unwrap().jobs.is_empty()); // Opt-in is per installation.
}

#[tokio::test]
async fn manual_helper_failures_keep_resources_until_rauc_and_cleanup_are_known() {
    let mut f = Fixture::new().await;
    let (_manager, helper) = prepare_manager(&mut f).await;
    f.refresh().await;
    helper.0.lock().unwrap().fail_start = true;
    let s = f.manual("dev-test", true, false).await;
    assert!(s.error.contains("job: failed"), "{s:?}");
    assert!(!f.gate.blocked());
    assert!(f.rauc.0.lock().unwrap().installed.is_empty());
    helper.0.lock().unwrap().fail_start = false;
    helper.0.lock().unwrap().fail_stop = true;
    let s = f.manual("dev-test", true, false).await;
    assert_eq!(s.state, "reboot");
    assert!(s.error.contains("helper cleanup failed"), "{s:?}");
    assert!(f.gate.blocked());
    assert!(f.options.data_dir.join("updates/manual.raucb").exists());
    assert!(f.options.update_prepared_bundle.exists());
    helper.0.lock().unwrap().fail_stop = false;
    f.updater.reconcile().await.unwrap();
    assert!(!f.gate.blocked());
    assert!(!f.options.update_prepared_bundle.exists());
}

#[tokio::test]
async fn manual_rauc_outcomes_retry_busy_and_unknown_owner_retain_helper() {
    for mode in [1, 2, 3] {
        let mut f = Fixture::new().await;
        let (_manager, helper) = prepare_manager(&mut f).await;
        f.refresh().await;
        f.rauc.0.lock().unwrap().mode = mode;
        let s = f.manual("dev-test", true, false).await;
        if mode == 2 {
            assert_eq!(s.state, "reboot");
        } else {
            assert!(!s.error.is_empty(), "{s:?}");
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while f.gate.blocked() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(helper.0.lock().unwrap().jobs, [false, false, true, false]);
        assert!(!f.gate.blocked());
        if mode == 1 || mode == 3 {
            assert!(s.retry_required);
            let blocked = f.manual("dev-test", true, false).await;
            assert!(blocked.error.contains("retry explicitly"));
            f.rauc.0.lock().unwrap().mode = 0;
            assert_eq!(f.manual("dev-test", true, true).await.state, "reboot");
        }
    }
    let mut f = Fixture::new().await;
    let (_manager, helper) = prepare_manager(&mut f).await;
    f.refresh().await;
    f.rauc.0.lock().unwrap().operation = "installing".into();
    let busy = f.manual("dev-test", true, true).await;
    assert!(busy.error.contains("busy"));
    assert_eq!(helper.0.lock().unwrap().jobs, [false]);
    f.rauc.0.lock().unwrap().operation = "idle".into();
    f.updater.reconcile().await.unwrap();
    f.rauc.0.lock().unwrap().mode = 4;
    use std::os::fd::AsFd;
    let file = manual_bundle(&f.bus.dir, "dev-test");
    let id = f
        .updater
        .install_bundle(file.as_fd().into(), true, false)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while f.rauc.0.lock().unwrap().installed.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    f.rauc_connection.clone().close().await.unwrap();
    let s = f.terminal(&id).await;
    assert_eq!(s.state, "uncertain");
    assert_eq!(helper.0.lock().unwrap().jobs, [false, false, true]);
    assert!(f.gate.blocked());
    assert!(f.options.update_prepared_bundle.exists());
    assert!(f.options.data_dir.join("updates/manual.raucb").exists());
}

#[tokio::test]
async fn activatable_native_rauc_is_started_before_initial_recovery() {
    use futures_util::StreamExt;
    // Native service activation uses its own disposable bus and slots, never the host bus.
    let dir = std::env::temp_dir().join(format!("rauc-activation-{}", token().unwrap()));
    std::fs::create_dir(&dir).unwrap();
    let services = dir.join("services");
    std::fs::create_dir(&services).unwrap();
    let address = format!("unix:path={}", dir.join("bus").display());
    let config = dir.join("rauc.conf");
    std::fs::write(&config, format!(
        "[system]\ncompatible=activation-test\nbootloader=noop\ndata-directory={}\n\n[slot.rootfs.0]\ndevice={}\ntype=ext4\nbootname=A\n\n[slot.rootfs.1]\ndevice={}\ntype=ext4\nbootname=B\n",
        dir.join("rauc-data").display(), dir.join("slot-a.img").display(), dir.join("slot-b.img").display()
    )).unwrap();
    let executable = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|path| path.join("rauc"))
        .find(|path| path.is_file())
        .expect("native RAUC required");
    std::fs::write(
        services.join("de.pengutronix.rauc.service"),
        format!(
            "[D-BUS Service]\nName={RAUC}\nExec={} --conf={} --override-boot-slot=A service\n",
            executable.display(),
            config.display()
        ),
    )
    .unwrap();
    let bus_config = dir.join("bus.conf");
    std::fs::write(&bus_config, format!(
        "<busconfig><type>session</type><listen>{address}</listen><servicedir>{}</servicedir><auth>EXTERNAL</auth><policy context=\"default\"><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow eavesdrop=\"true\"/></policy></busconfig>", services.display()
    )).unwrap();
    let child = Command::new("dbus-daemon")
        .arg("--nofork")
        .arg(format!("--config-file={}", bus_config.display()))
        .env("DBUS_SYSTEM_BUS_ADDRESS", &address)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let bus = PrivateBus {
        child,
        dir,
        address,
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !bus.dir.join("bus").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let connection = bus.connect().await;
    let proxy = Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .unwrap();
    assert!(!proxy
        .call::<_, _, bool>("NameHasOwner", &(RAUC,))
        .await
        .unwrap());
    assert!(proxy
        .call::<_, _, Vec<String>>("ListActivatableNames", &())
        .await
        .unwrap()
        .iter()
        .any(|name| name == RAUC));
    let monitor = bus.connect().await;
    Proxy::new(
        &monitor,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus.Monitoring",
    )
    .await
    .unwrap()
    .call::<_, _, ()>(
        "BecomeMonitor",
        &(
            vec!["type='method_call',interface='org.freedesktop.DBus'"],
            0u32,
        ),
    )
    .await
    .unwrap();
    let activation_count = Arc::new(AtomicUsize::new(0));
    let calls = activation_count.clone();
    let mut stream = zbus::MessageStream::from(&monitor);
    let observation = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            if message
                .header()
                .member()
                .is_some_and(|member| member.as_str() == "GetId")
            {
                break; // Receive-order barrier after the constructors and recovery below.
            }
            if message
                .header()
                .member()
                .is_some_and(|member| member.as_str() == "StartServiceByName")
            {
                if let Ok((name, _)) = message.body().deserialize::<(String, u32)>() {
                    if name == RAUC {
                        calls.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
    });
    let mut options = Options::from_env(true);
    options.update_repo.clear();
    options.update_asset.clear();
    options.update_prepare_unit.clear();
    options.data_dir = bus.dir.join("data");
    options.boot_health = bus.dir.join("health");
    std::fs::write(&options.boot_health, "good A\n").unwrap();
    let gate = Gate::default();
    gate.set(true);
    let (_, settings) = watch::channel(Settings::default());
    let acquired = Arc::new(Mutex::new(vec![]));
    let released = Arc::new(Mutex::new(vec![]));
    let hooks = Fixture::hooks(
        &gate,
        &acquired,
        &released,
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicBool::new(false)),
        &Arc::new(AtomicUsize::new(0)),
    );
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
    let owner: String = proxy
        .call("GetNameOwner", &(RAUC,))
        .await
        .expect("initial availability must activate RAUC");
    let pid: u32 = proxy
        .call("GetConnectionUnixProcessID", &(&owner,))
        .await
        .unwrap();
    struct ActivatedRauc(u32);
    impl Drop for ActivatedRauc {
        fn drop(&mut self) {
            // SAFETY: PID came from the unique RAUC owner on this disposable test bus.
            unsafe {
                libc::kill(self.0 as i32, libc::SIGTERM);
            }
        }
    }
    let native = ActivatedRauc(pid);
    assert_eq!(updater.status().state, "idle", "{:?}", updater.status());
    assert!(!gate.blocked());
    assert!(!updater.configured());
    let another = Updater::new(
        connection.clone(),
        &options,
        Events::default(),
        gate.clone(),
        settings,
        hooks,
    )
    .await
    .unwrap();
    assert_eq!(another.status().state, "idle");
    assert_eq!(
        proxy
            .call::<_, _, String>("GetNameOwner", &(RAUC,))
            .await
            .unwrap(),
        owner
    );
    drop(native);
    tokio::time::timeout(Duration::from_secs(2), async {
        while proxy
            .call::<_, _, bool>("NameHasOwner", &(RAUC,))
            .await
            .unwrap()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(updater.reconcile().await.is_err());
    assert!(gate.blocked());
    proxy.call::<_, _, String>("GetId", &()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), observation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        activation_count.load(Ordering::SeqCst),
        1,
        "already owned RAUC must not be activated again"
    );
}
