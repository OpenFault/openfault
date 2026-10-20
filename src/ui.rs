//! Server-rendered UI: setup wizard, auth pages, settings (PLAN §8, §9).
use crate::{
    AppState,
    auth::{self, CurrentUser},
    db::now,
    mail,
};
use askama::Template;
use axum::{
    Extension, Form, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use rusqlite::{OptionalExtension, params};
use rust_embed::RustEmbed;
use serde::Deserialize;
use tower_http::compression::CompressionLayer;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(home))
        .route("/setup", get(setup_page).post(setup))
        .route("/projects/{slug}/setup-sdk", get(setup_sdk))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route(
            "/forgot",
            get(|| async {
                render(Forgot {
                    sent: false,
                    smtp: false,
                })
            })
            .post(forgot),
        )
        .route("/reset/{token}", get(reset_page).post(reset))
        .route("/invite/{token}", get(invite_page).post(accept_invite))
        .route("/settings", get(settings).post(save_org))
        .route("/settings/members", get(members))
        .route("/settings/members/invite", post(invite))
        .route("/settings/members/{id}/role", post(set_role))
        .route("/settings/members/{id}/remove", post(remove_member))
        .route("/settings/projects", get(projects).post(create_project))
        .route("/settings/projects/{slug}", get(project).post(save_project))
        .route("/settings/projects/{slug}/keys", post(create_key))
        .route(
            "/settings/projects/{slug}/keys/{id}/revoke",
            post(revoke_key),
        )
        .route("/settings/tokens", get(tokens).post(create_token))
        .route("/settings/tokens/{id}/revoke", post(revoke_token))
        .route("/settings/account", get(account).post(save_account))
        .route("/settings/account/password", post(change_password))
        .route("/static/{*path}", get(asset))
        .layer(DefaultBodyLimit::max(64 * 1024)) // overrides the ingest-wide disable
        .layer(middleware::from_fn(auth::csrf))
        .layer(CompressionLayer::new())
}

// ---------- plumbing ----------

pub struct E(StatusCode, String);
impl<T: std::fmt::Display> From<T> for E {
    fn from(e: T) -> Self {
        E(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}
impl IntoResponse for E {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::error!("ui: {}", self.1);
        }
        (self.0, self.1).into_response()
    }
}
fn forbidden() -> E {
    E(StatusCode::FORBIDDEN, "forbidden".into())
}
fn not_found() -> E {
    E(StatusCode::NOT_FOUND, "not found".into())
}
type R = Result<Response, E>;

pub fn render(t: impl Template) -> R {
    Ok(Html(t.render()?).into_response())
}
fn to(path: &str) -> R {
    Ok(Redirect::to(path).into_response())
}
fn with_cookie(path: &str, cookie: String) -> R {
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to(path)).into_response())
}
fn admin(u: &CurrentUser) -> Result<(), E> {
    if u.is_admin() {
        Ok(())
    } else {
        Err(forbidden())
    }
}

/// OPENFAULT_BASE_URL, else http://{Host}.
pub fn base_url(h: &HeaderMap) -> String {
    std::env::var("OPENFAULT_BASE_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| {
            let host = h.get(header::HOST).and_then(|v| v.to_str().ok());
            format!("http://{}", host.unwrap_or("localhost:8080"))
        })
}

/// `{scheme}://{public_key}@{host}/{project_id}`
pub fn dsn(base: &str, key: &str, pid: i64) -> String {
    let (scheme, host) = base.split_once("://").unwrap_or(("http", base));
    format!("{scheme}://{key}@{host}/{pid}")
}

fn slugify(s: &str) -> String {
    let s: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() { "project".into() } else { s }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

const PLATFORMS: [&str; 5] = ["ruby", "javascript", "node", "python", "other"];
fn platform(p: &str) -> &str {
    if PLATFORMS.contains(&p) { p } else { "other" }
}

#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

async fn asset(Path(p): Path<String>) -> Response {
    let Some(f) = Assets::get(&p) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ct = match p.rsplit('.').next() {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    // ponytail: 1h cache, no fingerprinted names; add a ?v=hash when assets change often.
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        f.data,
    )
        .into_response()
}

// ---------- data ----------

#[derive(Clone)]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub platform: String,
    pub retention_days: i64,
    pub store_ip: bool,
}

