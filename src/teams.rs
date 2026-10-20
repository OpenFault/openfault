//! Teams: org users grouped for project access. Grants live in team_members/team_projects; every
//! visibility/admin check reads the `project_access` view (schema.sql) = project_members ∪ team grants.
//! Also the REST surface `/api/v1/teams…` and `/api/v1/invites`.
use crate::{
    AppState,
    api::{ApiErr, Scope},
    auth::CurrentUser,
    db::{Db, now},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{Connection, OptionalExtension, Result, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Guarded upgrade for databases created before teams (tables themselves are `IF NOT EXISTS` in schema.sql).
pub fn migrate(c: &Connection) -> Result<()> {
    let has: bool = c.query_row(
        "SELECT count(*) > 0 FROM pragma_table_info('invites') WHERE name = 'team_ids_json'",
        [],
        |r| r.get(0),
    )?;
    if !has {
        c.execute(
            "ALTER TABLE invites ADD COLUMN team_ids_json TEXT NOT NULL DEFAULT '[]'",
            [],
        )?;
    }
    Ok(())
}

pub fn role(r: &str) -> &'static str {
    if r == "admin" { "admin" } else { "member" }
}

/// One team grant, as stored in `invites.team_ids_json`.
#[derive(Serialize, Deserialize, Clone)]
pub struct Grant {
    pub id: i64,
    pub role: String,
}

#[derive(Clone, Serialize)]
pub struct Team {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub members: i64,
    pub projects: i64,
    /// the viewer's role in this team, if any
    pub role: Option<String>,
    /// viewer is an org admin or this team's admin
    pub manage: bool,
}

const COLS: &str = "t.id, t.slug, t.name, (SELECT count(*) FROM team_members WHERE team_id = t.id),
    (SELECT count(*) FROM team_projects WHERE team_id = t.id), (SELECT role FROM team_members WHERE team_id = t.id AND user_id = ?1), ?2";
const SEES: &str =
    "(?2 OR EXISTS (SELECT 1 FROM team_members WHERE team_id = t.id AND user_id = ?1))";
fn row(r: &rusqlite::Row) -> Result<Team> {
    let role: Option<String> = r.get(5)?;
    let all: bool = r.get(6)?;
    Ok(Team {
        id: r.get(0)?,
        slug: r.get(1)?,
        name: r.get(2)?,
        members: r.get(3)?,
        projects: r.get(4)?,
        manage: all || role.as_deref() == Some("admin"),
        role,
    })
}

/// Teams `u` can see: every team for org owner/admin, else the ones they're in.
pub fn list(c: &Connection, u: &CurrentUser) -> Result<Vec<Team>> {
    c.prepare(&format!(
        "SELECT {COLS} FROM teams t WHERE {SEES} ORDER BY t.name"
    ))?
    .query_map(params![u.id, u.is_admin()], row)?
    .collect()
}

pub fn find(c: &Connection, u: &CurrentUser, slug: &str) -> Result<Option<Team>> {
    c.query_row(
        &format!("SELECT {COLS} FROM teams t WHERE {SEES} AND t.slug = ?3"),
        params![u.id, u.is_admin(), slug],
        row,
    )
    .optional()
}

/// New team; None if the slug is taken.
pub fn create(c: &Connection, name: &str) -> Result<Option<String>> {
    let slug = crate::ui::slugify(name);
    let n = c.execute(
        "INSERT OR IGNORE INTO teams (name, slug, created_at) VALUES (?1, ?2, ?3)",
        params![name, slug, now()],
    )?;
    Ok((n > 0).then_some(slug))
}

/// Add or re-role a team member.
pub fn set_member(c: &Connection, tid: i64, uid: i64, r: &str) -> Result<usize> {
    c.execute(
        "INSERT INTO team_members (team_id, user_id, role) VALUES (?1, ?2, ?3)
         ON CONFLICT (team_id, user_id) DO UPDATE SET role = iif(excluded.role = 'admin', 'admin', team_members.role)",
        params![tid, uid, role(r)],
    )
}

pub struct Person {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub role: String,
}
pub fn members(c: &Connection, tid: i64) -> Result<Vec<Person>> {
    c.prepare("SELECT u.id, u.name, u.email, m.role FROM team_members m JOIN users u ON u.id = m.user_id WHERE m.team_id = ?1 ORDER BY u.name, u.email")?
        .query_map([tid], |r| Ok(Person { id: r.get(0)?, name: r.get(1)?, email: r.get(2)?, role: r.get(3)? }))?
        .collect()
}
/// (id, slug, name)
pub fn projects(c: &Connection, tid: i64) -> Result<Vec<(i64, String, String)>> {
    c.prepare("SELECT p.id, p.slug, p.name FROM team_projects tp JOIN projects p ON p.id = tp.project_id WHERE tp.team_id = ?1 ORDER BY p.name")?
        .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect()
}

/// Org owner/admin, direct project admin, or admin of a team containing the project.
pub fn is_project_admin(c: &Connection, uid: i64, pid: i64) -> Result<bool> {
    c.query_row(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = ?1 AND role != 'member')
             OR EXISTS (SELECT 1 FROM project_access WHERE user_id = ?1 AND project_id = ?2 AND role = 'admin')",
        params![uid, pid],
        |r| r.get(0),
    )
}
pub async fn project_admin(db: &Db, u: &CurrentUser, pid: i64) -> crate::db::Result<bool> {
    let uid = u.id;
    db.read(move |c| is_project_admin(c, uid, pid)).await
}

