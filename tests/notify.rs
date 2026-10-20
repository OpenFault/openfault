//! M6 notification pipeline: queue → worker pass → email outbox / webhook stub / relay stub.
mod common;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use common::{KEY, q, req, setup, wait_processed};
use openfault::notify;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;

async fn stub() -> (String, Seen) {
    let seen: Seen = Default::default();
    async fn hook(State(s): State<Seen>, h: HeaderMap, body: String) -> &'static str {
        s.lock()
            .unwrap()
            .push(("hook".into(), h, serde_json::from_str(&body).unwrap()));
        "ok"
    }
    async fn relay(State(s): State<Seen>, h: HeaderMap, Json(v): Json<Value>) -> Json<Value> {
        let results: Vec<Value> = v["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let dead = t == "bb22";
                json!({"token": t, "status": if dead { 410 } else { 200 }, "reason": if dead { Some("Unregistered") } else { None }})
            })
            .collect();
        s.lock().unwrap().push(("relay".into(), h, v));
        Json(json!({"results": results}))
    }
    let app = Router::new()
        .route("/hook", post(hook))
        .route("/v1/push", post(relay))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn post_message(t: &common::T, msg: &str) {
    let id = uuid::Uuid::now_v7().simple().to_string();
    let body = format!(
        "{}\n{{\"type\":\"event\"}}\n{}\n",
        json!({"event_id": id}),
        json!({"event_id": id, "platform": "node", "level": "error", "message": msg})
    );
    let (s, _) = req(
        t,
        "POST",
        &format!("/api/1/envelope/?sentry_key={KEY}"),
        "",
        Some(body),
    )
    .await;
    assert_eq!(s, 200);
    wait_processed(&t.db).await;
}

fn outbox_for(subject_part: &str) -> Vec<(String, String, String, String)> {
    openfault::mail::OUTBOX
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.1.contains(subject_part))
        .cloned()
        .collect()
}

#[tokio::test]
async fn pipeline_email_webhook_push_dedupe_prefs() {
    let (base, seen) = stub().await;
    // SAFETY: set before any worker reads them; the other test in this binary does not read these.
    unsafe {
        std::env::set_var("OPENFAULT_ALLOW_PRIVATE_WEBHOOKS", "1");
        std::env::set_var("OPENFAULT_PUSH_RELAY_URL", &base);
        std::env::set_var("OPENFAULT_PUSH_RELAY_TOKEN", "rt");
    }
    let t = setup().await;
    // user 1 (admin): push only via the project_id NULL fallback, two iOS devices (bb22 is dead).
    // user 2 (member of dev): email only (no device), opted out of regression for project 1.
    t.db.write(|c| {
        c.execute_batch(
            "INSERT INTO devices (user_id, platform, push_token, created_at) VALUES (1, 'ios', 'aa11', 0), (1, 'ios', 'bb22', 0);
             INSERT INTO notif_prefs (user_id, project_id, kind, email, push) VALUES (2, 1, 'regression', 0, 0);",
        )
    })
    .await
    .unwrap();
    let all_push: Vec<notify::Pref> = notify::KINDS
        .iter()
        .map(|k| notify::Pref {
            project_id: None,
            kind: k.to_string(),
            email: false,
            push: true,
        })
        .collect();
    notify::set_prefs(&t.db, 1, all_push.clone()).await.unwrap();
    assert_eq!(notify::prefs_for(&t.db, 1).await.unwrap().len(), 4);
    assert!(
        notify::webhook::create(&t.db, 1, "teams", &format!("{base}/hook"), None)
            .await
            .is_err()
    );
    let hook = notify::webhook::create(
        &t.db,
        1,
        "generic",
        &format!("{base}/hook"),
        Some("s3cret".into()),
    )
    .await
    .unwrap();

    let title = "<img src=x onerror=alert(1)> boom";
    post_message(&t, title).await;
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM notif_queue WHERE kind = 'new_issue'"
        )
        .await,
        1
    );
    let issue = q(&t.db, "SELECT issue_id FROM notif_queue").await;

    assert_eq!(notify::drain(&t.db).await.unwrap(), 1);
    let mails = outbox_for("New issue: <img src=x");
    assert_eq!(mails.len(), 1, "{mails:?}");
    assert_eq!(mails[0].0, "m@x.io");
    assert!(
        mails[0]
            .3
            .contains("&#60;img src=x onerror=alert(1)&#62; boom"),
        "{}",
        mails[0].3
    );
    assert!(!mails[0].3.contains("<img src=x"));
    assert!(mails[0].3.contains(&format!("/issues/{issue}")));

    let hooks: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.0 == "hook")
        .cloned()
        .collect();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].2["kind"], "new_issue");
    assert_eq!(hooks[0].2["issue"]["id"], issue);
    let sig = hooks[0].1["x-openfault-signature"]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        sig,
        notify::webhook::signature("s3cret", &serde_json::to_vec(&hooks[0].2).unwrap())
    );
    let relays: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.0 == "relay")
        .cloned()
        .collect();
    assert_eq!(relays.len(), 1);
    assert_eq!(relays[0].1["authorization"], "Bearer rt");
    assert_eq!(relays[0].2["tokens"], json!(["aa11", "bb22"]));
    assert_eq!(relays[0].2["collapse_id"], format!("issue-{issue}"));
    // 410 from the relay deletes the device.
    assert_eq!(q(&t.db, "SELECT count(*) FROM devices").await, 1);
    let log = notify::recent_deliveries(&t.db, 1, 10).await.unwrap();
    assert_eq!((log[0].status(), log[0].sent), ("sent", 3), "{log:?}");
    let hooks_now = notify::webhook::list(&t.db, 1).await.unwrap();
    assert!(hooks_now[0].last_ok_at.is_some() && hooks_now[0].has_secret);

    // dedupe: re-running the same row sends nothing on any channel.
    let before = seen.lock().unwrap().len();
    t.db.write(|c| c.execute("UPDATE notif_queue SET done_at = NULL, attempts = 0", []))
        .await
        .unwrap();
    assert_eq!(notify::drain(&t.db).await.unwrap(), 1);
    assert_eq!(outbox_for("New issue: <img src=x").len(), 1);
    assert_eq!(seen.lock().unwrap().len(), before);

    // regression: user 2 opted out, so no email; push + webhook still go.
    t.db.write(move |c| {
        c.execute(
            "UPDATE issues SET status = 'resolved' WHERE id = ?1",
            [issue],
        )
    })
    .await
    .unwrap();
    post_message(&t, title).await;
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM notif_queue WHERE kind = 'regression'"
        )
        .await,
        1
    );
    notify::drain(&t.db).await.unwrap();
    assert!(outbox_for("Regression: <img").is_empty());
    let s = seen.lock().unwrap().clone();
    assert_eq!(s.len(), before + 2);
    assert!(
        s.iter()
            .any(|x| x.0 == "hook" && x.2["kind"] == "regression")
    );
    assert!(
        s.iter()
            .any(|x| x.0 == "relay" && x.2["tokens"] == json!(["aa11"]))
    );

    assert!(notify::webhook::test_webhook(&t.db, hook).await.is_ok());
}

