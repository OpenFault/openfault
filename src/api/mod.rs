//! REST API `/api/v1` (PLAN §7). Bearer `of_…` tokens; JSON in and out; errors are `{"detail": …}`.
use crate::{
    AppState,
    auth::{ApiToken, CurrentUser, auth_token},
    db::now,
    issues::{self, IssueAction, IssueFilter, ME, Range, Sort, untrusted_json},
    ui::{Project, visible_projects},
};
use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Query, State},
    http::{StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/projects", get(projects))
        .route("/projects/{slug}/issues", get(project_issues))
        .route("/projects/{slug}/releases", get(releases))
        .route("/projects/{slug}/stats", get(project_stats))
        .route("/projects/{slug}/members", get(members))
        .route("/issues", get(all_issues))
        .route("/issues/{id}", get(issue).post(act))
        .route("/issues/{id}/events", get(events))
        .route("/issues/{id}/markdown", get(markdown))
        .route("/events/{id}", get(event))
        .route("/stats", get(all_stats))
        .route("/me", get(me))
        .route("/me/notifications", get(get_prefs).put(put_prefs))
        .route("/me/devices", get(list_devices).post(add_device))
        .route("/me/devices/{id}", delete(del_device).patch(patch_device))
        .merge(crate::pairing::routes())
}

pub struct ApiErr(pub StatusCode, pub String);
impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::error!("api: {}", self.1);
        }
        (self.0, Json(json!({"detail": self.1}))).into_response()
    }
}
fn err(code: StatusCode, msg: &str) -> ApiErr {
    ApiErr(code, msg.into())
}
fn internal(e: impl std::fmt::Display) -> ApiErr {
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
type R<T = Json<Value>> = Result<T, ApiErr>;

/// An authenticated token: its user and the projects it may see.
#[derive(Clone)]
pub struct Scope {
    pub token: ApiToken,
    pub user: CurrentUser,
    /// visible_projects(user) ∩ token.project_id
    pub projects: Vec<Project>,
}

impl Scope {
    pub fn ids(&self) -> Vec<i64> {
        self.projects.iter().map(|p| p.id).collect()
    }
    pub fn can_write(&self) -> bool {
        self.token.scopes.split([',', ' ']).any(|s| s == "write")
    }
    pub fn writer(&self) -> Result<(), ApiErr> {
        if self.can_write() {
            Ok(())
        } else {
            Err(err(StatusCode::FORBIDDEN, "token is read-only"))
        }
    }
    pub fn project(&self, slug: &str) -> Result<&Project, ApiErr> {
        self.projects
            .iter()
            .find(|p| p.slug == slug)
            .ok_or_else(|| err(StatusCode::FORBIDDEN, "project not in token scope"))
    }
    /// 404 if the issue does not exist or is outside scope (same answer, so ids in other projects don't leak).
    pub async fn issue_project(&self, st: &AppState, id: i64) -> Result<i64, ApiErr> {
        let pid: Option<i64> = st
            .db
            .read(move |c| {
                c.query_row("SELECT project_id FROM issues WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()
            })
            .await
            .map_err(internal)?;
        pid.filter(|p| self.ids().contains(p))
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "issue not found"))
    }
}

/// Bearer header → Scope. Shared by REST and the MCP middleware.
pub async fn scope_from(st: &AppState, h: &axum::http::HeaderMap) -> Result<Scope, ApiErr> {
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
    let token = auth_token(st, bearer)
        .await
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "invalid token"))?;
    let uid = token.user_id;
    let user = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, email, name, role FROM users WHERE id = ?1",
                [uid],
                |r| {
                    Ok(CurrentUser {
                        id: r.get(0)?,
                        email: r.get(1)?,
                        name: r.get(2)?,
                        role: r.get(3)?,
                    })
                },
            )
        })
        .await
        .map_err(internal)?;
    let mut projects = visible_projects(st, &user)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "project lookup failed"))?;
    if let Some(p) = token.project_id {
        projects.retain(|x| x.id == p);
    }
    Ok(Scope {
        token,
        user,
        projects,
    })
}