const PROJECT_COLS: &str = "id, slug, name, coalesce(platform, ''), retention_days,
    coalesce(json_extract(settings_json, '$.store_ip'), 0)";
fn project_row(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        slug: r.get(1)?,
        name: r.get(2)?,
        platform: r.get(3)?,
        retention_days: r.get(4)?,
        store_ip: r.get::<_, i64>(5)? != 0,
    })
}
pub(crate) const VISIBLE: &str =
    "(?1 OR id IN (SELECT project_id FROM project_members WHERE user_id = ?2))";

/// Owner/admin: every project. Member: only projects in project_members.
pub async fn visible_projects(st: &AppState, u: &CurrentUser) -> Result<Vec<Project>, E> {
    let (all, uid) = (u.is_admin(), u.id);
    Ok(st
        .db
        .read(move |c| {
            c.prepare(&format!(
                "SELECT {PROJECT_COLS} FROM projects WHERE {VISIBLE} ORDER BY name"
            ))?
            .query_map(params![all, uid], project_row)?
            .collect()
        })
        .await?)
}

/// The project by slug, or 404 if it does not exist or `u` cannot see it.
pub async fn project_for(st: &AppState, u: &CurrentUser, slug: String) -> Result<Project, E> {
    let (all, uid) = (u.is_admin(), u.id);
    st.db
        .read(move |c| {
            c.query_row(
                &format!("SELECT {PROJECT_COLS} FROM projects WHERE {VISIBLE} AND slug = ?3"),
                params![all, uid, slug],
                project_row,
            )
            .optional()
        })
        .await?
        .ok_or_else(not_found)
}

fn insert_project(
    c: &rusqlite::Connection,
    name: &str,
    slug: &str,
    plat: &str,
) -> rusqlite::Result<i64> {
    c.execute(
        "INSERT INTO projects (org_id, slug, name, platform, created_at) VALUES (1, ?1, ?2, ?3, ?4)",
        params![slug, name, plat, now()],
    )?;
    let pid = c.last_insert_rowid();
    c.execute(
        "INSERT INTO dsn_keys (project_id, public_key, created_at) VALUES (?1, ?2, ?3)",
        params![pid, hex(&auth::random::<16>()), now()],
    )?;
    Ok(pid)
}

fn valid_email(e: &str) -> bool {
    e.len() <= 254
        && e.split_once('@')
            .is_some_and(|(a, b)| !a.is_empty() && b.contains('.'))
}

// ---------- setup + auth pages ----------

#[derive(Template)]
#[template(path = "setup.html")]
struct Setup {
    error: Option<String>,
}

async fn users_exist(st: &AppState) -> Result<bool, E> {
    Ok(st
        .db
        .read(|c| c.query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |r| r.get(0)))
        .await?)
}

async fn setup_page(State(st): State<AppState>) -> R {
    if users_exist(&st).await? {
        return to("/login");
    }
    render(Setup { error: None })
}

#[derive(Deserialize)]
struct SetupForm {
    org_name: String,
    name: String,
    email: String,
    password: String,
    project_name: String,
    platform: String,
}

async fn setup(State(st): State<AppState>, Form(f): Form<SetupForm>) -> R {
    let email = f.email.trim().to_lowercase();
    let err = if f.password.len() < 10 {
        Some("Password must be at least 10 characters.")
    } else if !valid_email(&email) {
        Some("Enter a valid email.")
    } else if f.org_name.trim().is_empty() || f.project_name.trim().is_empty() {
        Some("Organization and project names are required.")
    } else {
        None
    };
    if let Some(e) = err {
        return render(Setup {
            error: Some(e.into()),
        });
    }
    let pw = auth::hash_password(f.password).await;
    let done = st
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            if tx.query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |r| r.get(0))? {
                return Ok(None);
            }
            let org = f.org_name.trim();
            tx.execute(
                "INSERT INTO orgs (id, name, slug, created_at) VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, slug = excluded.slug",
                params![org, slugify(org), now()],
            )?;
            tx.execute(
                "INSERT INTO users (email, name, pw_hash, is_owner, role, created_at) VALUES (?1, ?2, ?3, 1, 'owner', ?4)",
                params![email, f.name.trim(), pw, now()],
            )?;
            let uid = tx.last_insert_rowid();
            let slug = slugify(&f.project_name);
            insert_project(&tx, f.project_name.trim(), &slug, platform(&f.platform))?;
            tx.commit()?;
            Ok(Some((uid, slug)))
        })
        .await?;
    let Some((uid, slug)) = done else {
        return to("/login");
    };
    with_cookie(
        &format!("/projects/{slug}/setup-sdk"),
        auth::login(&st, uid).await?,
    )
}

