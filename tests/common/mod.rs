//! Shared seed + request helpers for tests/api.rs and tests/mcp.rs.
#![allow(dead_code)]
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use openfault::{AppState, app, auth::hash_token, db::Db, process};
use serde_json::Value;
use tower::ServiceExt;

/// DSN key of the real fixtures (tests/fixtures/real/NOTES.md); project 1.
pub const KEY: &str = "abcdef0123456789abcdef0123456789";
pub const KEY2: &str = "22222222222222222222222222222222";
pub const ADMIN_W: &str = "of_admin_write";
pub const ADMIN_R: &str = "of_admin_read";
pub const MEMBER_W: &str = "of_member_write";

pub struct T {
    pub app: Router,
    pub db: Db,
    pub _dir: tempfile::TempDir,
}

/// Org + projects `dev` (1) and `other` (2); admin user 1, member user 2 (dev only); three tokens.
pub async fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    let toks = [ADMIN_W, ADMIN_R, MEMBER_W].map(hash_token);
    db.write(move |c| {
        openfault::db::seed_dev(c, KEY)?;
        c.execute_batch(&format!(
            "INSERT INTO projects (id, org_id, slug, name, created_at) VALUES (2, 1, 'other', 'Other', 0);
             INSERT INTO dsn_keys (project_id, public_key, created_at) VALUES (2, '{KEY2}', 0);
             INSERT INTO users (id, email, name, pw_hash, role, created_at) VALUES (1, 'a@x.io', 'Ada', 'x', 'admin', 0), (2, 'm@x.io', 'Max', 'x', 'member', 0);
             INSERT INTO project_members (project_id, user_id, role) VALUES (1, 2, 'member');
             INSERT INTO api_tokens (token_hash, user_id, scopes) VALUES ('{}', 1, 'write'), ('{}', 1, 'read'), ('{}', 2, 'write');",
            toks[0], toks[1], toks[2]
        ))
    })
    .await
    .unwrap();
    let st = AppState::new(db.clone());
    tokio::spawn(process::run(db.clone(), st.notify.clone()));
    T {
        app: app(st),
        db,
        _dir: dir,
    }
}

/// Posts a real fixture with a fresh event_id so every post stores a new event.
pub async fn post_fixture(t: &T, name: &str, project: i64) {
    let raw = std::fs::read_to_string(format!(
        "{}/tests/fixtures/real/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let old = serde_json::from_str::<Value>(raw.lines().next().unwrap()).unwrap()["event_id"]
        .as_str()
        .unwrap()
        .to_string();
    let body = raw.replace(&old, &uuid::Uuid::now_v7().simple().to_string());
    let key = if project == 1 { KEY } else { KEY2 };
    let (s, _) = req(
        t,
        "POST",
        &format!("/api/{project}/envelope/?sentry_key={key}"),
        "",
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{name}");
}

pub async fn wait_processed(db: &Db) {
    for _ in 0..100 {
        let n: i64 = db
            .read(|c| c.query_row("SELECT count(*) FROM inbox", [], |r| r.get(0)))
            .await
            .unwrap();
        if n == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    panic!("inbox not drained");
}

pub async fn q(db: &Db, sql: &str) -> i64 {
    let sql = sql.to_string();
    db.read(move |c| c.query_row(&sql, [], |r| r.get(0)))
        .await
        .unwrap()
}

/// Returns (status, JSON body or Null). `token` "" sends no Authorization header.
pub async fn req(
    t: &T,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
) -> (StatusCode, Value) {
    let (s, _, v) = raw_req(t, method, uri, token, body, &[]).await;
    (s, v)
}

pub async fn raw_req(
    t: &T,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
    extra: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost");
    if !token.is_empty() {
        r = r.header("authorization", format!("Bearer {token}"));
    }
    if body.is_some() {
        r = r.header("content-type", "application/json");
    }
    for (k, v) in extra {
        r = r.header(*k, *v);
    }
    let res = t
        .app
        .clone()
        .oneshot(r.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let (parts, b) = res.into_parts();
    let bytes = axum::body::to_bytes(b, usize::MAX).await.unwrap();
    (
        parts.status,
        parts.headers,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())),
    )
}
