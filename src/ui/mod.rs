//! Server-rendered UI (PLAN §8, §9): plumbing, setup/auth pages, settings.
mod backup;
pub mod chart;
mod dashboard;
mod issues;
pub use issues::issue_markdown;
mod releases;
mod settings;
mod teams;
mod tv;

use crate::{
    AppState,
    auth::{self, CurrentUser},
    db::now,
    mail,
};
use askama::Template;
use axum::{
    Extension, Form, Router,
    extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, Request, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use rusqlite::{OptionalExtension, params};
use rust_embed::RustEmbed;
use serde::Deserialize;
use tower_http::compression::CompressionLayer;

pub fn routes() -> Router<AppState> {
    Router::new()
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
        .route("/setup/email", get(setup_email))
        .route("/settings/server", post(save_server))
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
        .route(
            "/settings/members/invites/{hash}/revoke",
            post(revoke_invite),
        )
        .route("/static/{*path}", get(asset))
        .merge(dashboard::routes())
        .merge(issues::routes())
        .merge(releases::routes())
        .merge(settings::routes())
        .merge(backup::routes())
        .merge(teams::routes())
        .merge(tv::routes())
        .fallback(async || E(StatusCode::NOT_FOUND, String::new()))
        .layer(DefaultBodyLimit::max(64 * 1024)) // overrides the ingest-wide disable
        .layer(middleware::from_fn(auth::csrf))
        .layer(middleware::from_fn(clear_flash))
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
        let (title, body) = match self.0 {
            StatusCode::FORBIDDEN => (
                "No access",
                "You don't have permission to view this page. Ask an admin if you need it.",
            ),
            StatusCode::NOT_FOUND => ("Not found", "This page doesn't exist, or you can't see it."),
            s if s.is_server_error() => (
                "Something broke",
                "The server hit an error. It has been logged.",
            ),
            _ => ("Request rejected", self.1.as_str()),
        };
        let page = ErrorPage {
            code: self.0.as_u16(),
            title,
            body,
        };
        (self.0, Html(page.render().unwrap_or_default())).into_response()
    }
}
#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage<'a> {
    code: u16,
    title: &'a str,
    body: &'a str,
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
/// static asset cache-buster; bump when static/ changes.
pub const V: &str = "0.1.0-m3.14";

/// A Phosphor icon from the sprite (`static/icons.svg`); render with `|safe`.
pub fn icon(name: &str) -> String {
    format!(
        "<svg class=\"i\" aria-hidden=\"true\"><use href=\"/static/icons.svg?v={V}#i-{name}\"/></svg>"
    )
}
const FLASH: &str = "of_flash";

/// Redirect + a one-shot toast message (cookie read by `Me`, cleared by `clear_flash`).
pub fn flash(path: &str, msg: &str) -> R {
    let c = format!(
        "{FLASH}={}; Path=/; Max-Age=60; SameSite=Lax",
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, msg)
    );
    with_cookie(path, c)
}

async fn clear_flash(req: Request, next: Next) -> Response {
    let had = auth::cookie(req.headers(), FLASH).is_some_and(|v| !v.is_empty());
    let mut res = next.run(req).await;
    if had && !res.status().is_redirection() {
        res.headers_mut().append(
            header::SET_COOKIE,
            header::HeaderValue::from_static("of_flash=; Path=/; Max-Age=0"),
        );
    }
    res
}

pub struct SavedView {
    pub id: i64,
    pub name: String,
    pub href: String,
}

/// The signed-in user plus what every app page's chrome needs (switcher, sidebar, toast).
pub struct Me {
    pub user: CurrentUser,
    pub projects: Vec<Project>,
    pub views: Vec<SavedView>,
    /// unresolved issues per visible project (sidebar pills)
    pub counts: std::collections::HashMap<i64, i64>,
    /// (slug, name) of teams the user belongs to
    pub teams: Vec<(String, String)>,
    pub org: String,
    pub flash: Option<String>,
    pub path: String,
    /// path + query, what live fragments re-fetch
    pub url: String,
    /// project the page is scoped to (/projects/{slug}/...)
    pub scope: Option<Project>,
}

impl std::ops::Deref for Me {
    type Target = CurrentUser;
    fn deref(&self) -> &CurrentUser {
        &self.user
    }
}

