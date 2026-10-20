//! Settings → MCP and Settings → Notifications.
use super::{Me, R, base_url, flash, render};
use crate::{AppState, auth::CurrentUser};
use askama::Template;
use axum::{Form, Router, extract::State, http::HeaderMap, routing::get};
use rusqlite::params;

pub fn routes() -> Router<AppState> {
    Router::new().route("/settings/mcp", get(mcp)).route(
        "/settings/notifications",
        get(notifications).post(save_notifications),
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
    let uid = me.id;
    let prefs: Vec<(i64, String, bool, bool)> = st
        .db
        .read(move |c| {
            c.prepare("SELECT coalesce(project_id, 0), kind, email, push FROM notif_prefs WHERE user_id = ?1")?
                .query_map([uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect()
        })
        .await?;
    // no row = on (schema defaults)
    let rows = me
        .projects
        .iter()
        .map(|p| PrefRow {
            project_id: p.id,
            name: p.name.clone(),
            cells: KINDS
                .iter()
                .map(|(k, _)| {
                    let row = prefs.iter().find(|x| x.0 == p.id && x.1 == *k);
                    (*k, row.is_none_or(|x| x.2), row.is_none_or(|x| x.3))
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
    let rows: Vec<(i64, &str, bool, bool)> = projects
        .iter()
        .flat_map(|&pid| KINDS.iter().map(move |(k, _)| (pid, *k)))
        .map(|(pid, k)| (pid, k, on("e", pid, k), on("p", pid, k)))
        .collect();
    let uid = me.id;
    st.db
        .write(move |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM notif_prefs WHERE user_id = ?1 AND project_id IS NOT NULL", [uid])?;
            for (pid, k, e, p) in rows {
                tx.execute(
                    "INSERT INTO notif_prefs (user_id, project_id, kind, email, push) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![uid, pid, k, e, p],
                )?;
            }
            tx.commit()
        })
        .await?;
    flash("/settings/notifications", "Notifications saved")
}
