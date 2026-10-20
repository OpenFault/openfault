//! M3 UI: every page renders, actions work, event data is escaped.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use openfault::{AppState, app, db::Db, process};
use tower::ServiceExt;

struct Res {
    status: StatusCode,
    location: String,
    cookie: Option<String>,
    body: String,
}

async fn req(app: &Router, method: &str, uri: &str, cookie: &str, body: &str, ct: &str) -> Res {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "of.test")
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(r).await.unwrap();
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

const FORM: &str = "application/x-www-form-urlencoded";
const XSS1: &str = "<script>alert(1)</script>";
const XSS2: &str = "\"><img src=x onerror=alert(1)>";

fn event(key: &str) -> String {
    serde_json::json!({
        "event_id": key,
        "platform": "javascript",
        "level": "error",
        "release": "1.0.0",
        "environment": "production",
        "user": {"id": "u1", "email": "a@example.com"},
        "request": {"method": "POST", "url": "https://shop.test/pay", "headers": {"X-Probe": XSS2}, "data": {"x": XSS1}},
        "tags": {"route": XSS2},
        "breadcrumbs": {"values": [
            {"timestamp": 1.0, "category": "console", "message": format!("crumb {XSS1} {XSS2}")},
            {"timestamp": 2.0, "category": "ui.click", "message": XSS2, "data": {"html": XSS1}}
        ]},
        "exception": {"values": [{
            "type": "TypeError",
            "value": format!("boom {XSS1} {XSS2}"),
            "stacktrace": {"frames": [
                {"filename": "node_modules/lib/index.js", "function": "vendorFn", "lineno": 1, "in_app": false},
                {"filename": "app/checkout.js", "function": "submitOrder", "lineno": 42, "in_app": true,
                 "pre_context": ["a", "b", "c"], "context_line": format!("crash {XSS1}"), "post_context": ["d", "e", "f"]}
            ]}
        }]}
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_actions_and_escaping() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    let st = AppState::new(db.clone());
    tokio::spawn(process::run(db.clone(), st.notify.clone()));
    let app = app(st);

    let r = req(&app, "POST", "/setup", "", "org_name=Acme&name=Ann+Admin&email=ann@acme.io&password=ann+password1&project_name=Web+App&platform=javascript", FORM).await;
    let ann = r.cookie.unwrap();
    let key: String = db
        .read(|c| {
            c.query_row(
                "SELECT public_key FROM dsn_keys WHERE project_id = 1",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    let auth = format!("Sentry sentry_version=7, sentry_key={key}");
    for id in [
        "0a1b2c3d4e5f4a6b8c9d0e1f2a3b4c5d",
        "1a1b2c3d4e5f4a6b8c9d0e1f2a3b4c5d",
    ] {
        let r = Request::post("/api/1/store/")
            .header("x-sentry-auth", &auth)
            .body(Body::from(event(id)))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(r).await.unwrap().status(),
            StatusCode::OK
        );
    }
    for _ in 0..80 {
        let n: i64 = db
            .read(|c| c.query_row("SELECT count(*) FROM events", [], |r| r.get(0)))
            .await
            .unwrap();
        if n == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let id: i64 = db
        .read(|c| c.query_row("SELECT id FROM issues", [], |r| r.get(0)))
        .await
        .unwrap();

    let get = |uri: String, c: String| {
        let app = app.clone();
        async move { req(&app, "GET", &uri, &c, "", FORM).await }
    };
    let ok = |uri: String| {
        let ann = ann.clone();
        async move {
            let r = get(uri.clone(), ann).await;
            assert_eq!(r.status, StatusCode::OK, "{uri}");
            assert!(
                !r.body.contains(XSS1) && !r.body.contains(XSS2),
                "unescaped event data on {uri}"
            );
            r.body
        }
    };

    assert!(
        ok("/".into())
            .await
            .contains("Top issues by users affected")
    );
    assert!(ok("/?range=30d".into()).await.contains("last 30 days"));
    let list = ok("/issues".into()).await;
    assert!(list.contains(&format!("/issues/{id}")) && list.contains("class=\"ch bars s"));
    assert!(
        ok("/issues?q=is:resolved".into())
            .await
            .contains("Nothing resolved yet")
    );
    assert!(
        list.contains("sse-connect=\"/live?scope=all\"") && list.contains("hx-trigger=\"sse:tick")
    );
    assert!(
        ok("/projects/web-app/issues?q=level:error".into())
            .await
            .contains("boom")
    );
    for (tab, needle) in [
        ("stack", "submitOrder"),
        ("breadcrumbs", "ui.click"),
        ("request", "https://shop.test/pay"),
        ("tags", "a@example.com"),
        ("events", "0a1b2c3d4e5f4a6b8c9d0e1f2a3b4c5d"),
        ("activity", "No activity yet"),
        ("grouping", "default"),
    ] {
        let b = ok(format!("/issues/{id}?tab={tab}")).await;
        assert!(b.contains(needle), "{tab} lacks {needle}");
        // tab fragment: view-transition swap + exactly one sliding indicator
        assert!(
            b.contains("outerHTML transition:true") && b.matches(r#"class="ind""#).count() == 1
        );
    }
    let ev = ok(format!(
        "/issues/{id}/events/0a1b2c3d4e5f4a6b8c9d0e1f2a3b4c5d"
    ))
    .await;
    assert!(ev.contains("Newer") && ev.contains("Newest"));
    // releases: semver order, newest first (first_seen would say otherwise)
    db.write(|c| {
        c.execute(
            "INSERT INTO releases (project_id, version, is_semver, sort_key, first_seen)
             VALUES (1, '1.10.0', 1, '0000000001.0000000010.0000000000', 0)",
            [],
        )
    })
    .await
    .unwrap();
    let rel = ok("/releases".into()).await;
    let (a, b) = (rel.find(">1.10.0<").unwrap(), rel.find(">1.0.0<").unwrap());
    assert!(a < b, "releases not newest first");
    // TV mode: tiles + inline QR; single-project variant
    let tv = ok("/tv".into()).await;
    assert!(
        tv.contains("Unresolved") && tv.contains("Regressions") && tv.contains("<svg class=\"qr\"")
    );
    assert!(tv.contains("sse-connect=\"/live?scope=all\""));
    let tv1 = ok("/tv?project=web-app".into()).await;
    assert!(tv1.contains("Web App") && tv1.contains("scope=project:1"));
    assert_eq!(
        get("/tv?project=nope".into(), ann.clone()).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(ok("/settings/mcp".into()).await.contains("claude mcp add"));
    let notif = ok("/settings/notifications".into()).await;
    assert!(notif.contains("Regression"));
    // one project: an Email row and a Push row, 4 kind switches each
    assert_eq!(notif.matches("role=\"switch\"").count(), 8);
    for row in notif.split("class=\"ngrid\" role=\"group\"").skip(1) {
        let row = &row[..row.find("</div>").unwrap()];
        assert_eq!(row.matches("class=\"sw\"").count(), 4);
    }

    // Markdown export
    let md = get(format!("/issues/{id}.md"), ann.clone()).await;
    assert!(
        md.body.starts_with("# TypeError: boom")
            && md.body.contains("at submitOrder (app/checkout.js:42)")
    );
    assert!(!md.body.contains("vendorFn"));

    // saved views
    let r = req(
        &app,
        "POST",
        "/views",
        &ann,
        "name=Prod+errors&q=env%3Aproduction&sort=users",
        FORM,
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let page = ok("/issues".into()).await;
    assert!(
        page.contains("Prod errors") && page.contains("/issues?q=env%3Aproduction&#38;sort=users")
    );
    let vid: i64 = db
        .read(|c| c.query_row("SELECT id FROM saved_views", [], |r| r.get(0)))
        .await
        .unwrap();
    req(
        &app,
        "POST",
        &format!("/views/{vid}/delete"),
        &ann,
        "",
        FORM,
    )
    .await;
    assert!(!ok("/issues".into()).await.contains("Prod errors"));

    // actions + flash toast
    let r = req(
        &app,
        "POST",
        &format!("/issues/{id}/action"),
        &ann,
        "action=comment&text=looking+%3Cb%3E",
        FORM,
    )
    .await;
    let flash = r.cookie.unwrap();
    let b = get(
        format!("/issues/{id}?tab=activity"),
        format!("{ann}; {flash}"),
    )
    .await
    .body;
    assert!(b.contains("class=\"toast\"") && b.contains("looking &#60;b&#62;"));
    let r = req(
        &app,
        "POST",
        &format!("/issues/{id}/action"),
        &ann,
        "action=link_fix&url=javascript:alert(1)",
        FORM,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = req(
        &app,
        "POST",
        "/issues/bulk",
        &ann,
        &format!("id={id}&action=resolve&next=%2Fissues"),
        FORM,
    )
    .await;
    assert_eq!(r.location, "/issues");
    let status: String = db
        .read(move |c| {
            c.query_row("SELECT status FROM issues WHERE id = ?1", [id], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(status, "resolved");
    assert!(
        ok("/issues?q=is:resolved".into())
            .await
            .contains(&format!("/issues/{id}"))
    );

    // member: account only, styled 403 elsewhere
    let r = req(
        &app,
        "POST",
        "/settings/members/invite",
        &ann,
        "email=bob@acme.io&role=member&p=1",
        FORM,
    )
    .await;
    let i = r.body.find("http://of.test/invite/").unwrap() + "http://of.test/invite/".len();
    let tok = &r.body[i..i + r.body[i..].find('<').unwrap()];
    let bob = req(
        &app,
        "POST",
        &format!("/invite/{tok}"),
        "",
        "name=Bob&password=bobs+password",
        FORM,
    )
    .await
    .cookie
    .unwrap();
    let r = get("/settings/members".into(), bob.clone()).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("No access") && r.body.contains("app.css"));
    let r = get("/settings/account".into(), bob.clone()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains("href=\"/settings/members\""));
    assert_eq!(
        get(format!("/issues/{id}"), bob.clone()).await.status,
        StatusCode::OK
    );
    // members can't delete: styled 403, issue stays
    let r = get(format!("/issues/{id}/delete"), bob.clone()).await;
    assert!(r.status == StatusCode::FORBIDDEN && r.body.contains("No access"));
    let r = req(
        &app,
        "POST",
        &format!("/issues/{id}/delete"),
        &bob,
        "",
        FORM,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // off the project: every issue route is a 404, bulk changes nothing
    db.write(|c| c.execute("DELETE FROM project_members WHERE user_id != 1", []))
        .await
        .unwrap();
    let eid = "0a1b2c3d4e5f4a6b8c9d0e1f2a3b4c5d";
    for uri in [
        format!("/issues/{id}"),
        format!("/issues/{id}.md"),
        format!("/issues/{id}/events/{eid}"),
        format!("/issues/{id}/delete"),
    ] {
        assert_eq!(
            get(uri.clone(), bob.clone()).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    assert!(
        !get("/issues?q=".into(), bob.clone())
            .await
            .body
            .contains(&format!("data-id=\"{id}\""))
    );
    for (uri, body) in [
        (format!("/issues/{id}/action"), "action=approve".to_string()),
        (format!("/issues/{id}/delete"), String::new()),
        ("/issues/bulk".into(), format!("action=approve&id={id}")),
    ] {
        let r = req(&app, "POST", &uri, &bob, &body, FORM).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{uri}");
    }
    let approved: Option<i64> = db
        .read(move |c| {
            c.query_row("SELECT approved_at FROM issues WHERE id = ?1", [id], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(approved, None);

    // `next` with a tab can't turn into a protocol-relative redirect
    let r = req(
        &app,
        "POST",
        &format!("/issues/{id}/action"),
        &ann,
        "action=unresolve&next=/%09/evil.example",
        FORM,
    )
    .await;
    assert_eq!(r.location, format!("/issues/{id}"));
}

#[tokio::test]
async fn icons_sprite_and_chart() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(AppState::new(Db::open(&dir.path().join("t.db")).unwrap()));
    let r = app
        .oneshot(
            Request::get("/static/icons.svg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "image/svg+xml");
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&b).contains("<symbol id=\"i-check\""));

    let vals: Vec<i64> = (0..24).map(|i| (i * 7) % 5).collect();
    let html = openfault::ui::chart::area("t", &vals, 56).to_string();
    assert_eq!(html.matches("<path").count(), 1, "{html}");
    assert!(html.contains("<linearGradient id=\"gt\"") && html.contains("url(#gt)"));
    let bars = openfault::ui::chart::bars(&vals, &[], 16, false).to_string();
    assert_eq!(bars.matches("<i ").count(), 24);
    // degenerate series: empty, single point, all-zero, huge → finite numbers only
    for v in [vec![], vec![5], vec![0; 12], vec![i64::MAX / 4, 0, 1]] {
        let s = format!(
            "{}{}",
            openfault::ui::chart::area("d", &v, 40),
            openfault::ui::chart::bars(&v, &[], 16, true)
        );
        assert!(!s.contains("NaN") && !s.contains("inf"), "{s}");
    }
}

#[test]
fn css_uses_luma_preset() {
    let css = include_str!("../static/app.css");
    assert!(css.contains("--radius: 0.625rem"));
    assert!(!css.contains("Geist"));
}

async fn admin_app() -> (Router, String, Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    let app = app(AppState::new(db.clone()));
    let r = req(&app, "POST", "/setup", "", "org_name=Acme&name=Ann+Admin&email=ann@acme.io&password=ann+password1&project_name=Web+App&platform=javascript", FORM).await;
    (app, r.cookie.unwrap(), db, dir)
}

const HOOKS: &str = "/settings/projects/web-app/webhooks";

async fn add_hook(app: &Router, c: &str) -> i64 {
    let r = req(
        app,
        "POST",
        HOOKS,
        c,
        "kind=generic&url=https%3A%2F%2Fopenfault.invalid%2Fhook&secret=s3",
        FORM,
    )
    .await;
    assert_eq!(r.location, HOOKS);
    let page = req(app, "GET", HOOKS, c, "", FORM).await.body;
    let i = page.find("/webhooks/").unwrap() + "/webhooks/".len();
    page[i..].split('/').next().unwrap().parse().unwrap()
}

#[tokio::test]
async fn webhooks_list_and_deliveries() {
    let (app, c, db, _d) = admin_app().await;
    add_hook(&app, &c).await;
    db.write(|c| {
        c.execute(
            "INSERT INTO notif_queue (project_id, issue_id, kind, payload_json, created_at) VALUES (1, 1, 'new_issue', json_object('title', ?1), strftime('%s','now'))",
            [XSS1],
        )
    })
    .await
    .unwrap();
    let r = req(&app, "GET", HOOKS, &c, "", FORM).await;
    assert_eq!(r.status, StatusCode::OK);
    let b = r.body;
    assert!(b.contains("openfault.invalid/hook") && b.contains("generic") && b.contains("signed"));
    assert!(b.contains("Deliveries") && b.contains("pending") && b.contains("New issue"));
    assert!(!b.contains(XSS1), "delivery title must be escaped");
    assert!(!b.contains("s3"), "secret never rendered");
}

#[tokio::test]
async fn webhook_create_shows_validation_inline() {
    let (app, c, _db, _d) = admin_app().await;
    let r = req(
        &app,
        "POST",
        HOOKS,
        &c,
        "kind=slack&url=http%3A%2F%2Fexample.com%2Fx",
        FORM,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body.contains("webhook URL must use https") && r.body.contains("http://example.com/x")
    );
    let r = req(
        &app,
        "POST",
        HOOKS,
        &c,
        "kind=teams&url=https%3A%2F%2Fexample.com%2Fx",
        FORM,
    )
    .await;
    assert!(r.body.contains("kind must be slack|discord|generic"));
}

#[tokio::test]
async fn webhook_toggle_test_delete() {
    let (app, c, db, _d) = admin_app().await;
    let id = add_hook(&app, &c).await;
    let enabled = |db: Db| async move {
        db.read(move |c| {
            c.query_row("SELECT enabled FROM webhooks WHERE id = ?1", [id], |r| {
                r.get::<_, bool>(0)
            })
        })
        .await
        .unwrap()
    };
    let r = req(&app, "POST", &format!("{HOOKS}/{id}/toggle"), &c, "", FORM).await;
    assert_eq!(r.location, HOOKS);
    assert!(!enabled(db.clone()).await);
    req(
        &app,
        "POST",
        &format!("{HOOKS}/{id}/toggle"),
        &c,
        "enabled=1",
        FORM,
    )
    .await;
    assert!(enabled(db.clone()).await);
    let r = req(&app, "POST", &format!("{HOOKS}/{id}/test"), &c, "", FORM).await;
    assert_eq!(r.location, HOOKS);
    assert!(
        r.cookie.unwrap().starts_with("of_flash="),
        "test result is flashed"
    );
    assert_eq!(
        req(&app, "POST", &format!("{HOOKS}/999/test"), &c, "", FORM)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    req(&app, "POST", &format!("{HOOKS}/{id}/delete"), &c, "", FORM).await;
    assert_eq!(
        req(&app, "POST", &format!("{HOOKS}/{id}/delete"), &c, "", FORM)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn org_settings_show_push_relay() {
    let (app, c, _db, _d) = admin_app().await;
    let b = req(&app, "GET", "/settings", &c, "", FORM).await.body;
    assert!(b.contains("Push relay"));
}

#[tokio::test]
async fn notifications_use_notify_prefs() {
    let (app, c, db, _d) = admin_app().await;
    let r = req(
        &app,
        "POST",
        "/settings/notifications",
        &c,
        "e:1:spike=on&p:1:new_issue=on",
        FORM,
    )
    .await;
    assert_eq!(r.location, "/settings/notifications");
    let p = openfault::notify::prefs_for(&db, 1).await.unwrap();
    assert_eq!(p.len(), 4);
    assert!(p.iter().any(|x| x.kind == "spike" && x.email && !x.push));
    let b = req(&app, "GET", "/settings/notifications", &c, "", FORM)
        .await
        .body;
    assert_eq!(b.matches(" checked").count(), 2);
}

#[tokio::test]
async fn project_subnav_links_webhooks() {
    let (app, c, _db, _d) = admin_app().await;
    let b = req(&app, "GET", "/settings/projects/web-app", &c, "", FORM)
        .await
        .body;
    assert!(b.contains("href=\"/settings/projects/web-app/webhooks\""));
}