/// Invite/re-invite: direct project grants (`[id]`) + team grants (`[{id, role}]`), both JSON.
/// Additive only: existing memberships are kept and roles only ever go up (member→admin).
pub fn apply_grants(c: &Connection, uid: i64, pids: &str, teams: &str) -> Result<()> {
    c.execute(
        "INSERT OR IGNORE INTO project_members (project_id, user_id, role)
         SELECT value, ?1, 'member' FROM json_each(?2) WHERE value IN (SELECT id FROM projects)",
        params![uid, pids],
    )?;
    c.execute(
        "INSERT INTO team_members (team_id, user_id, role)
         SELECT json_extract(value, '$.id'), ?1, iif(json_extract(value, '$.role') = 'admin', 'admin', 'member')
         FROM json_each(?2) WHERE json_extract(value, '$.id') IN (SELECT id FROM teams)
         ON CONFLICT (team_id, user_id) DO UPDATE SET role = iif(excluded.role = 'admin', 'admin', team_members.role)",
        params![uid, teams],
    )?;
    Ok(())
}

// ---------- REST ----------

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/teams", get(api_list).post(api_create))
        .route(
            "/teams/{slug}",
            get(api_get).patch(api_rename).delete(api_delete),
        )
        .route(
            "/teams/{slug}/members",
            post(api_add_member).delete(api_remove_member),
        )
        .route(
            "/teams/{slug}/projects",
            post(api_add_project).delete(api_remove_project),
        )
        .route("/invites", post(api_invite))
}

type R = std::result::Result<Json<Value>, ApiErr>;
fn bad(code: StatusCode, m: &str) -> ApiErr {
    ApiErr(code, m.into())
}
fn ie(e: impl std::fmt::Display) -> ApiErr {
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn org_admin(s: &Scope) -> std::result::Result<(), ApiErr> {
    s.writer()?;
    if s.user.is_admin() {
        Ok(())
    } else {
        Err(bad(StatusCode::FORBIDDEN, "owner/admin only"))
    }
}
/// Visible team or 404; with `manage`, also a write token and team-manager rights.
async fn team(
    st: &AppState,
    s: &Scope,
    slug: String,
    manage: bool,
) -> std::result::Result<Team, ApiErr> {
    let u = s.user.clone();
    let t = st
        .db
        .read(move |c| find(c, &u, &slug))
        .await
        .map_err(ie)?
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, "team not found"))?;
    if manage {
        s.writer()?;
        if !t.manage {
            return Err(bad(StatusCode::FORBIDDEN, "team admins only"));
        }
    }
    Ok(t)
}