impl Me {
    pub fn at(&self, seg: &str) -> bool {
        self.path.split('/').any(|s| s == seg)
    }
    pub fn initials(&self) -> String {
        initials(&self.user.name, &self.user.email)
    }
    pub fn base(&self, page: &str) -> String {
        match &self.scope {
            Some(p) => format!("/projects/{}/{page}", p.slug),
            None => format!("/{page}"),
        }
    }
    pub fn count(&self, id: &i64) -> i64 {
        self.counts.get(id).copied().unwrap_or(0)
    }
    /// sidebar row is the page's project
    pub fn on_project(&self, id: &i64) -> bool {
        self.scope.as_ref().is_some_and(|p| p.id == *id)
    }
    /// breadcrumb leaf for the current page
    pub fn section(&self) -> &'static str {
        let segs = ["settings", "releases", "issues", "tv"];
        let names = ["Settings", "Releases", "Issues", "TV"];
        match segs.iter().position(|s| self.at(s)) {
            Some(i) => names[i],
            None if self.path == "/" => "Dashboard",
            None => "",
        }
    }
    pub fn project_name(&self, id: &i64) -> &str {
        self.projects
            .iter()
            .find(|p| p.id == *id)
            .map_or("", |p| p.name.as_str())
    }
}

pub fn initials(name: &str, email: &str) -> String {
    let src = if name.trim().is_empty() { email } else { name };
    src.split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

impl FromRequestParts<AppState> for Me {
    type Rejection = axum::response::Redirect;
    async fn from_request_parts(p: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let user = CurrentUser::from_request_parts(p, st).await?;
        let projects = visible_projects(st, &user).await.unwrap_or_default();
        let uid = user.id;
        let ids: Vec<i64> = projects.iter().map(|p| p.id).collect();
        let (views, counts, teams, org) = st
            .db
            .read(move |c| {
                let views = c.prepare("SELECT id, name, query, sort FROM saved_views WHERE user_id = ?1 ORDER BY name")?
                    .query_map([uid], |r| {
                        let (q, s): (String, String) = (r.get(2)?, r.get(3)?);
                        Ok(SavedView {
                            id: r.get(0)?,
                            name: r.get(1)?,
                            href: format!("/issues?q={}&sort={}", enc(&q), enc(&s)),
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let teams = c
                    .prepare("SELECT t.slug, t.name FROM team_members m JOIN teams t ON t.id = m.team_id WHERE m.user_id = ?1 ORDER BY t.name")?
                    .query_map([uid], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let org = c
                    .query_row("SELECT name FROM orgs WHERE id = 1", [], |r| r.get(0))
                    .optional()?
                    .unwrap_or_default();
                Ok((views, crate::issues::unresolved_counts(c, &ids)?, teams, org))
            })
            .await
            .unwrap_or_default();
        let flash = auth::cookie(&p.headers, FLASH)
            .and_then(|v| {
                base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, v).ok()
            })
            .and_then(|b| String::from_utf8(b).ok());
        Ok(Me {
            user,
            projects,
            views,
            counts,
            teams,
            org,
            flash,
            path: p.uri.path().to_string(),
            url: p
                .uri
                .path_and_query()
                .map_or("/", |x| x.as_str())
                .to_string(),
            scope: None,
        })
    }
}

/// Percent-encode for a query value.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// "3m ago" style.
pub fn ago(t: &i64) -> String {
    let d = (now() - *t).max(0);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        86400..2_592_000 => format!("{}d ago", d / 86400),
        _ => jiff::Timestamp::from_second(*t)
            .map(|t| t.strftime("%b %d, %Y").to_string())
            .unwrap_or_default(),
    }
}

/// Relative time with the absolute one on hover; render with `|safe`.
pub fn t(t: &i64) -> String {
    let iso = jiff::Timestamp::from_second(*t)
        .map(|t| t.to_string())
        .unwrap_or_default();
    format!(
        "<time datetime=\"{iso}\" title=\"{}\">{}</time>",
        abs(t),
        ago(t)
    )
}

/// "Oct 20, 2026"
pub fn abs_date(t: &i64) -> String {
    jiff::Timestamp::from_second(*t)
        .map(|t| t.strftime("%b %d, %Y").to_string())
        .unwrap_or_default()
}

pub fn abs(t: &i64) -> String {
    jiff::Timestamp::from_second(*t)
        .map(|t| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_default()
}

pub fn n(v: &i64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) && c != '-' {
            out.push(',');
        }
        out.push(c);
    }
    out
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

/// Configured base URL (settings or OPENFAULT_BASE_URL), else http://{Host}.
pub fn base_url(st: &AppState, h: &HeaderMap) -> String {
    crate::config::base_or(&st.db, h.get(header::HOST).and_then(|v| v.to_str().ok()))
}

/// `{scheme}://{public_key}@{host}/{project_id}`
pub fn dsn(base: &str, key: &str, pid: i64) -> String {
    let (scheme, host) = base.split_once("://").unwrap_or(("http", base));
    format!("{scheme}://{key}@{host}/{pid}")
}

pub(crate) fn slugify(s: &str) -> String {
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
pub fn platform_icon(p: &str) -> &'static str {
    match p {
        "ruby" => "diamond",
        "javascript" => "file-js",
        "node" => "hexagon",
        "python" => "file-py",
        _ => "cube",
    }
}
pub(crate) fn platform(p: &str) -> &str {
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
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    // templates reference assets as ?v={crate version}; fonts never change.
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
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
    pub inapp_rules: String,
}

const PROJECT_COLS: &str = "id, slug, name, coalesce(platform, ''), retention_days,
    coalesce(json_extract(settings_json, '$.store_ip'), 0), coalesce(json_extract(settings_json, '$.inapp_rules'), '')";
fn project_row(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        slug: r.get(1)?,
        name: r.get(2)?,
        platform: r.get(3)?,
        retention_days: r.get(4)?,
        store_ip: r.get::<_, i64>(5)? != 0,
        inapp_rules: r.get(6)?,
    })
}
pub(crate) const VISIBLE: &str =
    "(?1 OR id IN (SELECT project_id FROM project_access WHERE user_id = ?2))";

/// Owner/admin: every project. Member: direct project_members ∪ projects of their teams (`project_access`).
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

/// `project_for`, and `u` must administer it (org admin, direct project admin, or admin of a team that has it).
pub async fn admin_project_for(st: &AppState, u: &CurrentUser, slug: String) -> Result<Project, E> {
    let p = project_for(st, u, slug).await?;
    if crate::teams::project_admin(&st.db, u, p.id).await? {
        Ok(p)
    } else {
        Err(forbidden())
    }
}

pub(crate) fn insert_project(
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

pub(crate) fn valid_email(e: &str) -> bool {
    e.len() <= 254
        && e.split_once('@')
            .is_some_and(|(a, b)| !a.is_empty() && b.contains('.'))
}

// ---------- setup + auth pages ----------

#[derive(Template)]
#[template(path = "setup.html")]
struct Setup {
    error: Option<String>,
    /// operator hint: the one-time setup token for REST/MCP bootstrap
    token: Option<String>,
    token_env: bool,
}

async fn users_exist(st: &AppState) -> Result<bool, E> {
    Ok(st
        .db
        .read(|c| c.query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |r| r.get(0)))
        .await?)
}

async fn setup_view(st: &AppState, error: Option<String>) -> R {
    let token = crate::setup::ensure(st).await?;
    render(Setup {
        error,
        token,
        token_env: std::env::var("OPENFAULT_SETUP_TOKEN").is_ok_and(|t| !t.is_empty()),
    })
}

async fn setup_page(State(st): State<AppState>) -> R {
    if users_exist(&st).await? {
        return to("/login");
    }
    setup_view(&st, None).await
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

/// Shared validation for the wizard and `POST /api/v1/setup`.
pub(crate) fn setup_error(org: &str, email: &str, pw: &str, project: &str) -> Option<&'static str> {
    if pw.len() < 10 {
        Some("Password must be at least 10 characters.")
    } else if !valid_email(email) {
        Some("Enter a valid email.")
    } else if org.trim().is_empty() || project.trim().is_empty() {
        Some("Organization and project names are required.")
    } else {
        None
    }
}

async fn setup(State(st): State<AppState>, Form(f): Form<SetupForm>) -> R {
    let email = f.email.trim().to_lowercase();
    if let Some(e) = setup_error(&f.org_name, &email, &f.password, &f.project_name) {
        return setup_view(&st, Some(e.into())).await;
    }
    let pw = auth::hash_password(f.password).await;
    let done = st
        .db
        .write(move |c| {
            let org = f.org_name.trim();
            crate::setup::create_instance(
                c,
                org,
                &slugify(org),
                f.name.trim(),
                &email,
                &pw,
                f.project_name.trim(),
                &f.platform,
                |_, _, _| Ok(()),
            )
        })
        .await?;
    let Some((uid, _, slug)) = done else {
        return to("/login");
    };
    crate::setup::consumed(&st).await;
    with_cookie(
        &format!("/setup/email?project={slug}"),
        auth::login(&st, uid).await?,
    )
}

// ---------- server settings: wizard step 2 + Settings → Organization ----------

/// What the server form shows. Secrets are never rendered, only whether they are set.
pub struct ServerView {
    pub base_url: String,
    pub base_env: bool,
    pub smtp: crate::config::Smtp,
    pub smtp_pw_set: bool,
    pub smtp_env: bool,
    pub smtp_on: bool,
    pub smtp_error: Option<String>,
    pub relay_url: String,
    pub relay_token_set: bool,
    pub relay_env: bool,
    pub relay_on: bool,
    pub allow_private: bool,
    pub allow_env: bool,
    /// (ok, message) of the last "Send test email"
    pub test: Option<(bool, String)>,
    /// test email recipient (the signed-in user)
    pub to: String,
    /// wizard: project slug to continue to; empty on the Settings page
    pub project: String,
    /// recent SMTP attempts: (time, recipient domain, subject, error)
    pub mails: Vec<(i64, String, String, Option<String>)>,
}

pub const SES_REGIONS: [&str; 12] = [
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "ca-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-central-1",
    "ap-south-1",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-northeast-1",
];

async fn server_view(
    st: &AppState,
    h: &HeaderMap,
    to: &str,
    project: String,
    test: Option<(bool, String)>,
) -> Result<ServerView, E> {
    let cfg = crate::config::get(&st.db);
    let mut smtp = cfg.stored.smtp.clone().unwrap_or_default();
    let smtp_pw_set = !smtp.password.is_empty();
    smtp.password.clear();
    if smtp.mode.is_empty() {
        smtp.mode = "starttls".into();
    }
    let relay = cfg.stored.push_relay.clone().unwrap_or_default();
    let mails = st
        .db
        .read(|c| {
            c.prepare("SELECT created_at, to_domain, subject, error FROM mail_log ORDER BY id DESC LIMIT 10")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect()
        })
        .await?;
    Ok(ServerView {
        base_url: base_url(st, h),
        base_env: cfg.base_env,
        smtp,
        smtp_pw_set,
        smtp_env: cfg.smtp_env,
        smtp_on: cfg.smtp.is_some(),
        smtp_error: cfg.smtp_error.clone(),
        relay_url: if cfg.relay_env {
            cfg.relay
                .as_ref()
                .map(|r| r.url.clone())
                .unwrap_or_default()
        } else {
            relay.url
        },
        relay_token_set: !relay.token.is_empty(),
        relay_env: cfg.relay_env,
        relay_on: cfg.relay.is_some(),
        allow_private: cfg.allow_private,
        allow_env: cfg.allow_env,
        test,
        to: to.to_string(),
        project,
        mails,
    })
}

#[derive(Template)]
#[template(path = "setup_email.html")]
struct SetupEmail {
    v: ServerView,
}

#[derive(Deserialize)]
struct ProjectQ {
    #[serde(default)]
    project: String,
}

async fn setup_email(
    me: Me,
    State(st): State<AppState>,
    h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<ProjectQ>,
) -> R {
    admin(&me)?;
    let v = server_view(&st, &h, &me.email, slugify(&q.project), None).await?;
    render(SetupEmail { v })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServerForm {
    base_url: String,
    smtp_host: String,
    smtp_port: String,
    smtp_username: String,
    smtp_password: String,
    smtp_from: String,
    smtp_mode: String,
    relay_url: String,
    relay_token: String,
    /// present only on the Settings form (the wizard has no checkbox)
    has_allow: String,
    allow_private: String,
    /// save | test
    action: String,
    project: String,
}

/// Saves the form (env-set sections untouched; blank secret = keep), then optionally sends a test email.
async fn save_server(
    me: Me,
    State(st): State<AppState>,
    h: HeaderMap,
    Form(f): Form<ServerForm>,
) -> R {
    admin(&me)?;
    let cfg = crate::config::get(&st.db);
    let (base_env, smtp_env, relay_env) = (cfg.base_env, cfg.smtp_env, cfg.relay_env);
    let project = slugify(&f.project);
    let wizard = !f.project.trim().is_empty();
    let test = f.action == "test";
    crate::config::update(&st.db, move |s| {
        let t = |x: &str| x.trim().to_string();
        if !base_env {
            s.base_url = Some(t(&f.base_url)).filter(|u| !u.is_empty());
        }
        if !smtp_env {
            let old = s.smtp.take().map(|x| x.password).unwrap_or_default();
            s.smtp = (!f.smtp_host.trim().is_empty()).then(|| crate::config::Smtp {
                host: t(&f.smtp_host),
                port: f.smtp_port.trim().parse().unwrap_or(0),
                username: t(&f.smtp_username),
                password: if f.smtp_password.is_empty() {
                    old
                } else {
                    f.smtp_password
                },
                from: t(&f.smtp_from),
                mode: match f.smtp_mode.as_str() {
                    "tls" | "none" => f.smtp_mode,
                    _ => "starttls".into(),
                },
            });
        }
        if !relay_env {
            let old = s.push_relay.take().map(|x| x.token).unwrap_or_default();
            s.push_relay = (!f.relay_url.trim().is_empty()).then(|| crate::config::Relay {
                url: t(&f.relay_url),
                token: if f.relay_token.trim().is_empty() {
                    old
                } else {
                    t(&f.relay_token)
                },
            });
        }
        if !f.has_allow.is_empty() {
            s.allow_private_webhooks = !f.allow_private.is_empty();
        }
    })
    .await?;
    if !test {
        return if wizard {
            to(&format!("/setup/backup?project={project}"))
        } else {
            flash("/settings", "Server settings saved")
        };
    }
    let r = match mail::send_test(&st.db, &me.email).await {
        Ok(()) => (true, format!("Test email sent to {}.", me.email)),
        Err(e) => (false, format!("Test email failed: {e}")),
    };
    let to = me.email.clone();
    if wizard {
        let v = server_view(&st, &h, &to, project, Some(r)).await?;
        return render(SetupEmail { v });
    }
    settings_page(&st, me, &h, Some(r)).await
}

#[derive(Template)]
#[template(path = "setup_sdk.html")]
struct SetupSdk {
    me: Me,
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
    me: Me,
    State(st): State<AppState>,
    h: HeaderMap,
    Path(slug): Path<String>,
) -> R {
    let project = project_for(&st, &me, slug).await?;
    let dsn = dsn(
        &base_url(&st, &h),
        &first_key(&st, project.id).await?,
        project.id,
    );
    render(SetupSdk { me, project, dsn })
}

#[derive(Template)]
#[template(path = "login.html")]
struct Login {
    error: Option<String>,
    email: String,
}

async fn login_page(State(st): State<AppState>) -> R {
    if !users_exist(&st).await? {
        return to("/setup");
    }
    render(Login {
        error: None,
        email: String::new(),
    })
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
            email: f.email,
        };
        return Ok((StatusCode::TOO_MANY_REQUESTS, render(page)?).into_response());
    }
    let email = f.email.trim().to_lowercase();
    let typed = f.email.clone();
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
        email: typed,
    })
}