impl FromRequestParts<AppState> for Scope {
    type Rejection = ApiErr;
    async fn from_request_parts(p: &mut Parts, st: &AppState) -> Result<Self, ApiErr> {
        scope_from(st, &p.headers).await
    }
}

fn safe(v: impl serde::Serialize) -> Value {
    let mut v = serde_json::to_value(v).unwrap_or(Value::Null);
    untrusted_json(&mut v);
    v
}

// ---------- projects / issues ----------

async fn projects(s: Scope) -> R {
    Ok(Json(json!(
        s.projects
            .iter()
            .map(|p| json!({"id": p.id, "slug": p.slug, "name": p.name, "platform": p.platform}))
            .collect::<Vec<_>>()
    )))
}

#[derive(Deserialize, Default)]
pub struct ListQ {
    status: Option<String>,
    level: Option<String>,
    release: Option<String>,
    env: Option<String>,
    assignee: Option<String>,
    approved: Option<String>,
    q: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    sort: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}

fn filter(s: &Scope, project_ids: Vec<i64>, q: ListQ) -> Result<IssueFilter, ApiErr> {
    let bad = |m: &str| err(StatusCode::BAD_REQUEST, m);
    let assignee_id = match q.assignee.as_deref() {
        None => None,
        Some("me") => Some(ME),
        Some(a) => Some(
            a.parse()
                .map_err(|_| bad("assignee must be 'me' or a user id"))?,
        ),
    };
    let approved = match q.approved.as_deref() {
        None => None,
        Some("yes" | "true") => Some(true),
        Some("no" | "false") => Some(false),
        _ => return Err(bad("approved must be yes|no")),
    };
    let sort = match q.sort.as_deref() {
        None => None,
        Some(x) => Some(
            Sort::parse(x).ok_or_else(|| bad("sort must be last_seen|first_seen|events|users"))?,
        ),
    };
    let base = IssueFilter {
        project_ids,
        status: q.status,
        level: q.level,
        release: q.release,
        environment: q.env,
        assignee_id,
        approved,
        since: q.since,
        until: q.until,
        sort,
        cursor: q.cursor,
        limit: q.limit.unwrap_or(25).min(100),
        text: None,
    };
    let mut f = issues::parse_query(q.q.as_deref().unwrap_or(""), base);
    if f.assignee_id == Some(ME) {
        f.assignee_id = Some(s.user.id);
    }
    Ok(f)
}

async fn list(st: &AppState, f: IssueFilter) -> R {
    if f.project_ids.is_empty() {
        return Ok(Json(
            json!({"untrusted": true, "issues": [], "next_cursor": null}),
        ));
    }
    let (rows, next) = issues::list_issues(&st.db, &f).await.map_err(internal)?;
    Ok(Json(
        json!({"untrusted": true, "issues": safe(rows), "next_cursor": next}),
    ))
}

async fn project_issues(
    s: Scope,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Query(q): Query<ListQ>,
) -> R {
    let pid = s.project(&slug)?.id;
    list(&st, filter(&s, vec![pid], q)?).await
}

async fn all_issues(s: Scope, State(st): State<AppState>, Query(q): Query<ListQ>) -> R {
    list(&st, filter(&s, s.ids(), q)?).await
}

#[derive(Deserialize)]
struct ExpandQ {
    expand: Option<String>,
}