#[tokio::test]
async fn spike_fires_once_per_6h() {
    let t = setup().await;
    let hour = 1_700_000_000 / 3600 * 3600;
    let insert = |n: usize, at: i64| {
        let db = t.db.clone();
        async move {
            db.write(move |c| {
                for _ in 0..n {
                    let id = uuid::Uuid::now_v7().simple().to_string();
                    let ev = json!({"event_id": id, "platform": "node", "level": "error", "message": "burst"});
                    let body = zstd::encode_all(&serde_json::to_vec(&ev).unwrap()[..], 3).unwrap();
                    c.execute("INSERT INTO inbox (project_id, received_at, body) VALUES (1, ?1, ?2)", rusqlite::params![at, body])?;
                }
                Ok(())
            })
            .await
            .unwrap();
            wait_processed(&db).await;
        }
    };
    insert(25, hour + 60).await;
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM notif_queue WHERE kind = 'spike'"
        )
        .await,
        1
    );
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM notif_queue WHERE kind = 'new_issue'"
        )
        .await,
        1
    );
    // 2h later another burst (median still 0) is inside the 6h cooldown.
    insert(30, hour + 2 * 3600 + 60).await;
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM notif_queue WHERE kind = 'spike'"
        )
        .await,
        1
    );
    // 7h later it may fire again.
    insert(20, hour + 7 * 3600 + 60).await;
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM notif_queue WHERE kind = 'spike'"
        )
        .await,
        2
    );
}

#[test]
fn migrate_adds_missing_columns_once() {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE issues (id INTEGER PRIMARY KEY);
         CREATE TABLE webhooks (id INTEGER PRIMARY KEY, url TEXT);
         CREATE TABLE devices (id INTEGER PRIMARY KEY, push_token TEXT);",
    )
    .unwrap();
    notify::migrate(&c).unwrap();
    notify::migrate(&c).unwrap();
    c.execute(
        "INSERT INTO webhooks (url, secret, last_ok_at) VALUES ('u', 's', 1)",
        [],
    )
    .unwrap();
    c.execute("UPDATE issues SET last_spike_at = 1", [])
        .unwrap();
    c.execute("UPDATE devices SET name = 'x', token_id = 1", [])
        .unwrap();
}

#[tokio::test]
async fn failed_webhook_backs_off_and_queue_is_capped() {
    let (base, _) = stub().await;
    unsafe { std::env::set_var("OPENFAULT_ALLOW_PRIVATE_WEBHOOKS", "1") };
    let t = setup().await;
    notify::webhook::create(&t.db, 1, "generic", &format!("{base}/missing"), None)
        .await
        .unwrap();
    post_message(&t, "backoff probe").await;
    assert_eq!(notify::drain(&t.db).await.unwrap(), 1);
    let log = notify::recent_deliveries(&t.db, 1, 1).await.unwrap();
    assert_eq!(log[0].attempts, 1);
    assert!(log[0].last_error.as_deref().unwrap().contains("HTTP 404"));
    // attempt 2 waits a minute.
    assert_eq!(notify::drain(&t.db).await.unwrap(), 0);
    t.db.write(|c| c.execute("UPDATE notif_queue SET created_at = created_at - 61", []))
        .await
        .unwrap();
    assert_eq!(notify::drain(&t.db).await.unwrap(), 1);

    let issue = log[0].issue_id;
    let total =
        t.db.write(move |c| {
            for _ in 0..notify::MAX_PENDING + 5 {
                notify::enqueue(
                    c,
                    &notify::Notification::new(1, issue, "spike", "x", "error"),
                )?;
            }
            c.query_row(
                "SELECT count(*) FROM notif_queue WHERE done_at IS NULL AND attempts < 3",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(total, notify::MAX_PENDING);
}