async fn logout(State(st): State<AppState>, h: HeaderMap) -> R {
    if let Some(hash) = auth::cookie(&h, auth::COOKIE).map(auth::hash_token) {
        st.db
            .write(move |c| c.execute("DELETE FROM sessions WHERE id_hash = ?1", [hash]))
            .await?;
    }
    with_cookie("/login", auth::clear_cookie(&st.db))
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
    // without a configured base URL the link comes from the request's Host header, which an
    // anonymous caller controls (reset-link poisoning), so never email it.
    let mail_ok = mail::configured(&st.db) && crate::config::get(&st.db).base_url.is_some();
    if found {
        let link = format!("{}/reset/{tok}", base_url(&st, &h));
        if mail_ok {
            let text = format!("Reset your OpenFault password (valid 1 hour):\n\n{link}\n");
            let html = format!(
                "<p>Reset your OpenFault password (valid 1 hour):</p><p><a href=\"{link}\">{link}</a></p>"
            );
            mail::send_mail(&st, &email, "Reset your OpenFault password", &text, &html).await;
        } else {
            // deviation: never show a reset link to an anonymous visitor; without SMTP the admin reads it from the log.
            tracing::warn!(
                "SMTP or base URL not configured; password reset link for {email}: {link}"
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
            let inv: Option<(String, String, String, String)> = tx
                .query_row(
                    "UPDATE invites SET accepted_at = ?2 WHERE token_hash = ?1 AND accepted_at IS NULL AND expires_at > ?2
                     RETURNING email, role, project_ids_json, team_ids_json",
                    params![th, now()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((email, role, pids, teams)) = inv else { return Ok(None) };
            if tx.query_row("SELECT EXISTS (SELECT 1 FROM users WHERE email = ?1)", [&email], |r| r.get(0))? {
                return Ok(None);
            }
            tx.execute(
                "INSERT INTO users (email, name, pw_hash, role, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![email, f.name.trim(), pw, role, now()],
            )?;
            let uid = tx.last_insert_rowid();
            crate::teams::apply_grants(&tx, uid, &pids, &teams)?;
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
#[template(path = "settings.html")]
struct Settings {
    me: Me,
    org_name: String,
    v: ServerView,
}

async fn settings_page(st: &AppState, me: Me, h: &HeaderMap, test: Option<(bool, String)>) -> R {
    admin(&me)?;
    let org_name = st
        .db
        .read(|c| {
            c.query_row("SELECT name FROM orgs WHERE id = 1", [], |r| r.get(0))
                .optional()
        })
        .await?
        .unwrap_or_default();
    let v = server_view(st, h, &me.email, String::new(), test).await?;
    render(Settings { me, org_name, v })
}

async fn settings(me: Me, State(st): State<AppState>, h: HeaderMap) -> R {
    settings_page(&st, me, &h, None).await
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}

async fn save_org(me: Me, State(st): State<AppState>, Form(f): Form<NameForm>) -> R {
    admin(&me)?;
    st.db
        .write(move |c| c.execute("UPDATE orgs SET name = ?1 WHERE id = 1", [f.name.trim()]))
        .await?;
    flash("/settings", "Organization saved")
}

pub struct Member {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub role: String,
    /// "Backend (admin), Mobile"
    pub teams: String,
}

pub struct PendingInvite {
    pub hash: String,
    pub email: String,
    pub role: String,
    pub expires_at: i64,
    /// teams + projects the invite grants
    pub access: String,
}

#[derive(Template)]
#[template(path = "members.html")]
struct Members {
    me: Me,
    users: Vec<Member>,
    projects: Vec<Project>,
    teams: Vec<crate::teams::Team>,
    invites: Vec<PendingInvite>,
    invite_link: Option<String>,
    invited: String,
    smtp: bool,
}

async fn members_page(st: &AppState, me: Me, invite: Option<(String, String)>) -> R {
    admin(&me)?;
    let invites = st
        .db
        .read(|c| {
            c.prepare(
                "SELECT token_hash, email, role, expires_at, trim(
                   coalesce((SELECT group_concat(t.name || iif(json_extract(j.value, '$.role') = 'admin', ' (admin)', ''), ', ')
                     FROM json_each(team_ids_json) j JOIN teams t ON t.id = json_extract(j.value, '$.id')), '') || ' · ' ||
                   coalesce((SELECT group_concat(name, ', ') FROM projects WHERE id IN (SELECT value FROM json_each(project_ids_json))), ''),
                 ' ·')
                 FROM invites WHERE accepted_at IS NULL AND expires_at > ?1 ORDER BY expires_at DESC",
            )?
                .query_map([now()], |r| {
                    Ok(PendingInvite {
                        hash: r.get(0)?,
                        email: r.get(1)?,
                        role: r.get(2)?,
                        expires_at: r.get(3)?,
                        access: r.get(4)?,
                    })
                })?
                .collect()
        })
        .await?;
    let (invited, invite_link) = match invite {
        Some((e, l)) => (e, Some(l)),
        None => (String::new(), None),
    };
    let users = st
        .db
        .read(|c| {
            c.prepare(
                "SELECT u.id, u.email, u.name, u.role, coalesce((SELECT group_concat(t.name || iif(m.role = 'admin', ' (admin)', ''), ', ')
                   FROM team_members m JOIN teams t ON t.id = m.team_id WHERE m.user_id = u.id), '')
                 FROM users u ORDER BY u.id",
            )?
                .query_map([], |r| {
                    Ok(Member {
                        id: r.get(0)?,
                        email: r.get(1)?,
                        name: r.get(2)?,
                        role: r.get(3)?,
                        teams: r.get(4)?,
                    })
                })?
                .collect()
        })
        .await?;
    let projects = visible_projects(st, &me).await?;
    let u = me.user.clone();
    let teams = st.db.read(move |c| crate::teams::list(c, &u)).await?;
    render(Members {
        me,
        users,
        projects,
        teams,
        invites,
        invite_link,
        invited,
        smtp: mail::configured(&st.db),
    })
}

async fn members(me: Me, State(st): State<AppState>) -> R {
    members_page(&st, me, None).await
}

/// Form fields: email, role, p (repeated project id), t (repeated team id) + `tr:{team id}` = member|admin.
async fn invite(
    me: Me,
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
    let teams = f
        .iter()
        .filter(|(n, _)| n == "t")
        .filter_map(|(_, v)| v.parse().ok())
        .map(|id: i64| crate::teams::Grant {
            id,
            role: crate::teams::role(&field(&format!("tr:{id}")).unwrap_or_default()).into(),
        })
        .collect();
    match create_invite(&st, me.id, &email, role, pids, teams, &base_url(&st, &h)).await? {
        Some(link) => members_page(&st, me, Some((email, link))).await,
        None => flash("/settings/members", &format!("Access updated for {email}")),
    }
}

/// Inserts an invite (7 days), emails it when SMTP is on, returns the link. Shared with MCP and REST.
/// Email already a member → adds the project/team grants directly, only raising roles (never lowering), returns None.
pub(crate) async fn create_invite(
    st: &AppState,
    by: i64,
    email: &str,
    role: &'static str,
    pids: Vec<i64>,
    teams: Vec<crate::teams::Grant>,
    base: &str,
) -> Result<Option<String>, E> {
    let tok = auth::new_token();
    let th = auth::hash_token(&tok);
    let e2 = email.to_string();
    let (pids, teams) = (
        serde_json::to_string(&pids).unwrap(),
        serde_json::to_string(&teams).unwrap(),
    );
    let existing = st
        .db
        .write(move |c| {
            let uid: Option<i64> = c
                .query_row("SELECT id FROM users WHERE email = ?1", [&e2], |r| r.get(0))
                .optional()?;
            if let Some(uid) = uid {
                crate::teams::apply_grants(c, uid, &pids, &teams)?;
                if role == "admin" {
                    c.execute("UPDATE users SET role = 'admin' WHERE id = ?1 AND role = 'member'", [uid])?;
                }
                return Ok(true);
            }
            c.execute(
                "INSERT INTO invites (token_hash, email, role, project_ids_json, team_ids_json, invited_by, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![th, e2, role, pids, teams, by, now() + 7 * 86400],
            )
            .map(|_| false)
        })
        .await?;
    if existing {
        return Ok(None);
    }
    let link = format!("{base}/invite/{tok}");
    if mail::configured(&st.db) {
        let text = format!("You were invited to OpenFault. Accept within 7 days:\n\n{link}\n");
        let html = format!(
            "<p>You were invited to OpenFault. Accept within 7 days:</p><p><a href=\"{link}\">{link}</a></p>"
        );
        mail::send_mail(st, email, "You're invited to OpenFault", &text, &html).await;
    }
    Ok(Some(link))
}

async fn revoke_invite(me: Me, State(st): State<AppState>, Path(hash): Path<String>) -> R {
    admin(&me)?;
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM invites WHERE token_hash = ?1 AND accepted_at IS NULL",
                [hash],
            )
        })
        .await?;
    flash("/settings/members", "Invite revoked")
}

