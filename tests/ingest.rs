use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use openfault::{
    AppState, app,
    db::{Db, seed_dev},
    process,
};
use serde_json::Value;
use std::io::Write;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

async fn setup() -> (Router, Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.write(|c| seed_dev(c, KEY)).await.unwrap();
    let st = AppState::new(db.clone());
    tokio::spawn(process::run(db.clone(), st.notify.clone()));
    (app(st), db, dir)
}

async fn post(
    app: &Router,
    uri: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut req = Request::post(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    (
        parts.status,
        parts.headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn count(db: &Db, sql: &'static str) -> i64 {
    db.read(move |c| c.query_row(sql, [], |r| r.get(0)))
        .await
        .unwrap()
}

async fn wait_processed(db: &Db) {
    for _ in 0..40 {
        if count(db, "SELECT count(*) FROM inbox").await == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("inbox not drained within 2s");
}

const AUTH: &str = "Sentry sentry_version=7, sentry_client=sentry.javascript.nextjs/9.10.0, sentry_key=0123456789abcdef0123456789abcdef";

#[tokio::test(flavor = "multi_thread")]
async fn fixtures_land_as_one_issue_per_bug() {
    let (app, db, _dir) = setup().await;
    let env = "/api/1/envelope/";

    // ruby: auth via envelope header `dsn`; sent twice with a different lineno
    let ruby = fixture("ruby_rails.envelope");
    let (s, h, j) = post(&app, env, ruby.clone(), &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(j["id"], "aa1b6c0e2f8e4d9fa0c1b2d3e4f50607");
    assert_eq!(
        h["x-sentry-rate-limits"],
        "86400:transaction;span;profile;replay;session:organization"
    );
    let ruby2 = String::from_utf8(ruby)
        .unwrap()
        .replace("\"lineno\":42,", "\"lineno\":43,")
        .replace("f50607", "f50608");
    assert_eq!(
        post(&app, env, ruby2.into_bytes(), &[]).await.0,
        StatusCode::OK
    );

    // next.js server: auth via X-Sentry-Auth; plus the same envelope gzip-compressed
    let server = fixture("nextjs_server.envelope");
    assert_eq!(
        post(&app, env, server.clone(), &[("x-sentry-auth", AUTH)])
            .await
            .0,
        StatusCode::OK
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&server).unwrap();
    let (s, _, j) = post(
        &app,
        env,
        gz.finish().unwrap(),
        &[("x-sentry-auth", AUTH), ("content-encoding", "gzip")],
    )
    .await;
    assert_eq!(
        (s, j["id"].as_str()),
        (StatusCode::OK, Some("bb2c7d1f3a9f4e0ab1d2c3e4f5061728"))
    );

    // next.js browser: auth via ?sentry_key=
    let uri = format!("{env}?sentry_key={KEY}&sentry_version=7");
    assert_eq!(
        post(&app, &uri, fixture("nextjs_browser.envelope"), &[])
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        post(&app, env, fixture("message.envelope"), &[]).await.0,
        StatusCode::OK
    );
    let (s, _, j) = post(&app, env, fixture("mixed.envelope"), &[]).await;
    assert_eq!(
        (s, j["id"].as_str()),
        (StatusCode::OK, Some("ee5fa04c6d2c4b3de4a5f60718293a4b"))
    );

    // legacy /store/
    let (s, h, j) = post(
        &app,
        "/api/1/store/",
        fixture("store_python.json"),
        &[("x-sentry-auth", AUTH)],
    )
    .await;
    assert_eq!(
        (s, j["id"].as_str()),
        (StatusCode::OK, Some("0a1b2c3d4e5f4a6b8c9d0e1f2a3b4c5d"))
    );
    assert!(h.contains_key("x-sentry-rate-limits"));

    wait_processed(&db).await;
    // ruby, nextjs server, nextjs browser, message, mixed's event, python store. Transaction/session ignored.
    assert_eq!(count(&db, "SELECT count(*) FROM issues").await, 6);
    assert_eq!(
        count(
            &db,
            "SELECT event_count FROM issues WHERE title LIKE 'NoMethodError%'"
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &db,
            "SELECT event_count FROM issues WHERE title LIKE 'TypeError%'"
        )
        .await,
        1 // gzip copy has the same event_id: deduped
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM events WHERE id = 'ff60b15d7e3d4c4ef5b6071829304b5c'"
        )
        .await,
        0
    );
    assert_eq!(count(&db, "SELECT count(*) FROM releases").await, 2);
    assert_eq!(
        count(&db, "SELECT sum(received) FROM project_hourly").await,
        7
    );

    let (title, culprit, payload): (String, String, Vec<u8>) = db
        .read(|c| c.query_row(
            "SELECT i.title, i.culprit, e.payload FROM events e JOIN issues i ON i.id = e.issue_id WHERE e.id = 'aa1b6c0e2f8e4d9fa0c1b2d3e4f50607'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))
        .await
        .unwrap();
    assert_eq!(title, "NoMethodError: undefined method `name' for nil");
    assert_eq!(culprit, "customer_name (app/models/order.rb)");
    let ev: Value = serde_json::from_slice(&zstd::decode_all(&payload[..]).unwrap()).unwrap();
    assert_eq!(ev["request"]["headers"]["Authorization"], "[Filtered]");
    assert_eq!(ev["request"]["headers"]["Cookie"], "[Filtered]");
    assert_eq!(ev["request"]["query_string"], "ref=email&token=[Filtered]");
    assert_eq!(ev["extra"]["api_key"], "[Filtered]");
    assert_eq!(ev["user"]["email"], "jane@example.com");
    assert!(ev["user"].get("ip_address").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_bad_requests() {
    let (app, db, _dir) = setup().await;
    let ruby = String::from_utf8(fixture("ruby_rails.envelope")).unwrap();

    let wrong = ruby.replace(KEY, "ffffffffffffffffffffffffffffffff");
    assert_eq!(
        post(&app, "/api/1/envelope/", wrong.into_bytes(), &[])
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // right key, wrong project
    assert_eq!(
        post(&app, "/api/2/envelope/", ruby.clone().into_bytes(), &[])
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&app, "/api/1/store/", b"{}".to_vec(), &[]).await.0,
        StatusCode::FORBIDDEN
    );

    let bad_id = ruby.replace("aa1b6c0e2f8e4d9fa0c1b2d3e4f50607", "not-a-uuid");
    let (s, _, j) = post(&app, "/api/1/envelope/", bad_id.into_bytes(), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(j["detail"].is_string());

    let big = format!(
        "{{\"dsn\":\"https://{KEY}@h/1\"}}\n{{\"type\":\"event\"}}\n{{\"message\":\"{}\"}}\n",
        "x".repeat(1 << 20)
    );
    assert_eq!(
        post(&app, "/api/1/envelope/", big.into_bytes(), &[])
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    // headers-only envelope: 200 with the header event_id, nothing queued
    let (s, _, j) = post(
        &app,
        "/api/1/envelope/",
        format!("{{\"event_id\":\"abc\",\"dsn\":\"https://{KEY}@h/1\"}}\n").into_bytes(),
        &[],
    )
    .await;
    assert_eq!((s, j["id"].as_str()), (StatusCode::OK, Some("abc")));
    assert_eq!(
        count(&db, "SELECT count(*) FROM inbox").await
            + count(&db, "SELECT count(*) FROM issues").await,
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resolved_issue_regresses() {
    let (app, db, _dir) = setup().await;
    assert_eq!(
        post(&app, "/api/1/envelope/", fixture("message.envelope"), &[])
            .await
            .0,
        StatusCode::OK
    );
    wait_processed(&db).await;
    db.write(|c| {
        c.execute(
            "UPDATE issues SET status = 'resolved', resolved_in_release = '1.4.2'",
            [],
        )
    })
    .await
    .unwrap();
    let again = String::from_utf8(fixture("message.envelope"))
        .unwrap()
        .replace("394a", "394b")
        .replace("order 1234", "order 999");
    assert_eq!(
        post(&app, "/api/1/envelope/", again.into_bytes(), &[])
            .await
            .0,
        StatusCode::OK
    );
    wait_processed(&db).await;
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM issues WHERE status = 'unresolved' AND event_count = 2"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM issue_activity WHERE kind = 'regressed'"
        )
        .await,
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_event_counts_once() {
    let (app, db, _dir) = setup().await;
    let mut rx = db.live.subscribe();
    for _ in 0..2 {
        let env = fixture("message.envelope");
        assert_eq!(
            post(&app, "/api/1/envelope/", env, &[]).await.0,
            StatusCode::OK
        );
        wait_processed(&db).await;
    }
    assert_eq!(count(&db, "SELECT sum(event_count) FROM issues").await, 1);
    assert_eq!(
        count(&db, "SELECT sum(received) FROM project_hourly").await,
        1
    );
    assert_eq!(count(&db, "SELECT sum(count) FROM issue_hourly").await, 1);
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_err(), "exactly one live tick");
}
