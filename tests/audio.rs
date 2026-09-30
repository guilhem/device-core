use device_core::{
    audio::{Audio, Status},
    common::{Events, Gate, SERVICE},
    options::Options,
};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Stdio},
    time::Duration,
};
use zbus::{Connection, Proxy};
struct Files(PathBuf);
impl Files {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "device-core-audio-{}",
            device_core::common::token().unwrap()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("audio.wav"), b"test").unwrap();
        std::fs::write(dir.join("audio.mp3"), b"test").unwrap();
        Self(dir)
    }
    fn source(&self) -> String {
        self.0.join("audio.wav").to_str().unwrap().into()
    }
    fn options(&self) -> Options {
        let mut options = Options::from_env(true);
        options.audio_roots = vec![self.0.clone()];
        options.sim_audio_ms = 10000;
        options
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Bus {
    child: Child,
    address: String,
}
impl Bus {
    fn new() -> Self {
        let mut child = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            address: address.trim().into(),
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
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn targeted_preemption_wait_restart_and_maintenance() {
    let files = Files::new();
    let gate = Gate::default();
    let events = Events::default();
    let mut rx = events.0.subscribe();
    let audio = Audio::new(files.options(), gate.clone(), events).unwrap();
    let first = audio.start("file", &files.source()).await.unwrap();
    let wait_first = {
        let audio = audio.clone();
        let id = first.clone();
        tokio::spawn(async move { audio.wait(&id).await.unwrap() })
    };
    let second = audio.start("file", &files.source()).await.unwrap();
    assert_ne!(first, second);
    assert_eq!(wait_first.await.unwrap(), "preempted");
    audio.stop(&first).await.unwrap();
    assert_eq!(audio.status().id, second);
    assert_eq!(audio.status().state, "playing");
    gate.set(true);
    assert!(audio.start("file", &files.source()).await.is_err());
    assert_eq!(audio.status().id, second);
    audio.stop(&second).await.unwrap();
    assert_eq!(audio.wait(&second).await.unwrap(), "stopped");
    gate.set(false);
    audio.set_volume(42).await.unwrap();
    assert_eq!(audio.status().volume, 42);
    assert!(audio.set_volume(101).await.is_err());
    assert!(rx.try_recv().is_ok());
    let fresh = Audio::new(files.options(), gate, Events::default()).unwrap();
    let third = fresh.start("file", &files.source()).await.unwrap();
    assert_ne!(third, first);
    assert!(fresh.stop(&first).await.is_err());
    assert_eq!(fresh.status().id, third);
    fresh.stop(&third).await.unwrap();
    assert!(audio.wait("foreign").await.is_err());
    let mut options = files.options();
    options.sim_audio_ms = 1;
    let quick = Audio::new(options, Gate::default(), Events::default()).unwrap();
    let id = quick.start("file", &files.source()).await.unwrap();
    assert_eq!(quick.wait(&id).await.unwrap(), "completed");
    assert_eq!(quick.status().state, "idle");
}

#[tokio::test]
async fn source_boundaries_and_fd_input_are_safe_without_host_mutation() {
    let files = Files::new();
    let outside = Files::new();
    let audio = Audio::new(files.options(), Gate::default(), Events::default()).unwrap();
    std::os::unix::fs::symlink(outside.0.join("audio.wav"), files.0.join("escape.wav")).unwrap();
    for source in [
        outside.source(),
        files.0.join("escape.wav").to_str().unwrap().into(),
        "/etc/passwd".into(),
    ] {
        assert!(audio.start("file", &source).await.is_err());
    }
    for url in [
        "https://127.0.0.1/x",
        "http://example.com/x",
        "http://localhost/x",
        "http://127.0.0.1@evil.test/x",
        "http://user@127.0.0.1/x",
        "http://127.0.0.1/x#bad",
        "file:///tmp/x",
        "http://192.168.1.1/x",
    ] {
        assert!(audio.start("stream", url).await.is_err(), "{url}");
    }
    for url in [
        "http://127.0.0.1:9/live",
        "http://[::1]:9/live",
        "http://127.10.2.1:9/live",
    ] {
        let id = audio.start("stream", url).await.unwrap();
        audio.stop(&id).await.unwrap();
    }
    assert!(audio.start("unknown", &files.source()).await.is_err());
}

#[tokio::test]
async fn private_bus_owner_cleanup_preserves_http_and_other_owner_playback() {
    use futures_util::StreamExt;
    let files = Files::new();
    let bus = Bus::new();
    let server = bus.connect().await;
    server.request_name(SERVICE).await.unwrap();
    let audio = Audio::new(files.options(), Gate::default(), Events::default()).unwrap();
    audio.serve(&server).await.unwrap();
    let dbus = zbus::fdo::DBusProxy::new(&server).await.unwrap();
    let mut signals = dbus.receive_name_owner_changed().await.unwrap();
    let cleanup_audio = audio.clone();
    let cleanup = tokio::spawn(async move {
        while let Some(signal) = signals.next().await {
            let args = signal.args().unwrap();
            if args.new_owner().is_none() {
                cleanup_audio.owner_lost(args.name().as_str()).await;
            }
        }
    });
    let client = bus.connect().await;
    let proxy = zbus::proxy::Builder::<Proxy<'_>>::new(&client)
        .destination(SERVICE)
        .unwrap()
        .path(device_core::audio::PATH)
        .unwrap()
        .interface(device_core::audio::INTERFACE)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let xml = proxy.introspect().await.unwrap();
    assert!(xml.contains("name=\"Wait\"") && xml.contains("name=\"Changed\""));
    let first: String = proxy
        .call("Start", &("file", files.source()))
        .await
        .unwrap();
    assert_eq!(
        proxy.get_property::<Status>("Status").await.unwrap().id,
        first
    );
    drop(proxy);
    client.close().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), audio.wait(&first))
            .await
            .unwrap()
            .unwrap(),
        "owner-lost"
    );
    let client = bus.connect().await;
    let owner = client.unique_name().unwrap().to_string();
    let proxy = zbus::proxy::Builder::<Proxy<'_>>::new(&client)
        .destination(SERVICE)
        .unwrap()
        .path(device_core::audio::PATH)
        .unwrap()
        .interface(device_core::audio::INTERFACE)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let second: String = proxy
        .call("Start", &("file", files.source()))
        .await
        .unwrap();
    let http = audio.start("file", &files.source()).await.unwrap();
    assert_eq!(audio.wait(&second).await.unwrap(), "preempted");
    audio.owner_lost(&owner).await;
    assert_eq!(audio.status().id, http);
    audio.stop(&http).await.unwrap();
    cleanup.abort();
}