/// Issue JSON: summary always; `expand` adds frames, breadcrumbs, request, tags, events.
pub async fn issue_json(st: &AppState, id: i64, expand: &[&str]) -> R<Value> {
    let d = issues::get_issue(&st.db, id)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "issue not found"))?;
    let has = |k| expand.contains(&k);
    let latest = d.latest_event.as_ref().map(|e| {
        let mut ev = json!({
            "id": e.id, "timestamp": e.timestamp, "release": e.release, "environment": e.environment,
            "level": e.level, "message": e.message, "user": e.user, "sdk": e.sdk, "symbolicated": e.symbolicated,
            "exceptions": e.exceptions.iter().map(|x| {
                let mut j = json!({"type": x.type_, "value": x.value, "mechanism": x.mechanism});
                if has("frames") { j["frames"] = json!(x.frames); }
                j
            }).collect::<Vec<_>>(),
        });
        if has("breadcrumbs") {
            ev["breadcrumbs"] = json!(e.breadcrumbs);
        }
        if has("request") {
            ev["request"] = json!(e.request);
        }
        if has("tags") {
            ev["tags"] = json!(e.tags);
        }
        ev
    });
    let mut out = json!({
        "untrusted": true,
        "issue": d.row,
        "resolved_in_release": d.resolved_in_release,
        "resolved_next_release": d.resolved_next_release,
        "mute_until": d.mute_until, "mute_after_count": d.mute_after_count, "mute_window_min": d.mute_window_min,
        "approved_by": d.approved_by,
        "fingerprint_hashes": d.fingerprint_hashes,
        "releases_seen": d.releases_seen,
        "environments": d.environments,
        "stored_count": d.stored_count,
        "activity": d.activity,
        "latest_event": latest,
    });
    if has("events") {
        out["events"] = json!(
            issues::list_events(&st.db, id, None, 25)
                .await
                .map_err(internal)?
                .0
        );
    }
    untrusted_json(&mut out);
    out["untrusted"] = true.into();
    Ok(out)
}

async fn issue(
    s: Scope,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ExpandQ>,
) -> R {
    s.issue_project(&st, id).await?;
    let ex = q.expand.unwrap_or_default();
    let mut ex: Vec<&str> = ex.split(',').map(str::trim).collect();
    let gated = |k: &&str| GATED.contains(k);
    if ex.iter().any(gated) && !approved(&st, id).await? {
        ex.retain(|k| !gated(k));
        return Ok(Json(not_approved(issue_json(&st, id, &ex).await?)));
    }
    Ok(Json(issue_json(&st, id, &ex).await?))
}

const GATED: [&str; 4] = ["frames", "breadcrumbs", "request", "events"];

async fn approved(st: &AppState, issue_id: i64) -> R<bool> {
    issues::fix_approved(&st.db, issue_id)
        .await
        .map_err(internal)
}

fn not_approved(mut v: Value) -> Value {
    v["fix_approved"] = false.into();
    v["detail"] = "Fix not approved. Ask a person to approve it in OpenFault.".into();
    v
}

/// Same text as the UI's `/issues/{id}.md`; unapproved → summary only.
async fn markdown(s: Scope, State(st): State<AppState>, Path(id): Path<i64>) -> R<Response> {
    s.issue_project(&st, id).await?;
    let d = issues::get_issue(&st.db, id)
        .await
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "issue not found"))?;
    let md = crate::ui::issue_markdown(&d, approved(&st, id).await?);
    Ok(([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], md).into_response())
}

/// Who an issue in this project can be assigned to (same rule as `issues::assignable`).
async fn members(s: Scope, State(st): State<AppState>, Path(slug): Path<String>) -> R {
    let pid = s.project(&slug)?.id;
    let rows: Vec<Value> = st
        .db
        .read(move |c| {
            c.prepare(
                "SELECT id, name, email, role FROM users WHERE role != 'member'
                   OR id IN (SELECT user_id FROM project_members WHERE project_id = ?1) ORDER BY name, email",
            )?
            .query_map([pid], |r| {
                Ok(json!({"id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?,
                    "email": r.get::<_, String>(2)?, "role": r.get::<_, String>(3)?}))
            })?
            .collect()
        })
        .await
        .map_err(internal)?;
    Ok(Json(json!(rows)))
}

