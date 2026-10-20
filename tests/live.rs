//! Live updates: SSE ticks on ingest + status change, scope visibility, summary buckets.
mod common;
use axum::{
    body::{Body, BodyDataStream},
    http::{Request, StatusCode, header},
};
use common::*;
use futures_util::StreamExt;
use openfault::{
    auth::hash_token,
    issues::{self, IssueAction},
};
use serde_json::Value;
use std::time::Duration;
use tower::ServiceExt;

/// Session cookie for `uid` without going through the login form.
async fn session(t: &T, uid: i64) -> String {
    let h = hash_token(&format!("live-{uid}"));
    t.db.write(move |c| {
        c.execute(
            "INSERT INTO sessions (id_hash, user_id, expires_at) VALUES (?1, ?2, 9999999999)",
            rusqlite::params![h, uid],
        )
    })
    .await
    .unwrap();
    format!("of_session=live-{uid}")
}

async fn get(t: &T, uri: &str, cookie: &str) -> axum::response::Response {
    let r = Request::get(uri)
        .header("host", "localhost")
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap();
    t.app.clone().oneshot(r).await.unwrap()
}

/// Next SSE event of type `name` as JSON, within 2s.
async fn next(s: &mut BodyDataStream, buf: &mut String, name: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            while let Some(i) = buf.find("\n\n") {
                let frame: String = buf.drain(..i + 2).collect();
                if frame.contains(&format!("event: {name}\n")) {
                    let data = frame
                        .lines()
                        .find_map(|l| l.strip_prefix("data: "))
                        .unwrap();
                    return serde_json::from_str(data).unwrap();
                }
            }
            buf.push_str(&String::from_utf8_lossy(&s.next().await.unwrap().unwrap()));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {name} event within 2s"))
}

#[tokio::test(flavor = "multi_thread")]
async fn ticks_scope_and_summary() {
    let t = setup().await;
    let admin = session(&t, 1).await;
    let member = session(&t, 2).await;

    let res = get(&t, "/live?scope=all", &admin).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let (mut s, mut buf) = (res.into_body().into_data_stream(), String::new());
    assert!(
        next(&mut s, &mut buf, "hello").await["server_time"]
            .as_i64()
            .unwrap()
            > 0
    );

    post_fixture(&t, "node_exception.envelope", 1).await;
    let tick = next(&mut s, &mut buf, "tick").await;
    assert_eq!(
        (
            tick["kind"].as_str(),
            tick["project_id"].as_i64(),
            tick["count"].as_i64()
        ),
        (Some("new_issue"), Some(1), Some(1))
    );
    assert!(tick["issue_title"].as_str().unwrap().chars().count() <= 120);

    let id = tick["issue_id"].as_i64().unwrap();
    issues::apply_action(
        &t.db,
        id,
        1,
        IssueAction::Resolve {
            in_release: None,
            next_release: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(next(&mut s, &mut buf, "tick").await["kind"], "resolved");

    // no session → redirect, never a stream
    assert_eq!(
        get(&t, "/live?scope=all", "").await.status(),
        StatusCode::SEE_OTHER
    );
    // member sees project 1 only
    assert_eq!(
        get(&t, "/live?scope=project:2", &member).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&t, "/live?scope=bogus", &member).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&t, &format!("/live/summary?scope=issue:{id}"), &member)
            .await
            .status(),
        StatusCode::OK
    );

    // two events inside one 500ms window → one coalesced tick
    post_fixture(&t, "ruby_exception.envelope", 1).await;
    post_fixture(&t, "node_exception.envelope", 1).await;
    assert_eq!(next(&mut s, &mut buf, "tick").await["count"], 2);
    wait_processed(&t.db).await;
    let res = get(&t, "/live/summary?scope=project:1", &member).await;
    assert_eq!(res.status(), StatusCode::OK);
    let v: Value = serde_json::from_slice(
        &axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let series: Vec<i64> = v["series_1h"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_i64().unwrap())
        .collect();
    assert_eq!(series.len(), 12);
    assert_eq!(
        series.iter().sum::<i64>(),
        v["received_1h"].as_i64().unwrap()
    );
    assert_eq!(
        (
            v["received_1h"].as_i64(),
            v["received_24h"].as_i64(),
            v["unresolved"].as_i64()
        ),
        (Some(3), Some(3), Some(2))
    );
    assert_eq!(v["latest"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_token_streams_and_token_project_narrows() {
    let t = setup().await;
    let h = hash_token("of_admin_dev");
    t.db.write(move |c| {
        c.execute(
            "INSERT INTO api_tokens (token_hash, user_id, scopes, project_id) VALUES (?1, 1, 'read', 1)",
            [h],
        )
    })
    .await
    .unwrap();
    let get = |uri: &str, tok: &str| {
        let r = Request::get(uri)
            .header("host", "localhost")
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        t.app.clone().oneshot(r)
    };
    let res = get("/live?scope=all", ADMIN_R).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let (mut s, mut buf) = (res.into_body().into_data_stream(), String::new());
    assert!(next(&mut s, &mut buf, "hello").await["server_time"].is_i64());
    post_fixture(&t, "node_exception.envelope", 1).await;
    assert_eq!(next(&mut s, &mut buf, "tick").await["project_id"], 1);

    let status =
        |uri: &'static str, tok: &'static str| async move { get(uri, tok).await.unwrap().status() };
    assert_eq!(
        status("/live?scope=all", "of_bogus").await,
        StatusCode::UNAUTHORIZED
    );
    // admin token pinned to project 1 can't see project 2
    assert_eq!(
        status("/live?scope=project:2", "of_admin_dev").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status("/live/summary?scope=project:1", "of_admin_dev").await,
        StatusCode::OK
    );
    assert_eq!(
        status("/live/summary?scope=project:2", ADMIN_R).await,
        StatusCode::OK
    );
}
