pub mod auth;
pub mod db;
pub mod ingest;
pub mod mail;
pub mod process;
pub mod scrub;
pub mod sourcemaps;
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
}

impl AppState {
    pub fn new(db: db::Db) -> Self {
        AppState {
            db,
            notify: Arc::new(Notify::new()),
            quota: Default::default(),
            attempts: Default::default(),
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
        .merge(ui::routes())
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(100 * MIB)) // decompressed
        .layer(RequestDecompressionLayer::new())
        .layer(RequestBodyLimitLayer::new(20 * MIB)) // compressed, on the wire
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