#[derive(Template)]
#[template(path = "setup_sdk.html")]
struct SetupSdk {
    me: CurrentUser,
    project: Project,
    dsn: String,
}

async fn first_key(st: &AppState, pid: i64) -> Result<String, E> {
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT public_key FROM dsn_keys WHERE project_id = ?1 AND active = 1 ORDER BY id LIMIT 1",
                [pid],
                |r| r.get(0),
            )
            .optional()
        })
        .await?
        .unwrap_or_default())
}

async fn setup_sdk(
    me: CurrentUser,
    State(st): State<AppState>,
    h: HeaderMap,
    Path(slug): Path<String>,
) -> R {
    let project = project_for(&st, &me, slug).await?;
    let dsn = dsn(
        &base_url(&h),
        &first_key(&st, project.id).await?,
        project.id,
    );
    render(SetupSdk { me, project, dsn })
}

#[derive(Template)]
#[template(path = "login.html")]
struct Login {
    error: Option<String>,
}

async fn login_page(State(st): State<AppState>) -> R {
    if !users_exist(&st).await? {
        return to("/setup");
    }
    render(Login { error: None })
}

#[derive(Deserialize)]
struct LoginForm {
    email: String,
    password: String,
}

type Peer = Option<Extension<ConnectInfo<std::net::SocketAddr>>>;

/// 10 attempts per 15 min per client IP, shared by /login and /forgot.
fn limited(st: &AppState, h: &HeaderMap, peer: Peer) -> bool {
    let xff = (std::env::var("OPENFAULT_TRUST_PROXY").as_deref() == Ok("1"))
        .then(|| h.get("x-forwarded-for")?.to_str().ok()?.split(',').next())
        .flatten()
        .map(|s| s.trim().to_string());
    let ip = xff
        .or(peer.map(|Extension(ConnectInfo(a))| a.ip().to_string()))
        .unwrap_or_default();
    let mut m = st.attempts.lock().unwrap();
    // ponytail: wiped wholesale past 10k IPs; an LRU if that ever matters.
    if m.len() > 10_000 {
        m.clear();
    }
    let t = now();
    let e = m.entry(ip).or_insert((0, t));
    if t - e.1 >= 900 {
        *e = (0, t);
    }
    e.0 += 1;
    e.0 > 10
}

const TOO_MANY: &str = "Too many attempts, try again later";

async fn login(
    State(st): State<AppState>,
    peer: Peer,
    h: HeaderMap,
    Form(f): Form<LoginForm>,
) -> R {
    if limited(&st, &h, peer) {
        let page = Login {
            error: Some(TOO_MANY.into()),
        };
        return Ok((StatusCode::TOO_MANY_REQUESTS, render(page)?).into_response());
    }
    let email = f.email.trim().to_lowercase();
    let row: Option<(i64, String)> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, pw_hash FROM users WHERE email = ?1",
                [email],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })
        .await?;
    let hash = row
        .as_ref()
        .map_or_else(|| auth::DUMMY_HASH.clone(), |r| r.1.clone());
    if auth::verify_password(f.password, hash).await
        && let Some((uid, _)) = row
    {
        return with_cookie("/", auth::login(&st, uid).await?);
    }
    render(Login {
        error: Some("Wrong email or password.".into()),
    })
}

async fn logout(State(st): State<AppState>, h: HeaderMap) -> R {
    if let Some(hash) = auth::cookie(&h, auth::COOKIE).map(auth::hash_token) {
        st.db
            .write(move |c| c.execute("DELETE FROM sessions WHERE id_hash = ?1", [hash]))
            .await?;
    }
    with_cookie("/login", auth::clear_cookie())
}

