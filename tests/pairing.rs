mod common;
use axum::http::StatusCode;
use common::*;
use openfault::auth::hash_token;
use serde_json::{Value, json};

/// Member user 2 with a web session cookie.
async fn session(t: &T) -> &'static str {
    let h = hash_token("sess2");
    t.db.write(move |c| {
        c.execute(
            "INSERT INTO sessions (id_hash, user_id, expires_at) VALUES (?1, 2, 9999999999)",
            [h],
        )
    })
    .await
    .unwrap();
    "of_session=sess2"
}

async fn start(t: &T, cookie: &str) -> (StatusCode, Value) {
    let (s, _, v) = raw_req(
        t,
        "POST",
        "/api/v1/pair/start",
        "",
        None,
        &[("cookie", cookie)],
    )
    .await;
    (s, v)
}

async fn claim(t: &T, code: &str, push: Option<&str>) -> (StatusCode, Value) {
    let body =
        json!({"code": code, "platform": "ios", "device_name": "Ada's iPhone", "push_token": push});
    req(t, "POST", "/api/v1/pair/claim", "", Some(body.to_string())).await
}

#[tokio::test]
async fn start_claim_and_single_use() {
    let t = setup().await;
    let ck = session(&t).await;
    let (s, p) = start(&t, ck).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let code = p["code"].as_str().unwrap();
    assert_eq!(code.len(), 8);
    assert!(!code.contains(['0', 'O', '1', 'I']));
    assert_eq!(
        p["qr_payload"],
        format!("openfault://pair?host=http%3A%2F%2Flocalhost&code={code}")
    );

    let (s, c) = claim(&t, &code.to_lowercase(), None).await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(
        c["user"],
        json!({"id": 2, "name": "Max", "email": "m@x.io"})
    );
    assert_eq!(
        c["projects"],
        json!([{"id": 1, "slug": "dev", "name": "dev"}])
    );
    let tok = c["token"].as_str().unwrap();
    let (s, me) = req(&t, "GET", "/api/v1/me", tok, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["id"], 2);
    assert_eq!(me["token"]["scopes"], "write");
    // no APNs permission yet: the device row still exists (NULL push_token) and owns the token.
    let id = c["device_id"].as_i64().unwrap();
    let sql = format!(
        "SELECT count(*) FROM devices WHERE id = {id} AND push_token IS NULL AND token_id IS NOT NULL"
    );
    assert_eq!(q(&t.db, &sql).await, 1);
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM api_tokens WHERE label = 'iOS: Ada''s iPhone'"
        )
        .await,
        1
    );
    // a paired phone is a person: full details without fix approval (MCP/other tokens stay gated).
    post_fixture(&t, "node_exception.envelope", 1).await;
    wait_processed(&t.db).await;
    let iid = q(&t.db, "SELECT max(id) FROM issues").await;
    let uri = format!("/api/v1/issues/{iid}?expand=frames,breadcrumbs");
    let (_, g) = req(&t, "GET", &uri, tok, None).await;
    assert!(g.get("fix_approved").is_none(), "{g}");
    let (_, g) = req(&t, "GET", &uri, MEMBER_W, None).await;
    assert_eq!(g["fix_approved"], false);

    let (s, e) = claim(&t, code, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(e["detail"], "pairing code not found");
    // bearer write token can start too; read-only and anonymous cannot.
    assert_eq!(
        req(&t, "POST", "/api/v1/pair/start", MEMBER_W, None)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        req(&t, "POST", "/api/v1/pair/start", ADMIN_R, None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        req(&t, "POST", "/api/v1/pair/start", "", None).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn expired_and_cancelled_codes_404() {
    let t = setup().await;
    let ck = session(&t).await;
    let a = start(&t, ck).await.1["code"].as_str().unwrap().to_string();
    t.db.write(|c| c.execute("UPDATE pairings SET expires_at = 1", []))
        .await
        .unwrap();
    assert_eq!(claim(&t, &a, None).await.0, StatusCode::NOT_FOUND);

    let b = start(&t, ck).await.1["code"].as_str().unwrap().to_string();
    let (s, _, _) = raw_req(
        &t,
        "DELETE",
        &format!("/api/v1/pair/{b}"),
        "",
        None,
        &[("cookie", ck)],
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(claim(&t, &b, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(claim(&t, "ZZZZZZZZ", None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sixth_open_code_rejected() {
    let t = setup().await;
    let ck = session(&t).await;
    for _ in 0..5 {
        assert_eq!(start(&t, ck).await.0, StatusCode::OK);
    }
    assert_eq!(start(&t, ck).await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn failed_claims_rate_limited() {
    let t = setup().await;
    for _ in 0..10 {
        assert_eq!(claim(&t, "ZZZZZZZZ", None).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        claim(&t, "ZZZZZZZZ", None).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn device_patch_and_delete_revokes_token() {
    let t = setup().await;
    let ck = session(&t).await;
    let code = start(&t, ck).await.1["code"].as_str().unwrap().to_string();
    let (_, c) = claim(&t, &code, None).await;
    // a second phone without APNs: NULL push_tokens don't collide.
    let code2 = start(&t, ck).await.1["code"].as_str().unwrap().to_string();
    assert_eq!(claim(&t, &code2, None).await.0, StatusCode::OK);
    let (tok, id) = (
        c["token"].as_str().unwrap(),
        c["device_id"].as_i64().unwrap(),
    );

    let (s, l) = req(&t, "GET", "/api/v1/me/devices", tok, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(l[0]["id"], id);
    assert_eq!(l[0]["name"], "Ada's iPhone");
    assert_eq!(l[0]["current"], true);
    assert_eq!(l.as_array().unwrap().len(), 2);
    assert_eq!(l[1]["current"], false);

    let patch = json!({"push_token": "apns-b", "name": "Pocket"}).to_string();
    let uri = format!("/api/v1/me/devices/{id}");
    assert_eq!(
        req(&t, "PATCH", &uri, tok, Some(patch)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM devices WHERE push_token = 'apns-b' AND name = 'Pocket'"
        )
        .await,
        1
    );

    assert_eq!(
        req(&t, "DELETE", &uri, MEMBER_W, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        req(&t, "GET", "/api/v1/me", tok, None).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn demo_pairing_gated_by_env() {
    let t = setup().await;
    let body = json!({"platform": "ios", "device_name": "Reviewer"}).to_string();
    unsafe { std::env::remove_var("OPENFAULT_DEMO_USER") };
    let (s, _) = req(&t, "POST", "/api/v1/pair/demo", "", Some(body.clone())).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    unsafe { std::env::set_var("OPENFAULT_DEMO_USER", "m@x.io") };
    let (s, c) = req(&t, "POST", "/api/v1/pair/demo", "", Some(body)).await;
    unsafe { std::env::remove_var("OPENFAULT_DEMO_USER") };
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["user"]["email"], "m@x.io");
    let (s, me) = req(&t, "GET", "/api/v1/me", c["token"].as_str().unwrap(), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["token"]["scopes"], "write");
    // the claim route still requires a code.
    let nocode = json!({"platform": "ios", "device_name": "x"}).to_string();
    let (s, _) = req(&t, "POST", "/api/v1/pair/claim", "", Some(nocode)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