#[derive(Deserialize)]
struct RoleForm {
    role: String,
}

async fn set_role(
    me: Me,
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
    flash("/settings/members", "Role updated")
}

async fn remove_member(me: Me, State(st): State<AppState>, Path(id): Path<i64>) -> R {
    admin(&me)?;
    st.db
        .write(move |c| c.execute("DELETE FROM users WHERE id = ?1 AND role != 'owner'", [id]))
        .await?;
    flash("/settings/members", "Member removed")
}

#[derive(Template)]
#[template(path = "projects.html")]
struct Projects {
    me: Me,
    projects: Vec<Project>,
    error: Option<String>,
}

async fn projects(me: Me, State(st): State<AppState>) -> R {
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

async fn create_project(me: Me, State(st): State<AppState>, Form(f): Form<ProjectForm>) -> R {
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
    flash(&format!("/settings/projects/{slug}"), "Project created")
}

pub struct Key {
    pub id: i64,
    pub label: String,
    pub active: bool,
    pub dsn: String,
    pub public_key_short: String,
    pub created_at: i64,
}

#[derive(Template)]
#[template(path = "project.html")]
struct ProjectPage {
    me: Me,
    project: Project,
    keys: Vec<Key>,
    dsn: String,
    quotas: Vec<(String, String)>,
    can_admin: bool,
    /// (slug, name) of teams that include this project
    teams: Vec<(String, String)>,
}

async fn project(me: Me, State(st): State<AppState>, h: HeaderMap, Path(slug): Path<String>) -> R {
    let project = project_for(&st, &me, slug).await?;
    let (base, pid) = (base_url(&st, &h), project.id);
    let can_admin = crate::teams::project_admin(&st.db, &me, pid).await?;
    let teams = st
        .db
        .read(move |c| {
            c.prepare("SELECT t.slug, t.name FROM team_projects tp JOIN teams t ON t.id = tp.team_id WHERE tp.project_id = ?1 ORDER BY t.name")?
                .query_map([pid], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        })
        .await?;
    let keys: Vec<Key> = st
        .db
        .read(move |c| {
            c.prepare("SELECT id, label, active, public_key, created_at FROM dsn_keys WHERE project_id = ?1 ORDER BY active DESC, id DESC")?
                .query_map([pid], |r| {
                    let key: String = r.get(3)?;
                    Ok(Key {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        active: r.get(2)?,
                        dsn: dsn(&base, &key, pid),
                        public_key_short: format!("{}…", &key[..key.len().min(12)]),
                        created_at: r.get(4)?,
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
    let quotas = crate::ingest::handler::QUOTAS
        .iter()
        .map(|&(secs, max)| {
            let w = if secs % 3600 == 0 {
                format!("{} h", secs / 3600)
            } else {
                format!("{} min", secs / 60)
            };
            (w, n(&(max as i64)))
        })
        .collect();
    render(ProjectPage {
        me,
        project,
        keys,
        dsn,
        quotas,
        can_admin,
        teams,
    })
}

#[derive(Deserialize)]
struct ProjectSettingsForm {
    name: String,
    platform: String,
    retention_days: i64,
    store_ip: Option<String>,
    #[serde(default)]
    inapp_rules: String,
}

async fn save_project(
    me: Me,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Form(f): Form<ProjectSettingsForm>,
) -> R {
    let p = admin_project_for(&st, &me, slug.clone()).await?;
    st.db
        .write(move |c| {
            c.execute(
                "UPDATE projects SET name = ?1, platform = ?2, retention_days = ?3,
                 settings_json = json_set(settings_json, '$.store_ip', json(?4), '$.inapp_rules', ?6) WHERE id = ?5",
                params![
                    f.name.trim(),
                    platform(&f.platform),
                    f.retention_days.clamp(1, 3650),
                    if f.store_ip.is_some() {
                        "true"
                    } else {
                        "false"
                    },
                    p.id,
                    f.inapp_rules.trim()
                ],
            )
        })
        .await?;
    flash(&format!("/settings/projects/{slug}"), "Project saved")
}

#[derive(Deserialize)]
struct LabelForm {
    label: String,
}

async fn create_key(
    me: Me,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Form(f): Form<LabelForm>,
) -> R {
    let p = admin_project_for(&st, &me, slug.clone()).await?;
    st.db
        .write(move |c| {
            c.execute(
                "INSERT INTO dsn_keys (project_id, public_key, label, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![p.id, hex(&auth::random::<16>()), f.label.trim(), now()],
            )
        })
        .await?;
    flash(&format!("/settings/projects/{slug}"), "Key created")
}

async fn revoke_key(
    me: Me,
    State(st): State<AppState>,
    Path((slug, id)): Path<(String, i64)>,
) -> R {
    let p = admin_project_for(&st, &me, slug.clone()).await?;
    st.db
        .write(move |c| {
            c.execute(
                "UPDATE dsn_keys SET active = 0 WHERE id = ?1 AND project_id = ?2",
                [id, p.id],
            )
        })
        .await?;
    flash(&format!("/settings/projects/{slug}"), "Key revoked")
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
    me: Me,
    tokens: Vec<TokenRow>,
    projects: Vec<Project>,
    new_token: Option<String>,
}

async fn tokens_page(st: &AppState, me: Me, new_token: Option<String>) -> R {
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

async fn tokens(me: Me, State(st): State<AppState>) -> R {
    tokens_page(&st, me, None).await
}

#[derive(Deserialize)]
struct TokenForm {
    label: String,
    scope: String,
    project: String,
}

async fn create_token(me: Me, State(st): State<AppState>, Form(f): Form<TokenForm>) -> R {
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

async fn revoke_token(me: Me, State(st): State<AppState>, Path(id): Path<i64>) -> R {
    let uid = me.id;
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM api_tokens WHERE id = ?1 AND user_id = ?2",
                [id, uid],
            )
        })
        .await?;
    flash("/settings/tokens", "Token revoked")
}

#[derive(Template)]
#[template(path = "account.html")]
struct Account {
    me: Me,
    msg: Option<String>,
}

async fn account(me: Me) -> R {
    render(Account { me, msg: None })
}

async fn save_account(me: Me, State(st): State<AppState>, Form(f): Form<NameForm>) -> R {
    let uid = me.id;
    st.db
        .write(move |c| {
            c.execute(
                "UPDATE users SET name = ?1 WHERE id = ?2",
                params![f.name.trim(), uid],
            )
        })
        .await?;
    flash("/settings/account", "Profile saved")
}

#[derive(Deserialize)]
struct ChangePasswordForm {
    current: String,
    new: String,
}

async fn change_password(
    me: Me,
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
