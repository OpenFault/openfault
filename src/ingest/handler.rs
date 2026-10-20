//! Sentry-compatible ingest: POST /api/{pid}/envelope/ and /api/{pid}/store/ (PLAN §4).
#![allow(clippy::result_large_err)] // ponytail: Err(Response) only on the reject path; boxing buys nothing here.
use super::envelope;
use crate::{AppState, db::now, scrub::scrub};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::collections::HashMap;

const MAX_EVENT: usize = 1 << 20;
const INBOX_CAP: i64 = 10_000;
const DROPPED: &str = "86400:transaction;span;profile;replay;session:organization";
// ponytail: quotas are constants (Bugsink parity); per-project editing lands with Settings. Monthly 1M quota skipped.
pub const QUOTAS: [(i64, usize); 2] = [(300, 1_000), (3600, 5_000)];

type Q = Query<HashMap<String, String>>;

fn err(code: StatusCode, detail: &str) -> Response {
    (
        code,
        [(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")],
        Json(json!({ "detail": detail })),
    )
        .into_response()
}

fn ok(id: &str) -> Response {
    (
        [
            ("x-sentry-rate-limits", DROPPED),
            ("access-control-allow-origin", "*"),
        ],
        Json(json!({ "id": id })),
    )
        .into_response()
}

fn too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [
            ("retry-after", "60"),
            ("x-sentry-rate-limits", "60:error:project:quota"),
            ("access-control-allow-origin", "*"),
        ],
        Json(json!({ "detail": "rate limited" })),
    )
        .into_response()
}

fn header_key(h: &HeaderMap) -> Option<String> {
    let v = h.get("x-sentry-auth")?.to_str().ok()?;
    v.trim_start_matches("Sentry")
        .split(',')
        .find_map(|p| p.trim().strip_prefix("sentry_key="))
        .map(str::to_string)
}

fn dsn_key(dsn: &str) -> Option<String> {
    let userinfo = dsn.split_once("://")?.1.split_once('@')?.0;
    Some(userinfo.split(':').next()?.to_string())
}

/// Some(store_ip) when the key is active and belongs to `pid`.
async fn auth(st: &AppState, pid: i64, key: Option<String>) -> Result<bool, Response> {
    let Some(key) = key else {
        return Err(err(StatusCode::FORBIDDEN, "missing sentry_key"));
    };
    let settings = st
        .db
        .read(move |c| {
            use rusqlite::OptionalExtension;
            c.query_row(
                "SELECT p.settings_json FROM dsn_keys k JOIN projects p ON p.id = k.project_id
                 WHERE k.public_key = ?1 AND k.project_id = ?2 AND k.active = 1",
                rusqlite::params![key, pid],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
        .await
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, "db unavailable"))?;
    match settings {
        Some(s) => Ok(serde_json::from_str::<Value>(&s)
            .ok()
            .and_then(|v| v["store_ip"].as_bool())
            .unwrap_or(false)),
        None => Err(err(StatusCode::FORBIDDEN, "invalid sentry_key for project")),
    }
}

fn event_id(ev: &Value, fallback: Option<&str>) -> Result<String, Response> {
    let raw = ev.get("event_id").and_then(Value::as_str).or(fallback);
    let id = match raw {
        Some(r) => r.replace('-', "").to_ascii_lowercase(),
        None => uuid::Uuid::now_v7().simple().to_string(),
    };
    if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err(err(StatusCode::BAD_REQUEST, "invalid event_id"))
    }
}

fn parse_event(b: &[u8]) -> Result<Value, Response> {
    if b.len() > MAX_EVENT {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "event exceeds 1 MiB"));
    }
    match serde_json::from_slice::<Value>(b) {
        Ok(v @ Value::Object(_)) => Ok(v),
        _ => Err(err(StatusCode::BAD_REQUEST, "event is not a JSON object")),
    }
}

/// Validate, quota-check, scrub, compress and INSERT into the inbox. Returns the last event id.
async fn accept(
    st: &AppState,
    pid: i64,
    store_ip: bool,
    events: Vec<Value>,
    fallback_id: Option<&str>,
) -> Response {
    let mut rows = Vec::with_capacity(events.len());
    let mut last_id = String::new();
    for mut ev in events {
        let id = match event_id(&ev, fallback_id) {
            Ok(id) => id,
            Err(r) => return r,
        };
        ev["event_id"] = id.clone().into();
        scrub(&mut ev, store_ip);
        let bytes = serde_json::to_vec(&ev).unwrap_or_default();
        rows.push(zstd::encode_all(&bytes[..], 3).unwrap_or_default());
        last_id = id;
    }
    let inbox: i64 = match st
        .db
        .read(|c| c.query_row("SELECT count(*) FROM inbox", [], |r| r.get(0)))
        .await
    {
        Ok(n) => n,
        Err(_) => return err(StatusCode::SERVICE_UNAVAILABLE, "db unavailable"),
    };
    if inbox > INBOX_CAP {
        return too_many();
    }
    let t = now();
    {
        let mut q = st.quota.lock().unwrap();
        let seen = q.entry(pid).or_default();
        while seen.front().is_some_and(|&x| x <= t - QUOTAS[1].0) {
            seen.pop_front();
        }
        let over = QUOTAS.iter().any(|&(win, max)| {
            seen.iter().rev().take_while(|&&x| x > t - win).count() + rows.len() > max
        });
        if over {
            return too_many();
        }
        seen.extend(std::iter::repeat_n(t, rows.len()));
    }
    let r = st
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            for body in rows {
                tx.execute(
                    "INSERT INTO inbox (project_id, received_at, body) VALUES (?1, ?2, ?3)",
                    rusqlite::params![pid, t, body],
                )?;
            }
            tx.commit()
        })
        .await;
    if r.is_err() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "db unavailable");
    }
    st.notify.notify_one();
    ok(&last_id)
}

pub async fn envelope(
    State(st): State<AppState>,
    Path(pid): Path<i64>,
    Query(q): Q,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let env = match envelope::parse(&body) {
        Ok(e) => e,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let key = header_key(&headers)
        .or_else(|| q.get("sentry_key").cloned())
        .or_else(|| {
            env.header
                .get("dsn")
                .and_then(Value::as_str)
                .and_then(dsn_key)
        });
    let store_ip = match auth(&st, pid, key).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let header_id = env.header.get("event_id").and_then(Value::as_str);
    let mut events = Vec::new();
    for item in env
        .items
        .iter()
        .filter(|i| i.header.get("type").and_then(Value::as_str) == Some("event"))
    {
        match parse_event(item.payload) {
            Ok(v) => events.push(v),
            Err(r) => return r,
        }
    }
    if events.is_empty() {
        return ok(header_id.unwrap_or(""));
    }
    accept(&st, pid, store_ip, events, header_id).await
}

pub async fn store(
    State(st): State<AppState>,
    Path(pid): Path<i64>,
    Query(q): Q,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let key = header_key(&headers).or_else(|| q.get("sentry_key").cloned());
    let store_ip = match auth(&st, pid, key).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match parse_event(&body) {
        Ok(ev) => accept(&st, pid, store_ip, vec![ev], None).await,
        Err(r) => r,
    }
}
