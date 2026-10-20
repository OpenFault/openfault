//! Onboarding: setup token → REST / MCP bootstrap, admin MCP tools, wizard step 2.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use common::*;
use openfault::{AppState, app, db::Db};
use serde_json::{Value, json};
use tower::ServiceExt;

const ACCEPT: (&str, &str) = ("accept", "application/json, text/event-stream");
const PROTO: (&str, &str) = ("mcp-protocol-version", "2025-06-18");

fn fresh() -> T {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    T {
        app: app(AppState::new(db.clone())),
        db,
        _dir: dir,
    }
}

/// (status, location, set-cookie, body) for a form/GET request.
async fn form(
    t: &T,
    method: &str,
    uri: &str,
    cookie: &str,
    body: &str,
) -> (StatusCode, String, String, String) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "of.test")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = t.app.clone().oneshot(r).await.unwrap();
    let h = |k| {
        res.headers()
            .get(k)
            .map(|v: &header::HeaderValue| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    };
    let (loc, ck) = (h(header::LOCATION), h(header::SET_COOKIE));
    let s = res.status();
    let b = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        s,
        loc,
        ck.split(';').next().unwrap_or("").to_string(),
        String::from_utf8_lossy(&b).into(),
    )
}

async fn setup_token(t: &T) -> String {
    let (s, _, _, body) = form(t, "GET", "/setup", "", "").await;
    assert_eq!(s, StatusCode::OK);
    let i = body.find("ofs_").expect("setup token on /setup");
    let tok = &body[i..i + 68];
    assert!(tok[4..].chars().all(|c| c.is_ascii_hexdigit()), "{tok}");
    tok.to_string()
}

fn body() -> String {
    json!({"org": {"name": "Acme"}, "owner": {"email": "ann@acme.io", "name": "Ann", "password": "correct horse"},
           "project": {"name": "Web App", "platform": "node"}, "base_url": "https://errors.acme.io/"})
    .to_string()
}

async fn rpc(t: &T, token: &str, method: &str, params: Value) -> (StatusCode, Value) {
    let b = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let (s, _, v) = raw_req(t, "POST", "/mcp", token, Some(b), &[ACCEPT, PROTO]).await;
    (s, v)
}

