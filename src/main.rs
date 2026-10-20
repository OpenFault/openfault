use openfault::{AppState, app, db::Db, process};
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}

/// `openfault healthcheck` for the Dockerfile HEALTHCHECK (distroless has no shell or curl):
/// GET /healthz on 127.0.0.1, 2s timeout; 0 on HTTP 200, else 1.
fn healthy(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let t = std::time::Duration::from_secs(2);
    (|| -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect_timeout(&addr, t)?;
        s.set_read_timeout(Some(t))?;
        s.set_write_timeout(Some(t))?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf)?;
        Ok(buf.starts_with(b"HTTP/1.1 200") || buf.starts_with(b"HTTP/1.0 200"))
    })()
    .unwrap_or(false)
}

fn healthcheck(port: u16) -> i32 {
    if healthy(port) {
        return 0;
    }
    eprintln!("healthcheck failed: http://127.0.0.1:{port}/healthz");
    1
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if std::env::args().nth(1).as_deref() == Some("relay") {
        return openfault::relay::run().await;
    }
    let port: u16 = std::env::var("OPENFAULT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(healthcheck(port));
    }
    let data = PathBuf::from(std::env::var("OPENFAULT_DATA").unwrap_or_else(|_| "./data".into()));
    std::fs::create_dir_all(&data).expect("create OPENFAULT_DATA");
    let path = data.join("openfault.db");
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("backup" | "restore")) {
        let up = args[0] == "restore" && healthy(port);
        std::process::exit(openfault::backup::cli(&args, &path, up).await);
    }
    let db = Db::open(&path).unwrap_or_else(|e| {
        eprintln!("cannot open database {}: {e}", path.display());
        std::process::exit(1);
    });
    // ponytail: dev-only seed (project "dev" with a fixed DSN key); release builds use the /setup wizard.
    if cfg!(debug_assertions)
        && let Ok(key) = std::env::var("OPENFAULT_DEV_DSN_KEY")
    {
        db.write(move |c| openfault::db::seed_dev(c, &key))
            .await
            .expect("seed dev project");
    }
    let state = AppState::new(db.clone());
    // no users yet → logs the one-time setup token (src/setup.rs).
    if let Err(e) = openfault::setup::ensure(&state).await {
        tracing::error!("setup token: {e}");
    }
    tokio::spawn(process::run(db.clone(), state.notify.clone()));
    tokio::spawn(openfault::notify::run(db.clone()));
    tokio::spawn(openfault::backup::scheduler(db.clone()));
    tokio::spawn(process::housekeeping(db));
    let listener = openfault::listen(port);
    tracing::info!("listening on :{port}");
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .expect("serve");
}