async fn act(
    s: Scope,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    body: Result<Json<IssueAction>, axum::extract::rejection::JsonRejection>,
) -> R {
    s.writer()?;
    s.issue_project(&st, id).await?;
    let Json(a) = body.map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, &e.body_text()))?;
    let deleted = matches!(a, IssueAction::Delete);
    if deleted && !s.user.is_admin() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "only owners and admins can delete issues",
        ));
    }
    if let IssueAction::Assign { user_id: Some(u) } = a
        && !issues::assignable(&st.db, id, u).await.map_err(internal)?
    {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "user_id is not a member of this project",
        ));
    }
    issues::apply_action(&st.db, id, s.user.id, a)
        .await
        .map_err(internal)?;
    if deleted {
        return Ok(Json(json!({"deleted": id})));
    }
    Ok(Json(issue_json(&st, id, &[]).await?))
}

#[derive(Deserialize)]
struct PageQ {
    cursor: Option<String>,
    limit: Option<u32>,
}

async fn events(
    s: Scope,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<PageQ>,
) -> R {
    s.issue_project(&st, id).await?;
    let (rows, next) = issues::list_events(&st.db, id, q.cursor, q.limit.unwrap_or(25).min(100))
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({"untrusted": true, "events": safe(rows), "next_cursor": next}),
    ))
}

async fn event(
    s: Scope,
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ExpandQ>,
) -> R {
    let mut e = issues::get_event(&st.db, &id)
        .await
        .map_err(internal)?
        .filter(|e| s.ids().contains(&e.project_id))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "event not found"))?;
    if !approved(&st, e.issue_id).await? {
        let mut v = safe(e);
        for k in ["breadcrumbs", "request", "raw"] {
            v.as_object_mut().map(|o| o.remove(k));
        }
        for x in v["exceptions"].as_array_mut().into_iter().flatten() {
            x.as_object_mut().map(|o| o.remove("frames"));
        }
        v["untrusted"] = true.into();
        return Ok(Json(not_approved(v)));
    }
    // the raw payload is up to 1 MB; only on request.
    if !q
        .expand
        .as_deref()
        .unwrap_or("")
        .split(',')
        .any(|x| x.trim() == "raw")
    {
        e.raw = Value::Null;
    }
    let mut v = safe(e);
    v["untrusted"] = true.into();
    Ok(Json(v))
}

async fn releases(
    s: Scope,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Query(q): Query<PageQ>,
) -> R {
    let pid = s.project(&slug)?.id;
    let rows = issues::list_releases(&st.db, pid, q.limit.unwrap_or(25).min(100))
        .await
        .map_err(internal)?;
    Ok(Json(json!({"untrusted": true, "releases": safe(rows)})))
}

#[derive(Deserialize)]
struct RangeQ {
    range: Option<String>,
}
fn range(q: RangeQ) -> Result<Range, ApiErr> {
    Range::parse(q.range.as_deref().unwrap_or("day"))
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "range must be day|week|month"))
}

async fn stats_json(st: &AppState, ids: &[i64], r: Range) -> R {
    let s = issues::stats(&st.db, ids, r).await.map_err(internal)?;
    let mut v = safe(s);
    v["untrusted"] = true.into();
    Ok(Json(v))
}

async fn project_stats(
    s: Scope,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Query(q): Query<RangeQ>,
) -> R {
    let pid = s.project(&slug)?.id;
    stats_json(&st, &[pid], range(q)?).await
}

async fn all_stats(s: Scope, State(st): State<AppState>, Query(q): Query<RangeQ>) -> R {
    stats_json(&st, &s.ids(), range(q)?).await
}

// ---------- me ----------

async fn me(s: Scope) -> R {
    Ok(Json(json!({
        "id": s.user.id, "email": s.user.email, "name": s.user.name, "role": s.user.role,
        "token": {"scopes": s.token.scopes, "project_id": s.token.project_id},
    })))
}

use crate::notify::{KINDS, Pref};

async fn get_prefs(s: Scope, State(st): State<AppState>) -> R {
    let rows = crate::notify::prefs_for(&st.db, s.user.id)
        .await
        .map_err(internal)?;
    Ok(Json(json!(rows)))
}

