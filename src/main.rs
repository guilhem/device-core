use device_core::{http, options::Options, runtime};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("device-core {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|arg| arg != "--simulate") {
        return Err("usage: device-core [--simulate | --version]".into());
    }
    let options = Options::from_env(args.iter().any(|arg| arg == "--simulate"));
    let listener = match options.http_addr.as_ref() {
        Some(address) => Some(tokio::net::TcpListener::bind(address).await?),
        None => None,
    };
    let app = runtime::start(options).await?;
    if let Some(listener) = listener {
        axum::serve(listener, http::router(app))
            .with_graceful_shutdown(shutdown())
            .await?;
    } else {
        shutdown().await;
    }
    Ok(())
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = term.recv() => () }
}