/// Isolate PATH in a subprocess: these decoder fixtures never open ALSA, and
/// concurrently running simulation tests keep their normal environment.
#[tokio::test]
async fn opened_fd_process_preemption_and_stream_redirect_rejection() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if std::env::var_os("DEVICE_CORE_DECODER_CHECK").is_none() {
        use std::os::unix::fs::PermissionsExt;
        let files = Files::new();
        let decoder = r#"#!/usr/bin/python3
import os, sys, time
assert sys.argv[-1] == '-'
data = sys.stdin.buffer.read()
with open(os.environ['DEVICE_CORE_FAKE_CAPTURE'], 'ab') as out:
    out.write(str(os.getpid()).encode() + b':' + data + b'\n')
    out.flush()
if data == b'long':
    time.sleep(30)
"#;
        for name in ["aplay", "mpg123"] {
            let path = files.0.join(name);
            std::fs::write(&path, decoder).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let result = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "opened_fd_process_preemption_and_stream_redirect_rejection",
                "--nocapture",
            ])
            .env("DEVICE_CORE_DECODER_CHECK", "1")
            .env("PATH", &files.0)
            .env("DEVICE_CORE_FAKE_CAPTURE", files.0.join("capture"))
            // A stream must not consult proxy env vars.
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("http_proxy", "http://127.0.0.1:1")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let files = Files::new();
    let capture = PathBuf::from(std::env::var_os("DEVICE_CORE_FAKE_CAPTURE").unwrap());
    let mut options = files.options();
    options.simulate = false;
    let audio = Audio::new(options, Gate::default(), Events::default()).unwrap();
    std::fs::write(files.0.join("audio.wav"), b"original").unwrap();
    let first = audio.start("file", &files.source()).await.unwrap();
    // Spawn has not been polled yet on this current-thread runtime.
    std::fs::remove_file(files.0.join("audio.wav")).unwrap();
    std::fs::write(files.0.join("audio.wav"), b"replaced").unwrap();
    assert_eq!(audio.wait(&first).await.unwrap(), "completed");
    assert!(std::fs::read_to_string(&capture)
        .unwrap()
        .contains(":original\n"));
    assert!(!std::fs::read_to_string(&capture)
        .unwrap()
        .contains("replaced"));
    std::fs::write(files.0.join("audio.wav"), b"long").unwrap();
    let long = audio.start("file", &files.source()).await.unwrap();
    let pid = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let text = std::fs::read_to_string(&capture).unwrap_or_default();
            if let Some(line) = text.lines().find(|line| line.ends_with(":long")) {
                break line.split(':').next().unwrap().parse::<i32>().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let next = audio
        .start("file", files.0.join("audio.mp3").to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(audio.wait(&long).await.unwrap(), "preempted");
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "old decoder was not reaped before Start returned"
    );
    assert_eq!(audio.wait(&next).await.unwrap(), "completed");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{address}/forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        drop(stream);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "redirect was followed"
        );
    });
    let id = audio
        .start("stream", &format!("http://{address}/audio"))
        .await
        .unwrap();
    assert_eq!(audio.wait(&id).await.unwrap(), "failed");
    server.await.unwrap();
}
