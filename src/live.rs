//! Live updates (PLAN §8): `GET /live` SSE ticks + `GET /live/summary` JSON for TV/dashboard refreshers.
use crate::{AppState, auth::CurrentUser, db::now};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use futures_util::stream::{self, StreamExt};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, convert::Infallible, time::Duration};
use tokio::sync::{
    Semaphore,
    broadcast::{Receiver, error::RecvError},
};

#[derive(Clone, Debug, Serialize)]
pub struct LiveEvent {
    pub project_id: i64,
    pub issue_id: i64,
    pub issue_title: String,
    pub level: String,
    pub kind: &'static str,
    pub ts: i64,
}

impl LiveEvent {
    /// Titles come from SDK payloads: control chars stripped, ≤120 chars. Clients must still insert as text.
    pub fn new(
        pid: i64,
        issue_id: i64,
        title: &str,
        level: &str,
        kind: &'static str,
        ts: i64,
    ) -> Self {
        LiveEvent {
            project_id: pid,
            issue_id,
            issue_title: title
                .chars()
                .filter(|c| !c.is_control())
                .take(120)
                .collect(),
            level: level
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(16)
                .collect(),
            kind,
            ts,
        }
    }
}

static SLOTS: Semaphore = Semaphore::const_new(256);
const WINDOW: Duration = Duration::from_millis(500);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/live", get(live))
        .route("/live/summary", get(summary))
}

#[derive(Deserialize)]
struct ScopeQ {
    scope: Option<String>,
}

enum Scope {
    All,
    Project(i64),
    Issue(i64),
}

/// Visible project ids for `u`, narrowed by scope. None = scope target missing or not visible (→ 404).
async fn resolve(st: &AppState, u: &CurrentUser, q: Option<String>) -> Option<(Scope, Vec<i64>)> {
    let q = q.unwrap_or_else(|| "all".into());
    let scope = match q.split_once(':') {
        None if q == "all" => Scope::All,
        Some(("project", id)) => Scope::Project(id.parse().ok()?),
        Some(("issue", id)) => Scope::Issue(id.parse().ok()?),
        _ => return None,
    };
    let (all, uid) = (u.is_admin(), u.id);
    let issue = match scope {
        Scope::Issue(i) => Some(i),
        _ => None,
    };
    let (visible, issue_pid) = st
        .db
        .read(move |c| {
            let v: Vec<i64> = c
                // same rule as ui::VISIBLE; inlined so UI rewrites can't silently widen it.
                .prepare("SELECT id FROM projects WHERE ?1 OR id IN (SELECT project_id FROM project_members WHERE user_id = ?2)")?
                .query_map(params![all, uid], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let ip: Option<i64> = match issue {
                Some(i) => c
                    .query_row("SELECT project_id FROM issues WHERE id = ?1", [i], |r| r.get(0))
                    .optional()?,
                None => None,
            };
            Ok((v, ip))
        })
        .await
        .ok()?;
    let pids = match scope {
        Scope::All => visible,
        Scope::Project(p) => visible.contains(&p).then(|| vec![p])?,
        Scope::Issue(_) => {
            let p = issue_pid?;
            visible.contains(&p).then(|| vec![p])?
        }
    };
    Some((scope, pids))
}

async fn live(State(st): State<AppState>, u: CurrentUser, Query(q): Query<ScopeQ>) -> Response {
    let Some((scope, pids)) = resolve(&st, &u, q.scope).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(permit) = SLOTS.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // ponytail: visibility is computed once per connection; a revoked member keeps ticks until reconnect.
    let pids: HashSet<i64> = pids.into_iter().collect();
    let matches = move |e: &LiveEvent| match scope {
        Scope::Issue(i) => e.issue_id == i,
        _ => pids.contains(&e.project_id),
    };
    let rx = st.db.live.subscribe();
    let hello = Event::default()
        .event("hello")
        .data(json!({"server_time": now()}).to_string());
    let ticks = stream::unfold((rx, matches, permit), |(mut rx, m, permit)| async move {
        let tick = next_tick(&mut rx, &m).await?;
        Some((
            Ok::<_, Infallible>(Event::default().event("tick").data(tick.to_string())),
            (rx, m, permit),
        ))
    });
    Sse::new(stream::once(async { Ok(hello) }).chain(ticks))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// Waits for the first matching event, then coalesces everything within WINDOW into one tick.
/// Lagged: skipped events (any scope, we can't tell) are added to `count`.
async fn next_tick(rx: &mut Receiver<LiveEvent>, m: &impl Fn(&LiveEvent) -> bool) -> Option<Value> {
    let (mut latest, mut count) = (None, 0u64);
    while latest.is_none() && count == 0 {
        match rx.recv().await {
            Ok(e) if m(&e) => (latest, count) = (Some(e), 1),
            Ok(_) => {}
            Err(RecvError::Lagged(n)) => count = n,
            Err(RecvError::Closed) => return None,
        }
    }
    let deadline = tokio::time::Instant::now() + WINDOW;
    while let Ok(r) = tokio::time::timeout_at(deadline, rx.recv()).await {
        match r {
            Ok(e) if m(&e) => (latest, count) = (Some(e), count + 1),
            Ok(_) => {}
            Err(RecvError::Lagged(n)) => count += n,
            Err(RecvError::Closed) => break,
        }
    }
    let mut v = latest.map_or_else(|| json!({}), |e| json!(e));
    v["count"] = count.into();
    Some(v)
}

async fn summary(State(st): State<AppState>, u: CurrentUser, Query(q): Query<ScopeQ>) -> Response {
    // ponytail: issue scope summarises the issue's project; per-issue series isn't needed by TV/dashboard.
    let Some((_, pids)) = resolve(&st, &u, q.scope).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ids = json!(pids).to_string();
    let t = now();
    let first = t - t % 300 - 11 * 300;
    let r = st
        .db
        .read(move |c| {
            const IN: &str = "project_id IN (SELECT value FROM json_each(?1))";
            let unresolved: i64 = c.query_row(
                &format!("SELECT count(*) FROM issues WHERE {IN} AND status = 'unresolved'"),
                [&ids],
                |r| r.get(0),
            )?;
            let received_24h: i64 = c.query_row(
                &format!("SELECT coalesce(sum(count), 0) FROM project_5min WHERE {IN} AND bucket_ts > ?2"),
                params![ids, t - 86400],
                |r| r.get(0),
            )?;
            let mut series = [0i64; 12];
            let mut st = c.prepare(&format!(
                "SELECT bucket_ts, sum(count) FROM project_5min WHERE {IN} AND bucket_ts >= ?2 GROUP BY bucket_ts"
            ))?;
            for row in st.query_map(params![ids, first], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
                let (b, n) = row?;
                if let Some(s) = series.get_mut(((b - first) / 300) as usize) {
                    *s += n;
                }
            }
            let latest: Vec<Value> = c
                .prepare(&format!(
                    "SELECT id, title, level, project_id, last_seen FROM issues WHERE {IN} ORDER BY last_seen DESC LIMIT 5"
                ))?
                .query_map([&ids], |r| {
                    let e = LiveEvent::new(r.get(3)?, r.get(0)?, &r.get::<_, String>(1)?, &r.get::<_, String>(2)?, "event", r.get(4)?);
                    Ok(json!({"issue_id": e.issue_id, "title": e.issue_title, "level": e.level, "project_id": e.project_id, "ts": e.ts}))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(json!({
                "unresolved": unresolved,
                "received_1h": series.iter().sum::<i64>(),
                "received_24h": received_24h,
                "series_1h": series,
                "latest": latest,
            }))
        })
        .await;
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            tracing::error!("live summary: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
