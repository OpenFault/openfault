//! Settings → MCP, Notifications and project Webhooks.
use super::{Me, Project, R, admin_project_for, base_url, flash, not_found, render};
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
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings/mcp", get(mcp))
        .route(
            "/settings/notifications",
            get(notifications).post(save_notifications),
        )
        .route("/settings/devices", get(devices))
        .route("/settings/devices/pair", post(pair))
        .route("/settings/devices/pair/{code}/status", get(pair_status))
        .route("/settings/devices/pair/{code}/cancel", post(pair_cancel))
        .route("/settings/devices/{id}/delete", post(delete_device))
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

async fn mcp(me: Me, State(st): State<AppState>, h: HeaderMap) -> R {
    let (claude, mcp_json, cursor) = crate::mcp::setup_snippets(&base_url(&st, &h), "<your-token>");
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
    health: notify::Health,
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
    let health = notify::project_health(&st.db, project.id).await?;
    render(Webhooks {
        me,
        project,
        hooks,
        deliveries,
        health,
        error,
        kind,
        url,
    })
}

async fn webhooks(me: Me, State(st): State<AppState>, Path(slug): Path<String>) -> R {
    let p = admin_project_for(&st, &me, slug).await?;
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
    let p = admin_project_for(&st, &me, slug.clone()).await?;
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
    let p = admin_project_for(&st, &me, slug.clone()).await?;
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

// ---------- devices (iOS pairing) ----------

pub struct DeviceRow {
    pub id: i64,
    pub platform: String,
    pub name: String,
    pub created_at: i64,
}

pub struct Pair {
    pub code: String,
    pub host: String,
    pub qr: String,
}

/// The swappable region: the pairing card (polling while pending) + the device list.
#[derive(Template)]
#[template(path = "devices_region.html")]
struct DevicesRegion {
    pair: Option<Pair>,
    notice: Option<String>,
    devices: Vec<DeviceRow>,
}

#[derive(Template)]
#[template(path = "devices.html")]
struct Devices {
    me: Me,
    pair: Option<Pair>,
    notice: Option<String>,
    devices: Vec<DeviceRow>,
}

async fn device_rows(st: &AppState, uid: i64) -> Result<Vec<DeviceRow>, super::E> {
    Ok(st
        .db
        .read(move |c| {
            c.prepare("SELECT id, platform, coalesce(name, platform), created_at FROM devices WHERE user_id = ?1 ORDER BY id")?
                .query_map([uid], |r| {
                    Ok(DeviceRow {
                        id: r.get(0)?,
                        platform: r.get(1)?,
                        name: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                })?
                .collect()
        })
        .await?)
}

fn pair_for(st: &AppState, code: String, h: &HeaderMap) -> Pair {
    let host = base_url(st, h);
    let qr = super::tv::qr(&format!(
        "openfault://pair?host={}&code={code}",
        super::enc(&host)
    ));
    Pair { code, host, qr }
}

async fn devices(me: Me, State(st): State<AppState>) -> R {
    let devices = device_rows(&st, me.id).await?;
    render(Devices {
        me,
        pair: None,
        notice: None,
        devices,
    })
}

async fn pair(me: Me, State(st): State<AppState>, h: HeaderMap) -> R {
    let p = crate::pairing::start_pairing(&st.db, me.id, &base_url(&st, &h))
        .await
        .map_err(|crate::api::ApiErr(s, m)| super::E(s, m))?;
    let devices = device_rows(&st, me.id).await?;
    render(Devices {
        me,
        pair: Some(pair_for(&st, p.code, &h)),
        notice: None,
        devices,
    })
}

/// Pending → same card (keeps polling); claimed → "Paired: name" + fresh list; gone → expired.
async fn pair_status(
    me: CurrentUser,
    State(st): State<AppState>,
    Path(code): Path<String>,
    h: HeaderMap,
) -> R {
    let (hash, uid) = (crate::auth::hash_token(&code), me.id);
    let row: Option<(Option<i64>, Option<String>)> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT p.claimed_at, d.name FROM pairings p LEFT JOIN devices d ON d.token_id = p.token_id
                 WHERE p.code_hash = ?1 AND p.user_id = ?2 AND (p.claimed_at IS NOT NULL OR p.expires_at > ?3)",
                params![hash, uid, crate::db::now()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })
        .await?;
    let (pair, notice) = match row {
        Some((None, _)) => (Some(pair_for(&st, code, &h)), None),
        Some((Some(_), name)) => (None, Some(format!("Paired: {}", name.unwrap_or_default()))),
        None => (None, Some("Pairing code expired".into())),
    };
    render(DevicesRegion {
        pair,
        notice,
        devices: device_rows(&st, uid).await?,
    })
}

async fn pair_cancel(me: CurrentUser, State(st): State<AppState>, Path(code): Path<String>) -> R {
    let (hash, uid) = (crate::auth::hash_token(&code), me.id);
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM pairings WHERE code_hash = ?1 AND user_id = ?2 AND claimed_at IS NULL",
                params![hash, uid],
            )
        })
        .await?;
    flash("/settings/devices", "Pairing cancelled")
}

/// ponytail: same SQL as api::del_device (a paired phone's token goes with it); that one is a Scope handler.
async fn delete_device(me: CurrentUser, State(st): State<AppState>, Path(id): Path<i64>) -> R {
    let uid = me.id;
    let found = st
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            let tid: Option<Option<i64>> = tx
                .query_row(
                    "DELETE FROM devices WHERE id = ?1 AND user_id = ?2 RETURNING token_id",
                    params![id, uid],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(Some(t)) = tid {
                tx.execute("DELETE FROM api_tokens WHERE id = ?1", [t])?;
            }
            tx.commit()?;
            Ok(tid.is_some())
        })
        .await?;
    if !found {
        return Err(not_found());
    }
    flash("/settings/devices", "Device removed")
}
