//! Teams: invite → team access, team admin rights, visibility via UI/API/MCP.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use common::*;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Form/GET as a browser session; returns (status, Location, Set-Cookie pair, body).
async fn ui(
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
        .header("sec-fetch-site", "same-origin")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = t.app.clone().oneshot(r).await.unwrap();
    let h = |k| {
        res.headers()
            .get(k)
            .map(|v: &header::HeaderValue| {
                v.to_str().unwrap().split(';').next().unwrap().to_string()
            })
            .unwrap_or_default()
    };
    let (loc, ck, s) = (h(header::LOCATION), h(header::SET_COOKIE), res.status());
    let b = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (s, loc, ck, String::from_utf8_lossy(&b).into())
}

async fn session(t: &T, uid: i64) -> String {
    let st = openfault::AppState::new(t.db.clone());
    openfault::auth::login(&st, uid)
        .await
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

fn slugs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| p["slug"].as_str().unwrap().to_string())
        .collect()
}

async fn mcp_text(t: &T, token: &str, name: &str, args: Value) -> String {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let (s, _, v) = raw_req(
        t,
        "POST",
        "/mcp",
        token,
        Some(body),
        &[
            ("accept", "application/json, text/event-stream"),
            ("mcp-protocol-version", "2025-06-18"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn teams_grant_access_everywhere() {
    let t = setup().await;
    let admin = session(&t, 1).await;

    // org admin creates a team holding project `other` (2)
    let (s, loc, _, _) = ui(&t, "POST", "/settings/teams", &admin, "name=Backend").await;
    assert_eq!(
        (s, loc.as_str()),
        (StatusCode::SEE_OTHER, "/settings/teams/backend")
    );
    let (s, ..) = ui(
        &t,
        "POST",
        "/settings/teams/backend/projects",
        &admin,
        "project_id=2",
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let tid = q(&t.db, "SELECT id FROM teams WHERE slug = 'backend'").await;
    let (s, _, _, page) = ui(&t, "GET", "/settings/teams/backend", &admin, "").await;
    assert!(s == StatusCode::OK && page.contains("Other") && page.contains("Add member"));
    assert!(
        ui(&t, "GET", "/settings/teams", &admin, "")
            .await
            .3
            .contains("Backend")
    );

    // invite as team admin → accept → sees the team's project only
    let (_, _, _, body) = ui(
        &t,
        "POST",
        "/settings/members/invite",
        &admin,
        &format!("email=tia@x.io&role=member&t={tid}&tr%3A{tid}=admin"),
    )
    .await;
    let i = body.find("http://of.test/invite/").expect("invite link") + "http://of.test".len();
    let link = body[i..i + body[i..].find('<').unwrap()].to_string();
    let (_, _, _, members) = ui(&t, "GET", "/settings/members", &admin, "").await;
    assert!(
        members.contains("Backend (admin)"),
        "pending invite shows its team"
    );
    let (s, _, tia, _) = ui(&t, "POST", &link, "", "name=Tia&password=long+enough+pw").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        ui(&t, "GET", "/projects/other/issues", &tia, "").await.0,
        StatusCode::OK
    );
    assert_eq!(
        ui(&t, "GET", "/projects/dev/issues", &tia, "").await.0,
        StatusCode::NOT_FOUND
    );

    // team admin: edits project settings + webhooks, manages team members, but no org settings
    let (s, _, _, page) = ui(&t, "GET", "/settings/projects/other", &tia, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("action=\"/settings/projects/other\"") && page.contains("Backend"));
    let (s, ..) = ui(
        &t,
        "POST",
        "/settings/projects/other",
        &tia,
        "name=Other2&platform=node&retention_days=30",
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        q(&t.db, "SELECT retention_days FROM projects WHERE id = 2").await,
        30
    );
    assert_eq!(
        ui(&t, "GET", "/settings/projects/other/webhooks", &tia, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        ui(&t, "GET", "/settings", &tia, "").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ui(&t, "GET", "/settings/members", &tia, "").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        ui(&t, "POST", "/settings/teams", &tia, "name=Rogue")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // can't pull in a project they don't administer (dev)
    assert_eq!(
        ui(
            &t,
            "POST",
            "/settings/teams/backend/projects",
            &tia,
            "project_id=1"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (s, ..) = ui(
        &t,
        "POST",
        "/settings/teams/backend/members",
        &tia,
        "user_id=2&role=member",
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);

    // member 2 (direct grant on dev) now also sees `other` — UI, REST and MCP
    let max = session(&t, 2).await;
    assert_eq!(
        ui(&t, "GET", "/projects/other/issues", &max, "").await.0,
        StatusCode::OK
    );
    assert_eq!(
        ui(&t, "GET", "/settings/projects/other", &max, "").await.0,
        StatusCode::OK
    );
    assert_eq!(
        ui(
            &t,
            "POST",
            "/settings/projects/other",
            &max,
            "name=x&platform=node&retention_days=1"
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
        "team member is not project admin"
    );
    let (_, v) = req(&t, "GET", "/api/v1/projects", MEMBER_W, None).await;
    assert_eq!(slugs(&v), ["other", "dev"]);
    assert!(
        mcp_text(&t, MEMBER_W, "list_projects", json!({}))
            .await
            .contains("other")
    );
    let (_, members) = ui(&t, "GET", "/settings/members", &admin, "")
        .await
        .3
        .split_once("Pending")
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .unwrap();
    assert!(members.contains("Backend"), "member list shows teams");

    // removing from the team removes visibility; direct grant stays
    let (s, ..) = ui(
        &t,
        "POST",
        "/settings/teams/backend/members/2/remove",
        &tia,
        "",
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let (_, v) = req(&t, "GET", "/api/v1/projects", MEMBER_W, None).await;
    assert_eq!(slugs(&v), ["dev"]);
    assert!(
        !mcp_text(&t, MEMBER_W, "list_projects", json!({}))
            .await
            .contains("other")
    );
    assert_eq!(
        ui(&t, "GET", "/projects/other/issues", &max, "").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        ui(&t, "GET", "/projects/dev/issues", &max, "").await.0,
        StatusCode::OK
    );

    // org member with no teams and no grants sees nothing
    t.db.write(|c| c.execute_batch(&format!(
        "INSERT INTO users (id, email, name, pw_hash, role, created_at) VALUES (9, 'n@x.io', 'Nod', 'x', 'member', 0);
         INSERT INTO api_tokens (token_hash, user_id, scopes) VALUES ('{}', 9, 'write');",
        openfault::auth::hash_token("of_nobody")))).await.unwrap();
    let (_, v) = req(&t, "GET", "/api/v1/projects", "of_nobody", None).await;
    assert_eq!(v, json!([]));
    let (_, v) = req(&t, "GET", "/api/v1/teams", "of_nobody", None).await;
    assert_eq!(v, json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn teams_api_and_mcp() {
    let t = setup().await;
    // REST: create, add member + project, list, rename; non-admin can't create
    let (s, v) = req(
        &t,
        "POST",
        "/api/v1/teams",
        ADMIN_W,
        Some(json!({"name": "Mobile"}).to_string()),
    )
    .await;
    assert_eq!((s, v["slug"].as_str()), (StatusCode::OK, Some("mobile")));
    assert_eq!(
        req(
            &t,
            "POST",
            "/api/v1/teams",
            MEMBER_W,
            Some(json!({"name": "X"}).to_string())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        req(
            &t,
            "POST",
            "/api/v1/teams",
            ADMIN_R,
            Some(json!({"name": "X"}).to_string())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (s, _) = req(
        &t,
        "POST",
        "/api/v1/teams/mobile/projects",
        ADMIN_W,
        Some(json!({"project": "other"}).to_string()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // members can't see a team they're not in
    assert_eq!(
        req(&t, "GET", "/api/v1/teams/mobile", MEMBER_W, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // re-invite an existing member: applies the team, no new invite
    let (s, v) = req(
        &t,
        "POST",
        "/api/v1/invites",
        ADMIN_W,
        Some(
            json!({"email": "m@x.io", "teams": [{"slug": "mobile", "role": "admin"}]}).to_string(),
        ),
    )
    .await;
    assert_eq!(
        (s, v["existing"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{v}"
    );
    assert_eq!(q(&t.db, "SELECT count(*) FROM invites").await, 0);
    let (_, v) = req(&t, "GET", "/api/v1/projects", MEMBER_W, None).await;
    assert_eq!(slugs(&v), ["other", "dev"]);
    // now a team admin: rename + manage members, view detail
    let (s, _) = req(
        &t,
        "PATCH",
        "/api/v1/teams/mobile",
        MEMBER_W,
        Some(json!({"name": "Mobile Apps"}).to_string()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = req(&t, "GET", "/api/v1/teams/mobile", MEMBER_W, None).await;
    assert_eq!(v["name"], "Mobile Apps");
    assert_eq!(v["projects"][0]["slug"], "other");
    assert_eq!(v["members"][0]["role"], "admin");
    // re-invite as plain member never lowers the team admin role or org role
    let (s, v) = req(
        &t,
        "POST",
        "/api/v1/invites",
        ADMIN_W,
        Some(json!({"email": "m@x.io", "role": "member", "teams": [{"slug": "mobile", "role": "member"}]}).to_string()),
    )
    .await;
    assert_eq!((s, v["existing"].as_bool()), (StatusCode::OK, Some(true)));
    let (_, v) = req(&t, "GET", "/api/v1/teams/mobile", MEMBER_W, None).await;
    assert_eq!(v["members"][0]["role"], "admin");
    // ...nor an org admin's role
    req(
        &t,
        "POST",
        "/api/v1/invites",
        ADMIN_W,
        Some(json!({"email": "a@x.io", "role": "member"}).to_string()),
    )
    .await;
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM users WHERE email = 'a@x.io' AND role = 'admin'"
        )
        .await,
        1
    );
    // team admin can't delete the team (org admin only)
    assert_eq!(
        req(&t, "DELETE", "/api/v1/teams/mobile", MEMBER_W, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    // new invite with teams + projects → pending row carries both
    let (s, v) = req(
        &t,
        "POST",
        "/api/v1/invites",
        ADMIN_W,
        Some(
            json!({"email": "z@x.io", "teams": [{"slug": "mobile"}], "projects": ["dev"]})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["link"].as_str().unwrap().contains("/invite/"));
    assert_eq!(
        q(
            &t.db,
            "SELECT count(*) FROM invites, json_each(team_ids_json) WHERE email = 'z@x.io'"
        )
        .await,
        1
    );
    assert_eq!(
        req(
            &t,
            "POST",
            "/api/v1/invites",
            ADMIN_W,
            Some(json!({"email": "q@x.io", "teams": [{"slug": "nope"}]}).to_string())
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // MCP: create_team (admin), add_to_team, list_teams, invite_member with teams/projects
    assert!(
        mcp_text(&t, ADMIN_W, "create_team", json!({"name": "Web"}))
            .await
            .contains("web")
    );
    let out = mcp_text(
        &t,
        ADMIN_W,
        "add_to_team",
        json!({"team": "web", "email": "m@x.io", "project": "dev"}),
    )
    .await;
    assert!(
        out.contains("added m@x.io") && out.contains("added project dev"),
        "{out}"
    );
    assert!(
        mcp_text(&t, MEMBER_W, "list_teams", json!({}))
            .await
            .contains("web")
    );
    let out = mcp_text(
        &t,
        MEMBER_W,
        "add_to_team",
        json!({"team": "web", "email": "a@x.io"}),
    )
    .await;
    assert!(out.contains("only team admins"), "{out}");
    let out = mcp_text(&t, ADMIN_W, "invite_member", json!({"email": "w@x.io", "teams": [{"slug": "web", "role": "admin"}], "projects": ["other"]})).await;
    assert!(out.contains("/invite/"), "{out}");

    // removing a project from a team drops visibility for token users
    let (s, _) = req(
        &t,
        "DELETE",
        "/api/v1/teams/mobile/projects",
        ADMIN_W,
        Some(json!({"project": "other"}).to_string()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = req(&t, "GET", "/api/v1/projects", MEMBER_W, None).await;
    assert_eq!(slugs(&v), ["dev"]);
    let (s, _) = req(&t, "DELETE", "/api/v1/teams/web", ADMIN_W, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(q(&t.db, "SELECT count(*) FROM team_members m JOIN teams t ON t.id = m.team_id WHERE t.slug = 'web'").await, 0);
}

#[tokio::test]
async fn deleted_team_is_dropped_from_pending_invites() {
    let dir = tempfile::tempdir().unwrap();
    let db = openfault::db::Db::open(&dir.path().join("t.db")).unwrap();
    let left: String = db
        .write(|c| {
            c.execute_batch(
                "INSERT INTO teams (id, name, slug, created_at) VALUES (1, 'A', 'a', 0), (2, 'B', 'b', 0);
                 INSERT INTO invites (token_hash, email, role, team_ids_json, expires_at)
                   VALUES ('h', 'x@x.io', 'member', '[{\"id\":1,\"role\":\"admin\"},{\"id\":2,\"role\":\"member\"}]', 9999999999);
                 DELETE FROM teams WHERE id = 2;
                 INSERT INTO teams (id, name, slug, created_at) VALUES (2, 'C', 'c', 0);",
            )?;
            c.query_row("SELECT team_ids_json FROM invites", [], |r| r.get(0))
        })
        .await
        .unwrap();
    assert_eq!(left, r#"[{"id":1,"role":"admin"}]"#);
}
