//! Env vars override the stored settings; the form shows them as set by environment (own binary: env is process-wide).
mod common;
use axum::{
    body::Body,
    http::{Request, header},
};
use common::*;
use openfault::{AppState, app, db::Db};
use tower::ServiceExt;

#[tokio::test]
async fn env_overrides_disable_fields() {
    unsafe {
        std::env::set_var(
            "OPENFAULT_SMTP_URL",
            "smtp://u:p@smtp.env.example:587?from=noreply@env.example",
        );
        std::env::set_var("OPENFAULT_PUSH_RELAY_URL", "https://relay.env.example");
        std::env::set_var("OPENFAULT_PUSH_RELAY_TOKEN", "envtok");
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    let app = app(AppState::new(db.clone()));
    let send = |m: &str, uri: &str, ck: &str, b: &str| {
        Request::builder()
            .method(m)
            .uri(uri)
            .header(header::HOST, "of.test")
            .header(header::COOKIE, ck)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(b.to_string()))
            .unwrap()
    };
    let r = app.clone().oneshot(send("POST", "/setup", "", "org_name=Acme&name=Ann&email=ann@acme.io&password=correct+horse&project_name=Web&platform=node")).await.unwrap();
    let ck = r.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let r = app
        .clone()
        .oneshot(send("GET", "/settings", &ck, ""))
        .await
        .unwrap();
    let b = String::from_utf8(
        axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(b.contains("Set by environment (OPENFAULT_SMTP_URL)"));
    assert!(b.contains("Set by environment (OPENFAULT_PUSH_RELAY_URL"));
    assert!(b.contains("https://relay.env.example") && !b.contains("envtok"));
    assert_eq!(b.matches("<fieldset disabled>").count(), 2);
    // a crafted POST cannot overwrite env-owned sections
    app.clone()
        .oneshot(send(
            "POST",
            "/settings/server",
            &ck,
            "action=save&smtp_host=evil.example&relay_url=https://evil.example&relay_token=x",
        ))
        .await
        .unwrap();
    assert_eq!(q(&db, "SELECT json_type(settings_json, '$.smtp') = 'null' AND json_type(settings_json, '$.push_relay') = 'null' FROM orgs").await, 1);
    assert!(openfault::mail::configured(&db));
}
