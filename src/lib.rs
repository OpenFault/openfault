pub mod api;
pub mod auth;
pub mod backup;
pub mod config;
pub mod db;
pub mod ingest;
pub mod issues;
pub mod live;
pub mod mail;
pub mod mcp;
pub mod notify;
pub mod pairing;
pub mod process;
pub mod relay;
pub mod scrub;
pub mod setup;
pub mod sourcemaps;
pub mod teams;
pub mod ui;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tower_http::{
    decompression::RequestDecompressionLayer, limit::RequestBodyLimitLayer, trace::TraceLayer,
};

#[derive(Clone)]
pub struct AppState {
    pub db: db::Db,
    pub notify: Arc<Notify>,
    /// ponytail: in-memory per-project accept timestamps; resets on restart, which is fine for burst quotas.
    pub quota: Arc<Mutex<HashMap<i64, VecDeque<i64>>>>,
    /// /login + /forgot attempts per client IP: (count, window_start).
    pub attempts: Arc<Mutex<HashMap<String, (u32, i64)>>>,
    /// raw setup token while no user exists (shown on /setup), see src/setup.rs.
    pub setup_token: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl AppState {
    pub fn new(db: db::Db) -> Self {
        config::get(&db);
        AppState {
            db,
            notify: Arc::new(Notify::new()),
            quota: Default::default(),
            attempts: Default::default(),
            setup_token: Default::default(),
        }
    }
}

const MIB: usize = 1 << 20;

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(crate::sourcemaps::routes())
        .route(
            "/api/{project_id}/envelope/",
            post(ingest::handler::envelope),
        )
        .route("/api/{project_id}/store/", post(ingest::handler::store))
        .nest("/api/v1", api::routes())
        .merge(mcp::routes(state.clone()))
        .merge(ui::routes())
        .merge(live::routes())
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(100 * MIB)) // decompressed
        .layer(RequestDecompressionLayer::new())
        .layer(RequestBodyLimitLayer::new(20 * MIB)) // compressed, on the wire
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Binds `OPENFAULT_BIND` (default `::`, dual-stack so both 127.0.0.1 and ::1 work) on `port`.
/// Falls back to 0.0.0.0 when the default IPv6 bind fails (host without IPv6).
pub fn listen(port: u16) -> tokio::net::TcpListener {
    use socket2::{Domain, Socket, Type};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    let host = std::env::var("OPENFAULT_BIND").unwrap_or_else(|_| "::".into());
    let ip: IpAddr = host
        .trim_matches(['[', ']'])
        .parse()
        .expect("OPENFAULT_BIND must be an IP address, e.g. :: or 0.0.0.0");
    let bind = |addr: SocketAddr| -> std::io::Result<std::net::TcpListener> {
        let s = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
        if addr.is_ipv6() {
            s.set_only_v6(false)?;
        }
        s.set_reuse_address(true)?;
        s.set_nonblocking(true)?;
        s.bind(&addr.into())?;
        s.listen(1024)?;
        Ok(s.into())
    };
    let l = bind(SocketAddr::new(ip, port))
        .or_else(|e| {
            if !ip.is_ipv6() || !ip.is_unspecified() {
                return Err(e);
            }
            tracing::warn!("IPv6 bind failed ({e}); falling back to 0.0.0.0:{port}");
            bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))
        })
        .expect("bind");
    tokio::net::TcpListener::from_std(l).expect("bind")
}