/// Replaces all of the user's prefs with the given list.
async fn put_prefs(s: Scope, State(st): State<AppState>, Json(prefs): Json<Vec<Pref>>) -> R {
    s.writer()?;
    let ids = s.ids();
    for p in &prefs {
        if !KINDS.contains(&p.kind.as_str()) {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "kind must be new_issue|regression|unmuted|spike",
            ));
        }
        if p.project_id.is_some_and(|id| !ids.contains(&id)) {
            return Err(err(StatusCode::FORBIDDEN, "project not in token scope"));
        }
    }
    let rows = serde_json::to_value(&prefs).map_err(internal)?;
    crate::notify::set_prefs(&st.db, s.user.id, prefs)
        .await
        .map_err(internal)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct Device {
    platform: String,
    push_token: String,
}

async fn add_device(s: Scope, State(st): State<AppState>, Json(d): Json<Device>) -> R {
    s.writer()?;
    if d.push_token.is_empty() || d.push_token.len() > 512 || d.platform.len() > 32 {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "invalid device"));
    }
    let uid = s.user.id;
    let id = st
        .db
        .write(move |c| {
            // keyed by (user_id, push_token): another user's row with the same token is never touched.
            c.query_row(
                "INSERT INTO devices (user_id, platform, push_token, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (user_id, push_token) DO UPDATE SET platform = excluded.platform RETURNING id",
                params![uid, d.platform, d.push_token, now()],
                |r| r.get::<_, i64>(0),
            )
        })
        .await
        .map_err(internal)?;
    Ok(Json(json!({"id": id})))
}

async fn del_device(s: Scope, State(st): State<AppState>, Path(id): Path<i64>) -> R<StatusCode> {
    s.writer()?;
    let uid = s.user.id;
    let n = st
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            // a paired phone's token goes with its device.
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
            Ok(tid.is_some() as usize)
        })
        .await
        .map_err(internal)?;
    if n == 0 {
        Err(err(StatusCode::NOT_FOUND, "device not found"))
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

async fn list_devices(s: Scope, State(st): State<AppState>) -> R {
    let (uid, tid) = (s.user.id, s.token.id);
    let rows = st
        .db
        .read(move |c| {
            c.prepare("SELECT id, platform, name, created_at, token_id = ?2 FROM devices WHERE user_id = ?1 ORDER BY id")?
                .query_map(params![uid, tid], |r| {
                    Ok(json!({"id": r.get::<_, i64>(0)?, "platform": r.get::<_, String>(1)?,
                        "name": r.get::<_, Option<String>>(2)?, "created_at": r.get::<_, i64>(3)?,
                        "current": r.get::<_, Option<bool>>(4)?.unwrap_or(false)}))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .map_err(internal)?;
    Ok(Json(json!(rows)))
}

#[derive(Deserialize)]
struct DevicePatch {
    push_token: Option<String>,
    name: Option<String>,
}

/// Rotates the APNs token and/or renames the device.
async fn patch_device(
    s: Scope,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(d): Json<DevicePatch>,
) -> R {
    s.writer()?;
    if d.push_token
        .as_ref()
        .is_some_and(|p| p.is_empty() || p.len() > 512)
        || d.name.as_ref().is_some_and(|n| n.len() > 100)
    {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "invalid device"));
    }
    let uid = s.user.id;
    let n = st
        .db
        .write(move |c| {
            c.execute(
                "UPDATE devices SET push_token = coalesce(?3, push_token), name = coalesce(?4, name) WHERE id = ?1 AND user_id = ?2",
                params![id, uid, d.push_token, d.name],
            )
        })
        .await
        .map_err(|e| {
            if e.0.contains("UNIQUE") {
                err(StatusCode::CONFLICT, "push_token already registered on another device")
            } else {
                internal(e)
            }
        })?;
    if n == 0 {
        return Err(err(StatusCode::NOT_FOUND, "device not found"));
    }
    Ok(Json(json!({"id": id})))
}