#[derive(Template)]
#[template(path = "forgot.html")]
struct Forgot {
    sent: bool,
    smtp: bool,
}

#[derive(Deserialize)]
struct EmailForm {
    email: String,
}

async fn forgot(
    State(st): State<AppState>,
    peer: Peer,
    h: HeaderMap,
    Form(f): Form<EmailForm>,
) -> R {
    if limited(&st, &h, peer) {
        let page = Message {
            title: "Reset password",
            body: TOO_MANY,
        };
        return Ok((StatusCode::TOO_MANY_REQUESTS, render(page)?).into_response());
    }
    let email = f.email.trim().to_lowercase();
    let tok = auth::new_token();
    let th = auth::hash_token(&tok);
    let e2 = email.clone();
    let found = st
        .db
        .write(move |c| {
            c.execute(
                "INSERT INTO password_resets (token_hash, user_id, expires_at)
                 SELECT ?1, id, ?2 FROM users WHERE email = ?3",
                params![th, now() + 3600, e2],
            )
        })
        .await?
        > 0;
    // without OPENFAULT_BASE_URL the link comes from the request's Host header, which an
    // anonymous caller controls (reset-link poisoning), so never email it.
    let mail_ok = mail::configured() && std::env::var("OPENFAULT_BASE_URL").is_ok();
    if found {
        let link = format!("{}/reset/{tok}", base_url(&h));
        if mail_ok {
            let text = format!("Reset your OpenFault password (valid 1 hour):\n\n{link}\n");
            let html = format!(
                "<p>Reset your OpenFault password (valid 1 hour):</p><p><a href=\"{link}\">{link}</a></p>"
            );
            mail::send_mail(&st, &email, "Reset your OpenFault password", &text, &html).await;
        } else {
            // deviation: never show a reset link to an anonymous visitor; without SMTP the admin reads it from the log.
            tracing::warn!(
                "SMTP or OPENFAULT_BASE_URL not configured; password reset link for {email}: {link}"
            );
        }
    }
    render(Forgot {
        sent: true,
        smtp: mail_ok,
    })
}

#[derive(Template)]
#[template(path = "message.html")]
struct Message {
    title: &'static str,
    body: &'static str,
}
fn expired() -> R {
    render(Message {
        title: "Link expired",
        body: "This link is invalid or has expired. Ask for a new one.",
    })
}

#[derive(Template)]
#[template(path = "reset.html")]
struct Reset {
    token: String,
    error: Option<String>,
}

async fn reset_valid(st: &AppState, token: &str) -> Result<bool, E> {
    let th = auth::hash_token(token);
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT EXISTS (SELECT 1 FROM password_resets WHERE token_hash = ?1 AND expires_at > ?2)",
                params![th, now()],
                |r| r.get(0),
            )
        })
        .await?)
}

async fn reset_page(State(st): State<AppState>, Path(token): Path<String>) -> R {
    if !reset_valid(&st, &token).await? {
        return expired();
    }
    render(Reset { token, error: None })
}

#[derive(Deserialize)]
struct PasswordForm {
    password: String,
}

async fn reset(
    State(st): State<AppState>,
    Path(token): Path<String>,
    Form(f): Form<PasswordForm>,
) -> R {
    if f.password.len() < 10 {
        return render(Reset {
            token,
            error: Some("Password must be at least 10 characters.".into()),
        });
    }
    let pw = auth::hash_password(f.password).await;
    let th = auth::hash_token(&token);
    let uid: Option<i64> = st
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            let uid: Option<i64> = tx
                .query_row(
                    "DELETE FROM password_resets WHERE token_hash = ?1 AND expires_at > ?2 RETURNING user_id",
                    params![th, now()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(uid) = uid {
                tx.execute("UPDATE users SET pw_hash = ?1 WHERE id = ?2", params![pw, uid])?;
                tx.execute("DELETE FROM sessions WHERE user_id = ?1", [uid])?;
            }
            tx.commit()?;
            Ok(uid)
        })
        .await?;
    match uid {
        Some(uid) => with_cookie("/", auth::login(&st, uid).await?),
        None => expired(),
    }
}

