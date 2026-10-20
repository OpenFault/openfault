//! Settings → MCP, Notifications and project Webhooks.
use super::{Me, Project, R, admin, base_url, flash, not_found, project_for, render};
use crate::{
    AppState,
    auth::CurrentUser,
    notify::{self, Delivery, Pref, webhook},
};
use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings/mcp", get(mcp))
        .route(
            "/settings/notifications",
            get(notifications).post(save_notifications),
        )
        .route(
            "/settings/projects/{slug}/webhooks",
            get(webhooks).post(create_webhook),
        )
        .route(
            "/settings/projects/{slug}/webhooks/{id}/{action}",
            post(webhook_action),
        )
}

#[derive(Template)]
#[template(path = "mcp.html")]
struct Mcp {
    me: Me,
    claude: String,
    mcp_json: String,
    cursor: String,
}

async fn mcp(me: Me, h: HeaderMap) -> R {
    let (claude, mcp_json, cursor) = crate::mcp::setup_snippets(&base_url(&h), "<your-token>");
    render(Mcp {
        me,
        claude,
        mcp_json,
        cursor,
    })
}

const KINDS: [(&str, &str); 4] = [
    ("new_issue", "New issue"),
    ("regression", "Regression"),
    ("unmuted", "Unmuted"),
    ("spike", "Spike"),
];

pub struct PrefRow {
    pub project_id: i64,
    pub name: String,
    /// (kind, email, push) per KINDS entry
    pub cells: Vec<(&'static str, bool, bool)>,
}

#[derive(Template)]
#[template(path = "notifications.html")]
struct Notifications {
    me: Me,
    kinds: [(&'static str, &'static str); 4],
    rows: Vec<PrefRow>,
}

async fn notifications(me: Me, State(st): State<AppState>) -> R {
    let prefs = notify::prefs_for(&st.db, me.id).await?;
    // project row, else project_id NULL row, else on
    let rows = me
        .projects
        .iter()
        .map(|p| PrefRow {
            project_id: p.id,
            name: p.name.clone(),
            cells: KINDS
                .iter()
                .map(|(k, _)| {
                    let row = prefs
                        .iter()
                        .find(|x| x.project_id == Some(p.id) && x.kind == *k)
                        .or_else(|| {
                            prefs
                                .iter()
                                .find(|x| x.project_id.is_none() && x.kind == *k)
                        });
                    (*k, row.is_none_or(|x| x.email), row.is_none_or(|x| x.push))
                })
                .collect(),
        })
        .collect();
    render(Notifications {
        me,
        kinds: KINDS,
        rows,
    })
}

/// Checkbox names: `e:{project_id}:{kind}` / `p:{project_id}:{kind}`; unchecked = absent.
async fn save_notifications(
    me: CurrentUser,
    State(st): State<AppState>,
    Form(f): Form<Vec<(String, String)>>,
) -> R {
    let projects: Vec<i64> = super::visible_projects(&st, &me)
        .await?
        .iter()
        .map(|p| p.id)
        .collect();
    let on = |pre: &str, pid: i64, k: &str| f.iter().any(|(n, _)| *n == format!("{pre}:{pid}:{k}"));
    // keep org-wide (project_id NULL) rows; set_prefs replaces everything
    let mut prefs: Vec<Pref> = notify::prefs_for(&st.db, me.id)
        .await?
        .into_iter()
        .filter(|p| p.project_id.is_none())
        .collect();
    for &pid in &projects {
        for (k, _) in KINDS {
            prefs.push(Pref {
                project_id: Some(pid),
                kind: k.into(),
                email: on("e", pid, k),
                push: on("p", pid, k),
            });
        }
    }
    notify::set_prefs(&st.db, me.id, prefs).await?;
    flash("/settings/notifications", "Notifications saved")
}

#[derive(Template)]
#[template(path = "webhooks.html")]
struct Webhooks {
    me: Me,
    project: Project,
    hooks: Vec<webhook::WebhookRow>,
    deliveries: Vec<Delivery>,
    error: Option<String>,
    kind: String,
    url: String,
}

async fn webhooks_page(
    me: Me,
    st: &AppState,
    project: Project,
    error: Option<String>,
    kind: String,
    url: String,
) -> R {
    let hooks = webhook::list(&st.db, project.id).await?;
    let deliveries = notify::recent_deliveries(&st.db, project.id, 50).await?;
    render(Webhooks {
        me,
        project,
        hooks,
        deliveries,
        error,
        kind,
        url,
    })
}

async fn webhooks(me: Me, State(st): State<AppState>, Path(slug): Path<String>) -> R {
    admin(&me)?;
    let p = project_for(&st, &me, slug).await?;
    webhooks_page(me, &st, p, None, "slack".into(), String::new()).await
}

#[derive(Deserialize)]
struct HookForm {
    kind: String,
    url: String,
    secret: Option<String>,
}

async fn create_webhook(
    me: Me,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Form(f): Form<HookForm>,
) -> R {
    admin(&me)?;
    let p = project_for(&st, &me, slug.clone()).await?;
    let url = f.url.trim().to_string();
    match webhook::create(&st.db, p.id, &f.kind, &url, f.secret).await {
        Ok(_) => flash(
            &format!("/settings/projects/{slug}/webhooks"),
            "Webhook added",
        ),
        Err(e) => webhooks_page(me, &st, p, Some(e), f.kind, url).await,
    }
}

async fn webhook_action(
    me: Me,
    State(st): State<AppState>,
    Path((slug, id, action)): Path<(String, i64, String)>,
    Form(f): Form<Vec<(String, String)>>,
) -> R {
    admin(&me)?;
    let p = project_for(&st, &me, slug.clone()).await?;
    let back = format!("/settings/projects/{slug}/webhooks");
    let found = match action.as_str() {
        "toggle" => {
            let on = f.iter().any(|(k, _)| k == "enabled");
            webhook::set_enabled(&st.db, p.id, id, on).await?
        }
        "delete" => webhook::delete(&st.db, p.id, id).await?,
        // test_webhook isn't project-scoped: check ownership first
        "test"
            if webhook::list(&st.db, p.id)
                .await?
                .iter()
                .any(|h| h.id == id) =>
        {
            let msg = match webhook::test_webhook(&st.db, id).await {
                Ok(()) => "Test delivered".to_string(),
                Err(e) => format!("Test failed: {e}"),
            };
            return flash(&back, &msg);
        }
        _ => false,
    };
    if !found {
        return Err(not_found());
    }
    flash(
        &back,
        if action == "delete" {
            "Webhook deleted"
        } else {
            "Webhook updated"
        },
    )
}
