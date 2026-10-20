//! In-place upgrade: a database created by the last deployed schema (7a762e6) opens with current code.
use openfault::{AppState, app, db::Db, process};
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

#[tokio::test(flavor = "multi_thread")]
async fn old_schema_upgrades_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("fixtures/schema_7a762e6.sql"))
            .unwrap();
        c.execute_batch(&format!(
            "INSERT INTO orgs (id, name, slug, created_at) VALUES (1, 'O', 'o', 0);
             INSERT INTO users (email, pw_hash, is_owner, role, created_at) VALUES ('a@x.io', 'x', 1, 'owner', 0);
             INSERT INTO invites (token_hash, email, role, expires_at) VALUES ('h', 'b@x.io', 'member', 9999999999);
             INSERT INTO projects (id, org_id, slug, name, created_at) VALUES (1, 1, 'dev', 'dev', 0);
             INSERT INTO dsn_keys (project_id, public_key, created_at) VALUES (1, '{KEY}', 0);
             -- resolved-in-next-release from the old UI: no resolve_baseline column yet.
             INSERT INTO issues (id, project_id, fingerprint, title, status, resolved_next_release, first_seen, last_seen)
               VALUES (7, 1, 'f', 'old', 'resolved', 1, 0, 0);"
        ))
        .unwrap();
    }
    for _ in 0..2 {
        drop(Db::open(&path).unwrap()); // idempotent: second open re-runs every migration
    }
    let db = Db::open(&path).unwrap();
    let n = |sql: &'static str| {
        let db = db.clone();
        async move {
            db.read(move |c| c.query_row(sql, [], |r| r.get::<_, i64>(0)))
                .await
                .unwrap()
        }
    };
    assert_eq!(
        n("SELECT count(*) FROM sqlite_master WHERE name IN ('teams','team_members','team_projects','project_access','backups_log','mail_log','setup_token')").await,
        7
    );
    assert_eq!(
        n("SELECT count(*) FROM invites WHERE team_ids_json = '[]'").await,
        1
    );
    let st = AppState::new(db.clone());
    openfault::setup::ensure(&st).await.unwrap();
    assert_eq!(
        n("SELECT count(*) FROM setup_token").await,
        0,
        "users exist: no setup token"
    );
    // a new release materializes the legacy pending resolve instead of failing the ingest tx.
    tokio::spawn(process::run(db.clone(), st.notify.clone()));
    let body = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/message.envelope"
    ))
    .unwrap();
    let res = app(st)
        .oneshot(
            axum::http::Request::post("/api/1/envelope/")
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    for _ in 0..40 {
        if n("SELECT count(*) FROM inbox").await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(n("SELECT count(*) FROM inbox").await, 0);
    assert_eq!(
        n("SELECT count(*) FROM issues WHERE id = 7 AND resolved_next_release = 0 AND resolved_in_release IS NOT NULL").await,
        1
    );
}