#[derive(Template)]
#[template(path = "invite.html")]
struct InvitePage {
    token: String,
    email: String,
    error: Option<String>,
}

async fn invite_email(st: &AppState, token: &str) -> Result<Option<String>, E> {
    let th = auth::hash_token(token);
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT email FROM invites WHERE token_hash = ?1 AND accepted_at IS NULL AND expires_at > ?2",
                params![th, now()],
                |r| r.get(0),
            )
            .optional()
        })
        .await?)
}

async fn invite_page(State(st): State<AppState>, Path(token): Path<String>) -> R {
    match invite_email(&st, &token).await? {
        Some(email) => render(InvitePage {
            token,
            email,
            error: None,
        }),
        None => expired(),
    }
}

#[derive(Deserialize)]
struct JoinForm {
    name: String,
    password: String,
}

async fn accept_invite(
    State(st): State<AppState>,
    Path(token): Path<String>,
    Form(f): Form<JoinForm>,
) -> R {
    let Some(email) = invite_email(&st, &token).await? else {
        return expired();
    };
    if f.password.len() < 10 {
        let error = Some("Password must be at least 10 characters.".into());
        return render(InvitePage {
            token,
            email,
            error,
        });
    }
    let pw = auth::hash_password(f.password).await;
    let th = auth::hash_token(&token);
    let uid: Option<i64> = st
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            let inv: Option<(String, String, String)> = tx
                .query_row(
                    "UPDATE invites SET accepted_at = ?2 WHERE token_hash = ?1 AND accepted_at IS NULL AND expires_at > ?2
                     RETURNING email, role, project_ids_json",
                    params![th, now()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((email, role, pids)) = inv else { return Ok(None) };
            if tx.query_row("SELECT EXISTS (SELECT 1 FROM users WHERE email = ?1)", [&email], |r| r.get(0))? {
                return Ok(None);
            }
            tx.execute(
                "INSERT INTO users (email, name, pw_hash, role, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![email, f.name.trim(), pw, role, now()],
            )?;
            let uid = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO project_members (project_id, user_id, role)
                 SELECT value, ?1, 'member' FROM json_each(?2) WHERE value IN (SELECT id FROM projects)",
                params![uid, pids],
            )?;
            tx.commit()?;
            Ok(Some(uid))
        })
        .await?;
    match uid {
        Some(uid) => with_cookie("/", auth::login(&st, uid).await?),
        None => expired(),
    }
}

// ---------- app pages ----------

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
    me: CurrentUser,
    projects: Vec<Project>,
}

async fn home(me: CurrentUser, State(st): State<AppState>) -> R {
    let projects = visible_projects(&st, &me).await?;
    render(Home { me, projects })
}

#[derive(Template)]
#[template(path = "settings.html")]
struct Settings {
    me: CurrentUser,
    org_name: String,
    base_url: String,
    smtp: bool,
}

async fn settings(me: CurrentUser, State(st): State<AppState>, h: HeaderMap) -> R {
    admin(&me)?;
    let org_name = st
        .db
        .read(|c| {
            c.query_row("SELECT name FROM orgs WHERE id = 1", [], |r| r.get(0))
                .optional()
        })
        .await?
        .unwrap_or_default();
    render(Settings {
        me,
        org_name,
        base_url: base_url(&h),
        smtp: mail::configured(),
    })
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}

async fn save_org(me: CurrentUser, State(st): State<AppState>, Form(f): Form<NameForm>) -> R {
    admin(&me)?;
    st.db
        .write(move |c| c.execute("UPDATE orgs SET name = ?1 WHERE id = 1", [f.name.trim()]))
        .await?;
    to("/settings")
}

pub struct Member {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub role: String,
}

#[derive(Template)]
#[template(path = "members.html")]
struct Members {
    me: CurrentUser,
    users: Vec<Member>,
    projects: Vec<Project>,
    invite_link: Option<String>,
    smtp: bool,
}

