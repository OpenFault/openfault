use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use openfault::{AppState, app, auth::auth_token, db::Db};
use tower::ServiceExt;

struct Res {
    status: StatusCode,
    location: String,
    cookie: Option<String>,
    body: String,
}

async fn req(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: &str,
    form: &str,
    extra: &[(&str, &str)],
) -> Res {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "of.test")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    for (k, v) in extra {
        r = r.header(*k, *v);
    }
    let res = app
        .clone()
        .oneshot(r.body(Body::from(form.to_string())).unwrap())
        .await
        .unwrap();
    let h = |k| {
        res.headers()
            .get(k)
            .map(|v: &header::HeaderValue| v.to_str().unwrap().to_string())
    };
    let (location, cookie) = (
        h(header::LOCATION).unwrap_or_default(),
        h(header::SET_COOKIE),
    );
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    Res {
        status,
        location,
        cookie: cookie.map(|c| c.split(';').next().unwrap().to_string()),
        body: String::from_utf8_lossy(&bytes).into(),
    }
}

fn between<'a>(s: &'a str, start: &str, end: char) -> &'a str {
    let i = s.find(start).expect(start) + start.len();
    &s[i..i + s[i..].find(end).unwrap()]
}

#[tokio::test]
async fn setup_invite_scoping_and_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    let st = AppState::new(db.clone());
    let app = app(st.clone());
    let get = |uri: &'static str, c: String| {
        let app = app.clone();
        async move { req(&app, "GET", uri, &c, "", &[]).await }
    };

    // fresh install → setup wizard
    let r = get("/", String::new()).await;
    assert_eq!(
        (r.status, r.location.as_str()),
        (StatusCode::SEE_OTHER, "/setup")
    );
    assert_eq!(get("/login", String::new()).await.location, "/setup");
    assert_eq!(get("/setup", String::new()).await.status, StatusCode::OK);

    let short = req(
        &app,
        "POST",
        "/setup",
        "",
        "org_name=Acme&name=Ann&email=ann@acme.io&password=short&project_name=Web&platform=node",
        &[],
    )
    .await;
    assert!(short.body.contains("at least 10"));
    let r = req(&app, "POST", "/setup", "", "org_name=Acme&name=Ann&email=ann%40acme.io&password=correct+horse&project_name=Web+App&platform=node", &[]).await;
    assert_eq!(r.location, "/setup/email?project=web-app");
    let ann = r.cookie.unwrap();
    assert!(ann.starts_with("of_session="));
    let sdk = get("/projects/web-app/setup-sdk", ann.clone()).await;
    assert!(
        sdk.body.contains("http://") && sdk.body.contains("@of.test/1"),
        "{}",
        sdk.body
    );
    assert_eq!(get("/setup", String::new()).await.location, "/login");

    // logout → login
    req(&app, "POST", "/logout", &ann, "", &[]).await;
    assert_eq!(get("/", ann.clone()).await.location, "/login");
    let bad = req(
        &app,
        "POST",
        "/login",
        "",
        "email=ann@acme.io&password=wrong-password",
        &[],
    )
    .await;
    assert!(bad.body.contains("Wrong email"));
    let r = req(
        &app,
        "POST",
        "/login",
        "",
        "email=ann@acme.io&password=correct+horse",
        &[],
    )
    .await;
    assert_eq!(r.location, "/");
    let ann = r.cookie.unwrap();
    assert_eq!(get("/", ann.clone()).await.status, StatusCode::OK);

    // CSRF: cross-site POST blocked
    let x = req(
        &app,
        "POST",
        "/settings/projects",
        &ann,
        "name=Evil&platform=node",
        &[("sec-fetch-site", "cross-site")],
    )
    .await;
    assert_eq!(x.status, StatusCode::FORBIDDEN);
    let x = req(
        &app,
        "POST",
        "/settings/projects",
        &ann,
        "name=Evil&platform=node",
        &[("origin", "https://evil.test")],
    )
    .await;
    assert_eq!(x.status, StatusCode::FORBIDDEN);

    // second project, then invite a member scoped to project 1 only
    let r = req(
        &app,
        "POST",
        "/settings/projects",
        &ann,
        "name=Secret&platform=ruby",
        &[("sec-fetch-site", "same-origin")],
    )
    .await;
    assert_eq!(r.location, "/settings/projects/secret");
    let r = req(
        &app,
        "POST",
        "/settings/members/invite",
        &ann,
        "email=bob@acme.io&role=member&p=1",
        &[],
    )
    .await;
    assert!(r.body.contains("SMTP not configured"));
    let tok = between(&r.body, "http://of.test/invite/", '<').to_string();
    let invite = format!("/invite/{tok}");
    let r = req(&app, "GET", &invite, "", "", &[]).await;
    assert!(r.body.contains("bob@acme.io"));
    let r = req(
        &app,
        "POST",
        &invite,
        "",
        "name=Bob&password=bobs+password",
        &[],
    )
    .await;
    assert_eq!(r.location, "/");
    let bob = r.cookie.unwrap();
    let again = req(
        &app,
        "POST",
        &invite,
        "",
        "name=Bob&password=bobs+password",
        &[],
    )
    .await;
    assert!(again.body.contains("expired"));

    // bob sees only Web App
    let r = get("/settings/projects", bob.clone()).await;
    assert!(r.body.contains("Web App") && !r.body.contains("Secret"));
    assert_eq!(
        get("/settings/projects/secret", bob.clone()).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get("/settings/projects/web-app", bob.clone()).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get("/settings/members", bob.clone()).await.status,
        StatusCode::FORBIDDEN
    );
    let r = req(
        &app,
        "POST",
        "/settings/tokens",
        &bob,
        "label=ci&scope=read&project=2",
        &[],
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "cannot scope a token to an invisible project"
    );

    // password change signs out bob's other sessions, keeps the current one
    let login = |pw: &'static str| {
        let app = app.clone();
        async move {
            req(
                &app,
                "POST",
                "/login",
                "",
                &format!("email=bob@acme.io&password={pw}"),
                &[],
            )
            .await
            .cookie
            .unwrap()
        }
    };
    let bob2 = login("bobs+password").await;
    let r = req(
        &app,
        "POST",
        "/settings/account/password",
        &bob,
        "current=bobs+password&new=bobs+new+password",
        &[],
    )
    .await;
    assert!(r.body.contains("Password changed."));
    assert_eq!(get("/", bob2).await.location, "/login");
    assert_eq!(get("/", bob.clone()).await.status, StatusCode::OK);

    // API token: create → resolves → revoke → gone
    let r = req(
        &app,
        "POST",
        "/settings/tokens",
        &bob,
        "label=ci&scope=write&project=1",
        &[],
    )
    .await;
    let token = format!("of_{}", between(&r.body, "<code>of_", '<'));
    assert_eq!(token.len(), 3 + 64);
    let t = auth_token(&st, &token).await.expect("token resolves");
    assert_eq!(
        (t.user_id, t.project_id, t.scopes.as_str()),
        (2, Some(1), "write")
    );
    assert!(auth_token(&st, "of_nope").await.is_none());
    let r = req(
        &app,
        "POST",
        &format!("/settings/tokens/{}/revoke", t.id),
        &bob,
        "",
        &[],
    )
    .await;
    assert_eq!(r.location, "/settings/tokens");
    assert!(auth_token(&st, &token).await.is_none());
}

#[tokio::test]
async fn login_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(AppState::new(Db::open(&dir.path().join("t.db")).unwrap()));
    for i in 1..=11 {
        let r = req(
            &app,
            "POST",
            "/login",
            "",
            "email=x%40y.z&password=nope",
            &[],
        )
        .await;
        if i <= 10 {
            assert_eq!(r.status, StatusCode::OK, "attempt {i}");
        } else {
            assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
            assert!(r.body.contains("Too many attempts"));
        }
    }
}