async fn api_list(s: Scope, State(st): State<AppState>) -> R {
    let u = s.user.clone();
    Ok(Json(json!(
        st.db.read(move |c| list(c, &u)).await.map_err(ie)?
    )))
}

#[derive(Deserialize)]
struct NameB {
    name: String,
}
async fn api_create(s: Scope, State(st): State<AppState>, Json(b): Json<NameB>) -> R {
    org_admin(&s)?;
    let name = b.name.trim().to_string();
    if name.is_empty() {
        return Err(bad(StatusCode::UNPROCESSABLE_ENTITY, "name is required"));
    }
    let slug = st
        .db
        .write(move |c| create(c, &name))
        .await
        .map_err(ie)?
        .ok_or_else(|| bad(StatusCode::CONFLICT, "team slug already exists"))?;
    Ok(Json(json!({"slug": slug})))
}

async fn api_get(s: Scope, State(st): State<AppState>, Path(slug): Path<String>) -> R {
    let t = team(&st, &s, slug, false).await?;
    let tid = t.id;
    let (m, p) = st
        .db
        .read(move |c| Ok((members(c, tid)?, projects(c, tid)?)))
        .await
        .map_err(ie)?;
    let mut v = json!(t);
    v["members"] = json!(
        m.iter()
            .map(|m| json!({"id": m.id, "name": m.name, "email": m.email, "role": m.role}))
            .collect::<Vec<_>>()
    );
    v["projects"] = json!(
        p.iter()
            .map(|(id, slug, name)| json!({"id": id, "slug": slug, "name": name}))
            .collect::<Vec<_>>()
    );
    Ok(Json(v))
}

async fn api_rename(
    s: Scope,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Json(b): Json<NameB>,
) -> R {
    let t = team(&st, &s, slug, true).await?;
    let name = b.name.trim().to_string();
    if name.is_empty() {
        return Err(bad(StatusCode::UNPROCESSABLE_ENTITY, "name is required"));
    }
    st.db
        .write(move |c| {
            c.execute(
                "UPDATE teams SET name = ?1 WHERE id = ?2",
                params![name, t.id],
            )
        })
        .await
        .map_err(ie)?;
    Ok(Json(json!({"ok": true})))
}

async fn api_delete(s: Scope, State(st): State<AppState>, Path(slug): Path<String>) -> R {
    org_admin(&s)?;
    let t = team(&st, &s, slug, false).await?;
    st.db
        .write(move |c| c.execute("DELETE FROM teams WHERE id = ?1", [t.id]))
        .await
        .map_err(ie)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct MemberB {
    email: String,
    role: Option<String>,
}
async fn api_add_member(
    s: Scope,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Json(b): Json<MemberB>,
) -> R {
    let t = team(&st, &s, slug, true).await?;
    add_member(
        &st.db,
        t.id,
        &b.email,
        b.role.as_deref().unwrap_or("member"),
    )
    .await
    .map_err(|m| bad(StatusCode::UNPROCESSABLE_ENTITY, &m))?;
    Ok(Json(json!({"ok": true})))
}

/// Add an existing org user (by email) to a team. Shared with MCP.
pub async fn add_member(
    db: &Db,
    tid: i64,
    email: &str,
    r: &str,
) -> std::result::Result<(), String> {
    let (email, r) = (email.trim().to_lowercase(), role(r));
    let n = db
        .write(move |c| {
            c.execute(
                "INSERT INTO team_members (team_id, user_id, role) SELECT ?1, id, ?3 FROM users WHERE email = ?2
                 ON CONFLICT (team_id, user_id) DO UPDATE SET role = iif(excluded.role = 'admin', 'admin', team_members.role)",
                params![tid, email, r],
            )
        })
        .await
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("no org member with that email; invite them first".into());
    }
    Ok(())
}