async fn members_page(st: &AppState, me: CurrentUser, invite_link: Option<String>) -> R {
    admin(&me)?;
    let users = st
        .db
        .read(|c| {
            c.prepare("SELECT id, email, name, role FROM users ORDER BY id")?
                .query_map([], |r| {
                    Ok(Member {
                        id: r.get(0)?,
                        email: r.get(1)?,
                        name: r.get(2)?,
                        role: r.get(3)?,
                    })
                })?
                .collect()
        })
        .await?;
    let projects = visible_projects(st, &me).await?;
    render(Members {
        me,
        users,
        projects,
        invite_link,
        smtp: mail::configured(),
    })
}

async fn members(me: CurrentUser, State(st): State<AppState>) -> R {
    members_page(&st, me, None).await
}

/// Form fields: email, role, p (repeated project id).
async fn invite(
    me: CurrentUser,
    State(st): State<AppState>,
    h: HeaderMap,
    Form(f): Form<Vec<(String, String)>>,
) -> R {
    admin(&me)?;
    let field = |k: &str| {
        f.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.trim().to_string())
    };
    let email = field("email").unwrap_or_default().to_lowercase();
    if !valid_email(&email) {
        return Err(E(StatusCode::UNPROCESSABLE_ENTITY, "invalid email".into()));
    }
    let role = if field("role").as_deref() == Some("admin") {
        "admin"
    } else {
        "member"
    };
    let pids: Vec<i64> = f
        .iter()
        .filter(|(n, _)| n == "p")
        .filter_map(|(_, v)| v.parse().ok())
        .collect();
    let tok = auth::new_token();
    let th = auth::hash_token(&tok);
    let (e2, uid) = (email.clone(), me.id);
    st.db
        .write(move |c| {
            c.execute(
                "INSERT INTO invites (token_hash, email, role, project_ids_json, invited_by, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![th, e2, role, serde_json::to_string(&pids).unwrap(), uid, now() + 7 * 86400],
            )
        })
        .await?;
    let link = format!("{}/invite/{tok}", base_url(&h));
    if mail::configured() {
        let text = format!("You were invited to OpenFault. Accept within 7 days:\n\n{link}\n");
        let html = format!(
            "<p>You were invited to OpenFault. Accept within 7 days:</p><p><a href=\"{link}\">{link}</a></p>"
        );
        mail::send_mail(&st, &email, "You're invited to OpenFault", &text, &html).await;
    }
    members_page(&st, me, Some(link)).await
}

#[derive(Deserialize)]
struct RoleForm {
    role: String,
}

async fn set_role(
    me: CurrentUser,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<RoleForm>,
) -> R {
    admin(&me)?;
    let role = if f.role == "admin" { "admin" } else { "member" };
    st.db
        .write(move |c| {
            c.execute(
                "UPDATE users SET role = ?1 WHERE id = ?2 AND role != 'owner'",
                params![role, id],
            )
        })
        .await?;
    to("/settings/members")
}

async fn remove_member(me: CurrentUser, State(st): State<AppState>, Path(id): Path<i64>) -> R {
    admin(&me)?;
    st.db
        .write(move |c| c.execute("DELETE FROM users WHERE id = ?1 AND role != 'owner'", [id]))
        .await?;
    to("/settings/members")
}

#[derive(Template)]
#[template(path = "projects.html")]
struct Projects {
    me: CurrentUser,
    projects: Vec<Project>,
    error: Option<String>,
}

async fn projects(me: CurrentUser, State(st): State<AppState>) -> R {
    let projects = visible_projects(&st, &me).await?;
    render(Projects {
        me,
        projects,
        error: None,
    })
}

#[derive(Deserialize)]
struct ProjectForm {
    name: String,
    platform: String,
}

async fn create_project(
    me: CurrentUser,
    State(st): State<AppState>,
    Form(f): Form<ProjectForm>,
) -> R {
    admin(&me)?;
    let slug = slugify(&f.name);
    let s2 = slug.clone();
    let created = st
        .db
        .write(move |c| {
            if c.query_row(
                "SELECT EXISTS (SELECT 1 FROM projects WHERE slug = ?1)",
                [&s2],
                |r| r.get(0),
            )? {
                return Ok(false);
            }
            insert_project(c, f.name.trim(), &s2, platform(&f.platform)).map(|_| true)
        })
        .await?;
    if !created {
        let projects = visible_projects(&st, &me).await?;
        let error = Some(format!("A project with slug \"{slug}\" already exists."));
        return render(Projects {
            me,
            projects,
            error,
        });
    }
    to(&format!("/settings/projects/{slug}"))
}

