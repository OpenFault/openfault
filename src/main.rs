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
    if std::env::var("OPENFAULT_SMTP_URL").is_ok() && std::env::var("OPENFAULT_BASE_URL").is_err() {
        eprintln!(
            "OPENFAULT_BASE_URL is required when OPENFAULT_SMTP_URL is set (links in emails must not come from the Host header)"
        );
        std::process::exit(1);
    }
    let data = PathBuf::from(std::env::var("OPENFAULT_DATA").unwrap_or_else(|_| "./data".into()));
    let port: u16 = std::env::var("OPENFAULT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    std::fs::create_dir_all(&data).expect("create OPENFAULT_DATA");
    let path = data.join("openfault.db");
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
    tokio::spawn(process::run(db.clone(), state.notify.clone()));
    tokio::spawn(openfault::notify::run(db.clone()));
    tokio::spawn(process::housekeeping(db));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind");
    tracing::info!("listening on :{port}");
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .expect("serve");
}