async fn api_remove_member(
    s: Scope,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Json(b): Json<MemberB>,
) -> R {
    let t = team(&st, &s, slug, true).await?;
    let email = b.email.trim().to_lowercase();
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM team_members WHERE team_id = ?1 AND user_id = (SELECT id FROM users WHERE email = ?2)",
                params![t.id, email],
            )
        })
        .await
        .map_err(ie)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ProjectB {
    project: String,
}
/// Project in token scope that the user administers (so team admins can't pull in projects they only view).
async fn admin_pid(st: &AppState, s: &Scope, slug: &str) -> std::result::Result<i64, ApiErr> {
    let pid = s.project(slug)?.id;
    if !project_admin(&st.db, &s.user, pid).await.map_err(ie)? {
        return Err(bad(
            StatusCode::FORBIDDEN,
            "you are not an admin of that project",
        ));
    }
    Ok(pid)
}
async fn api_add_project(
    s: Scope,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Json(b): Json<ProjectB>,
) -> R {
    let t = team(&st, &s, slug, true).await?;
    let pid = admin_pid(&st, &s, &b.project).await?;
    st.db
        .write(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO team_projects (team_id, project_id) VALUES (?1, ?2)",
                [t.id, pid],
            )
        })
        .await
        .map_err(ie)?;
    Ok(Json(json!({"ok": true})))
}
async fn api_remove_project(
    s: Scope,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Json(b): Json<ProjectB>,
) -> R {
    let t = team(&st, &s, slug, true).await?;
    let pid = s.project(&b.project)?.id;
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM team_projects WHERE team_id = ?1 AND project_id = ?2",
                [t.id, pid],
            )
        })
        .await
        .map_err(ie)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize, JsonSchema)]
pub struct TeamRef {
    pub slug: String,
    /// member | admin (default member)
    pub role: Option<String>,
}
/// Team slugs → grants; error names the first unknown slug.
pub async fn resolve(db: &Db, refs: Vec<TeamRef>) -> std::result::Result<Vec<Grant>, String> {
    let all: Vec<(i64, String)> = db
        .read(|c| {
            c.prepare("SELECT id, slug FROM teams")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        })
        .await
        .map_err(|e| e.to_string())?;
    refs.into_iter()
        .map(|t| {
            let id = all
                .iter()
                .find(|(_, s)| *s == t.slug)
                .ok_or_else(|| format!("no team '{}'", t.slug))?
                .0;
            Ok(Grant {
                id,
                role: role(t.role.as_deref().unwrap_or("member")).into(),
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct InviteB {
    email: String,
    role: Option<String>,
    #[serde(default)]
    teams: Vec<TeamRef>,
    #[serde(default)]
    projects: Vec<String>,
}
async fn api_invite(
    s: Scope,
    State(st): State<AppState>,
    h: HeaderMap,
    Json(b): Json<InviteB>,
) -> R {
    org_admin(&s)?;
    let email = b.email.trim().to_lowercase();
    if !crate::ui::valid_email(&email) {
        return Err(bad(StatusCode::UNPROCESSABLE_ENTITY, "invalid email"));
    }
    let pids = b
        .projects
        .iter()
        .map(|p| s.project(p).map(|x| x.id))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let teams = resolve(&st.db, b.teams)
        .await
        .map_err(|m| bad(StatusCode::UNPROCESSABLE_ENTITY, &m))?;
    let base = crate::config::base_or(&st.db, h.get(header::HOST).and_then(|v| v.to_str().ok()));
    let link = crate::ui::create_invite(
        &st,
        s.user.id,
        &email,
        role(b.role.as_deref().unwrap_or("member")),
        pids,
        teams,
        &base,
    )
    .await
    .map_err(|_| ie("could not create invite"))?;
    Ok(Json(
        json!({"email": email, "link": link, "existing": link.is_none()}),
    ))
}