pub struct Key {
    pub id: i64,
    pub label: String,
    pub active: bool,
    pub dsn: String,
}

#[derive(Template)]
#[template(path = "project.html")]
struct ProjectPage {
    me: CurrentUser,
    project: Project,
    keys: Vec<Key>,
    dsn: String,
}

async fn project(
    me: CurrentUser,
    State(st): State<AppState>,
    h: HeaderMap,
    Path(slug): Path<String>,
) -> R {
    let project = project_for(&st, &me, slug).await?;
    let (base, pid) = (base_url(&h), project.id);
    let keys: Vec<Key> = st
        .db
        .read(move |c| {
            c.prepare("SELECT id, label, active, public_key FROM dsn_keys WHERE project_id = ?1 ORDER BY active DESC, id")?
                .query_map([pid], |r| {
                    Ok(Key {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        active: r.get(2)?,
                        dsn: dsn(&base, &r.get::<_, String>(3)?, pid),
                    })
                })?
                .collect()
        })
        .await?;
    let dsn = keys
        .iter()
        .find(|k| k.active)
        .map(|k| k.dsn.clone())
        .unwrap_or_default();
    render(ProjectPage {
        me,
        project,
        keys,
        dsn,
    })
}

#[derive(Deserialize)]
struct ProjectSettingsForm {
    name: String,
    platform: String,
    retention_days: i64,
    store_ip: Option<String>,
}

async fn save_project(
    me: CurrentUser,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Form(f): Form<ProjectSettingsForm>,
) -> R {
    admin(&me)?;
    let p = project_for(&st, &me, slug.clone()).await?;
    st.db
        .write(move |c| {
            c.execute(
                "UPDATE projects SET name = ?1, platform = ?2, retention_days = ?3,
                 settings_json = json_set(settings_json, '$.store_ip', json(?4)) WHERE id = ?5",
                params![
                    f.name.trim(),
                    platform(&f.platform),
                    f.retention_days.clamp(1, 3650),
                    if f.store_ip.is_some() {
                        "true"
                    } else {
                        "false"
                    },
                    p.id
                ],
            )
        })
        .await?;
    to(&format!("/settings/projects/{slug}"))
}

#[derive(Deserialize)]
struct LabelForm {
    label: String,
}

async fn create_key(
    me: CurrentUser,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Form(f): Form<LabelForm>,
) -> R {
    admin(&me)?;
    let p = project_for(&st, &me, slug.clone()).await?;
    st.db
        .write(move |c| {
            c.execute(
                "INSERT INTO dsn_keys (project_id, public_key, label, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![p.id, hex(&auth::random::<16>()), f.label.trim(), now()],
            )
        })
        .await?;
    to(&format!("/settings/projects/{slug}"))
}

async fn revoke_key(
    me: CurrentUser,
    State(st): State<AppState>,
    Path((slug, id)): Path<(String, i64)>,
) -> R {
    admin(&me)?;
    let p = project_for(&st, &me, slug.clone()).await?;
    st.db
        .write(move |c| {
            c.execute(
                "UPDATE dsn_keys SET active = 0 WHERE id = ?1 AND project_id = ?2",
                [id, p.id],
            )
        })
        .await?;
    to(&format!("/settings/projects/{slug}"))
}

pub struct TokenRow {
    pub id: i64,
    pub label: String,
    pub scopes: String,
    pub project: String,
    pub last_used: String,
}

#[derive(Template)]
#[template(path = "tokens.html")]
struct Tokens {
    me: CurrentUser,
    tokens: Vec<TokenRow>,
    projects: Vec<Project>,
    new_token: Option<String>,
}

