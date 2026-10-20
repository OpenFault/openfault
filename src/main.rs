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
    let data = PathBuf::from(std::env::var("OPENFAULT_DATA").unwrap_or_else(|_| "./data".into()));
    let port: u16 = std::env::var("OPENFAULT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    std::fs::create_dir_all(&data).expect("create OPENFAULT_DATA");
    let db = Db::open(&data.join("openfault.db")).expect("open database");
    if let Ok(key) = std::env::var("OPENFAULT_DEV_DSN_KEY") {
        db.write(move |c| openfault::db::seed_dev(c, &key))
            .await
            .expect("seed dev project");
    }
    let state = AppState::new(db.clone());
    tokio::spawn(process::run(db.clone(), state.notify.clone()));
    tokio::spawn(process::housekeeping(db));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind");
    tracing::info!("listening on :{port}");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown())
        .await
        .expect("serve");
}