async fn call(t: &T, token: &str, name: &str, args: Value) -> (bool, String) {
    let (s, v) = rpc(
        t,
        token,
        "tools/call",
        json!({"name": name, "arguments": args}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let r = &v["result"];
    (
        r["isError"].as_bool().unwrap_or(false),
        r["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

async fn tools(t: &T, token: &str) -> Vec<String> {
    let (s, v) = rpc(t, token, "tools/list", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn rest_setup_with_token() {
    let t = fresh();
    let tok = setup_token(&t).await;
    // same token on a second visit; wrong token refused
    assert_eq!(setup_token(&t).await, tok);
    let (s, _) = req(&t, "POST", "/api/v1/setup", "ofs_nope", Some(body())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let (s, v) = req(&t, "POST", "/api/v1/setup", &tok, Some(body())).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let api = v["api_token"].as_str().unwrap();
    assert!(api.starts_with("of_"));
    let dsn = v["dsn"].as_str().unwrap();
    assert!(
        dsn.starts_with("https://") && dsn.ends_with("@errors.acme.io/1"),
        "{dsn}"
    );
    assert_eq!(v["project"]["slug"], "web-app");
    assert_eq!(v["login_url"], "https://errors.acme.io/login");

    // api token works and sees the project; the DSN key ingests
    let (s, p) = req(&t, "GET", "/api/v1/projects", api, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(p.to_string().contains("web-app"));
    let key = dsn
        .trim_start_matches("https://")
        .split('@')
        .next()
        .unwrap();
    let ev = json!({"event_id": "0123456789abcdef0123456789abcdef", "message": "hi"}).to_string();
    let (s, _) = req(
        &t,
        "POST",
        &format!("/api/1/store/?sentry_key={key}"),
        "",
        Some(ev),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    assert_eq!(q(&t.db, "SELECT count(*) FROM setup_token").await, 0);
    let (s, _) = req(&t, "POST", "/api/v1/setup", &tok, Some(body())).await;
    assert_eq!(s, StatusCode::GONE);
    let (_, loc, _, _) = form(&t, "GET", "/setup", "", "").await;
    assert_eq!(loc, "/login");
}

#[tokio::test]
async fn mcp_setup_and_admin_tools() {
    let t = fresh();
    let tok = setup_token(&t).await;
    let init = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}});
    let (_, v) = rpc(&t, &tok, "initialize", init).await;
    assert!(
        v["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("setup_instance")
    );
    assert_eq!(tools(&t, &tok).await, ["setup_instance"]);
    // regular tools refuse the setup token
    let (err, _) = call(&t, &tok, "list_projects", json!({})).await;
    assert!(err);

    let args: Value = serde_json::from_str(&body()).unwrap();
    let (err, out) = call(&t, &tok, "setup_instance", args.clone()).await;
    assert!(!err, "{out}");
    let i = out.find("Bearer of_").unwrap() + 7;
    let api = out[i..].split_whitespace().next().unwrap().to_string();
    assert!(out.contains("DSN: https://"), "{out}");
    // setup token is dead now
    let (s, _) = rpc(&t, &tok, "tools/list", json!({})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let names = tools(&t, &api).await;
    for n in [
        "create_project",
        "invite_member",
        "configure_email",
        "send_test_email",
        "configure_push_relay",
        "list_members",
    ] {
        assert!(names.iter().any(|x| x == n), "{n} missing");
    }
    assert!(!names.iter().any(|x| x == "setup_instance"));

    let (err, out) = call(
        &t,
        &api,
        "create_project",
        json!({"name": "Mobile", "platform": "javascript"}),
    )
    .await;
    assert!(
        !err && out.contains("Created mobile. DSN: https://") && out.ends_with("/2"),
        "{out}"
    );
    let (err, out) = call(
        &t,
        &api,
        "create_project",
        json!({"name": "Mobile", "platform": "javascript"}),
    )
    .await;
    assert!(err && out.contains("already exists"));
    let (err, out) = call(
        &t,
        &api,
        "invite_member",
        json!({"email": "bob@acme.io", "project_slugs": ["mobile"]}),
    )
    .await;
    assert!(
        !err && out.contains("https://errors.acme.io/invite/"),
        "{out}"
    );
    let (err, out) = call(&t, &api, "list_members", json!({})).await;
    assert!(
        !err && out.contains("ann@acme.io") && out.contains("owner"),
        "{out}"
    );

    let smtp = json!({"host": "smtp.nonexistent.invalid", "port": 2525, "username": "u", "password": "s3cret-pw", "from": "OpenFault <noreply@acme.io>", "mode": "starttls"});
    let (err, out) = call(&t, &api, "configure_email", smtp).await;
    assert!(!err, "{out}");
    assert!(!out.contains("s3cret-pw"));
    let (err, out) = call(&t, &api, "send_test_email", json!({})).await;
    assert!(err && out.len() > 3, "{out}");
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM mail_log WHERE error IS NOT NULL"
        )
        .await,
        1
    );
    let (err, out) = call(
        &t,
        &api,
        "configure_email",
        json!({"host": "x", "from": "not an address"}),
    )
    .await;
    assert!(err && out.contains("from"), "{out}");
    let (err, _) = call(
        &t,
        &api,
        "configure_push_relay",
        json!({"url": "https://push.example.com", "token": "rt"}),
    )
    .await;
    assert!(!err);
    assert_eq!(
        q(
            &t.db,
            "SELECT json_extract(settings_json, '$.push_relay.token') = 'rt' FROM orgs"
        )
        .await,
        1
    );
    // the setup call cannot be replayed with the new api token
    let (err, _) = call(&t, &api, "setup_instance", args).await;
    assert!(err);
}

#[tokio::test]
async fn wizard_step_two_skip_save_and_test() {
    let t = fresh();
    let (_, loc, ck, _) = form(&t, "POST", "/setup", "", "org_name=Acme&name=Ann&email=ann@acme.io&password=correct+horse&project_name=Web+App&platform=node").await;
    assert_eq!(loc, "/setup/email?project=web-app");
    assert_eq!(q(&t.db, "SELECT count(*) FROM setup_token").await, 0);
    let (s, _, _, b) = form(&t, "GET", &loc, &ck, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        b.contains("Email &amp; notifications") && b.contains("Send test email to ann@acme.io")
    );
    assert!(
        b.contains("value=\"http://of.test\""),
        "base URL prefilled from Host"
    );
    assert!(b.contains("href=\"/setup/backup?project=web-app\">Skip"));
    assert!(b.contains("Amazon SES · us-east-1"));

    // save → continues to the SDK page and stores settings
    let f = "project=web-app&action=save&base_url=https://errors.acme.io&smtp_host=smtp.nonexistent.invalid&smtp_port=2525&smtp_username=u&smtp_password=s3cret-pw&smtp_from=noreply%40acme.io&smtp_mode=starttls&relay_url=&relay_token=";
    let (_, loc, _, _) = form(&t, "POST", "/settings/server", &ck, f).await;
    assert_eq!(loc, "/setup/backup?project=web-app");
    // step 3: backups (R2 preset, skippable), Finish → SDK page; secret never rendered back
    let (s, _, _, b) = form(&t, "GET", &loc, &ck, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.contains("Cloudflare R2") && b.contains("href=\"/projects/web-app/setup-sdk\">Skip"));
    let f = "project=web-app&action=save&enabled=1&bucket=bk&region=auto&access_key=AK&secret_key=zzz-secret&hour_utc=4&keep=7&path_style=1";
    let (_, loc, _, _) = form(&t, "POST", "/settings/backups", &ck, f).await;
    assert_eq!(loc, "/projects/web-app/setup-sdk");
    assert_eq!(
        q(
            &t.db,
            "SELECT json_extract(settings_json, '$.backup.secret_key') = 'zzz-secret' AND json_extract(settings_json, '$.backup.keep') = 7 FROM orgs"
        )
        .await,
        1
    );
    let (s, _, _, b) = form(&t, "GET", "/settings/backups", &ck, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.contains("•••• set") && !b.contains("zzz-secret") && b.contains("value=\"bk\""));
    // test connection with blank secret keeps it and reports the failure inline
    let f = "action=test&enabled=1&endpoint=http://127.0.0.1:1&bucket=bk&region=auto&access_key=AK&secret_key=";
    let (s, _, _, b) = form(&t, "POST", "/settings/backups", &ck, f).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.contains("Connection failed"), "inline error");
    assert_eq!(
        q(
            &t.db,
            "SELECT json_extract(settings_json, '$.backup.secret_key') = 'zzz-secret' FROM orgs"
        )
        .await,
        1
    );
    assert_eq!(
        q(
            &t.db,
            "SELECT json_extract(settings_json, '$.smtp.password') = 's3cret-pw' FROM orgs"
        )
        .await,
        1
    );
    let (_, _, _, b) = form(&t, "GET", "/projects/web-app/setup-sdk", &ck, "").await;
    assert!(
        b.contains("@errors.acme.io/1"),
        "DSN uses the saved base URL"
    );

    // Settings → Organization: same form, secret not rendered, blank password keeps it
    let (_, _, _, b) = form(&t, "GET", "/settings", &ck, "").await;
    assert!(b.contains("•••• set") && !b.contains("s3cret-pw"));
    assert!(b.contains("smtp.nonexistent.invalid"));
    let f = "action=test&has_allow=1&base_url=https://errors.acme.io&smtp_host=smtp.nonexistent.invalid&smtp_port=2525&smtp_username=u&smtp_password=&smtp_from=noreply%40acme.io&smtp_mode=starttls";
    let (s, _, _, b) = form(&t, "POST", "/settings/server", &ck, f).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.contains("Test email failed"), "inline error");
    assert!(b.contains("Recent email"));
    assert_eq!(
        q(
            &t.db,
            "SELECT json_extract(settings_json, '$.smtp.password') = 's3cret-pw' FROM orgs"
        )
        .await,
        1
    );
    // security none + credentials: warned, not blocked
    assert!(!b.contains("sent unencrypted"));
    let f = "action=save&has_allow=1&base_url=https://errors.acme.io&smtp_host=smtp.nonexistent.invalid&smtp_port=25&smtp_username=u&smtp_password=&smtp_from=noreply%40acme.io&smtp_mode=none";
    let (_, loc, _, _) = form(&t, "POST", "/settings/server", &ck, f).await;
    assert_eq!(loc, "/settings");
    let (_, _, _, b) = form(&t, "GET", "/settings", &ck, "").await;
    assert!(b.contains("credentials will be sent unencrypted"));
    // clearing the host turns email off
    let (_, loc, _, _) = form(
        &t,
        "POST",
        "/settings/server",
        &ck,
        "action=save&has_allow=1&base_url=https://errors.acme.io&smtp_host=",
    )
    .await;
    assert_eq!(loc, "/settings");
    assert_eq!(
        q(
            &t.db,
            "SELECT json_type(settings_json, '$.smtp') = 'null' FROM orgs"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn wizard_step_two_skip() {
    let t = fresh();
    let (_, _, ck, _) = form(&t, "POST", "/setup", "", "org_name=Acme&name=Ann&email=ann@acme.io&password=correct+horse&project_name=Web+App&platform=node").await;
    // skipping is a plain link: the SDK page works and nothing was stored
    let (s, _, _, b) = form(&t, "GET", "/projects/web-app/setup-sdk", &ck, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.contains("@of.test/1"));
    assert_eq!(
        q(
            &t.db,
            "SELECT json_type(settings_json, '$.smtp') IS NULL FROM orgs"
        )
        .await,
        1
    );
}