async fn tokens_page(st: &AppState, me: CurrentUser, new_token: Option<String>) -> R {
    let uid = me.id;
    let tokens = st
        .db
        .read(move |c| {
            c.prepare(
                "SELECT t.id, coalesce(t.label, ''), t.scopes, coalesce(p.name, 'All my projects'), t.last_used
                 FROM api_tokens t LEFT JOIN projects p ON p.id = t.project_id WHERE t.user_id = ?1 ORDER BY t.id DESC",
            )?
            .query_map([uid], |r| {
                Ok(TokenRow {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    scopes: r.get(2)?,
                    project: r.get(3)?,
                    last_used: r
                        .get::<_, Option<i64>>(4)?
                        .and_then(|t| jiff::Timestamp::from_second(t).ok())
                        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| "never".into()),
                })
            })?
            .collect()
        })
        .await?;
    let projects = visible_projects(st, &me).await?;
    render(Tokens {
        me,
        tokens,
        projects,
        new_token,
    })
}

async fn tokens(me: CurrentUser, State(st): State<AppState>) -> R {
    tokens_page(&st, me, None).await
}

#[derive(Deserialize)]
struct TokenForm {
    label: String,
    scope: String,
    project: String,
}

async fn create_token(me: CurrentUser, State(st): State<AppState>, Form(f): Form<TokenForm>) -> R {
    let scope = if f.scope == "write" { "write" } else { "read" };
    let pid = match f.project.as_str() {
        "" => None,
        id => {
            let id: i64 = id.parse().map_err(|_| not_found())?;
            let visible = visible_projects(&st, &me).await?;
            Some(
                visible
                    .iter()
                    .find(|p| p.id == id)
                    .ok_or_else(not_found)?
                    .id,
            )
        }
    };
    let tok = auth::new_api_token();
    let (th, uid) = (auth::hash_token(&tok), me.id);
    st.db
        .write(move |c| {
            c.execute(
                "INSERT INTO api_tokens (token_hash, user_id, project_id, scopes, label) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![th, uid, pid, scope, f.label.trim()],
            )
        })
        .await?;
    tokens_page(&st, me, Some(tok)).await
}

async fn revoke_token(me: CurrentUser, State(st): State<AppState>, Path(id): Path<i64>) -> R {
    let uid = me.id;
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM api_tokens WHERE id = ?1 AND user_id = ?2",
                [id, uid],
            )
        })
        .await?;
    to("/settings/tokens")
}

#[derive(Template)]
#[template(path = "account.html")]
struct Account {
    me: CurrentUser,
    msg: Option<String>,
}

async fn account(me: CurrentUser) -> R {
    render(Account { me, msg: None })
}

async fn save_account(me: CurrentUser, State(st): State<AppState>, Form(f): Form<NameForm>) -> R {
    let uid = me.id;
    st.db
        .write(move |c| {
            c.execute(
                "UPDATE users SET name = ?1 WHERE id = ?2",
                params![f.name.trim(), uid],
            )
        })
        .await?;
    to("/settings/account")
}

#[derive(Deserialize)]
struct ChangePasswordForm {
    current: String,
    new: String,
}

async fn change_password(
    me: CurrentUser,
    State(st): State<AppState>,
    h: HeaderMap,
    Form(f): Form<ChangePasswordForm>,
) -> R {
    let current = auth::cookie(&h, auth::COOKIE).map(auth::hash_token);
    let uid = me.id;
    let hash: String = st
        .db
        .read(move |c| {
            c.query_row("SELECT pw_hash FROM users WHERE id = ?1", [uid], |r| {
                r.get(0)
            })
        })
        .await?;
    let msg = if !auth::verify_password(f.current, hash).await {
        "Current password is wrong."
    } else if f.new.len() < 10 {
        "New password must be at least 10 characters."
    } else {
        let pw = auth::hash_password(f.new).await;
        st.db
            .write(move |c| {
                let tx = c.transaction()?;
                tx.execute(
                    "UPDATE users SET pw_hash = ?1 WHERE id = ?2",
                    params![pw, uid],
                )?;
                // sign out every other session (a stolen cookie dies with the old password).
                tx.execute(
                    "DELETE FROM sessions WHERE user_id = ?1 AND id_hash IS NOT ?2",
                    params![uid, current],
                )?;
                tx.commit()
            })
            .await?;
        "Password changed."
    };
    render(Account {
        me,
        msg: Some(msg.into()),
    })
}
