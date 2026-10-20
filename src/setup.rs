//! First-boot bootstrap (PLAN §0). While no user exists there is one setup token (`ofs_…`, stored
//! hashed, printed to the log and shown on /setup). It authorizes `POST /api/v1/setup` and the MCP
//! `setup_instance` tool, which create org + owner + project like the wizard. Creating the first
//! user (any path) deletes it. `OPENFAULT_SETUP_TOKEN` pre-sets it for automation.
use crate::{
    AppState,
    api::ApiErr,
    auth, config,
    db::now,
    ui::{self, dsn, platform, slugify},
};
use axum::{
    Json,
    extract::{FromRequest, Request, State},
    http::{StatusCode, header},
};
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
pub struct OrgIn {
    pub name: String,
    /// default: from name
    pub slug: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
pub struct OwnerIn {
    pub email: String,
    pub name: String,
    /// at least 10 characters
    pub password: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct ProjectIn {
    pub name: String,
    /// ruby | javascript | node | python | other
    pub platform: String,
}
/// `SetupReq`'s schema, parsed only after the setup-open check so a closed setup answers first.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct SetupArgs(pub Value);
impl JsonSchema for SetupArgs {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        SetupReq::schema_name()
    }
    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        SetupReq::json_schema(g)
    }
}
#[derive(Deserialize, JsonSchema)]
pub struct SetupReq {
    pub org: OrgIn,
    pub owner: OwnerIn,
    pub project: ProjectIn,
    pub smtp: Option<config::Smtp>,
    pub push_relay: Option<config::Relay>,
    /// public URL of this server, used in DSNs and email links, e.g. https://errors.example.com
    pub base_url: Option<String>,
}

fn users_exist(c: &Connection) -> rusqlite::Result<bool> {
    c.query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |r| r.get(0))
}

/// The raw setup token while no user exists (created once per process, or OPENFAULT_SETUP_TOKEN).
pub async fn ensure(st: &AppState) -> crate::db::Result<Option<String>> {
    let mut g = st.setup_token.lock().await;
    if g.is_some() {
        return Ok(g.clone());
    }
    let env = std::env::var("OPENFAULT_SETUP_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let tok = env.clone().unwrap_or_else(|| {
        let hex: String = auth::random::<32>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("ofs_{hex}")
    });
    let h = auth::hash_token(&tok);
    let fresh = st
        .db
        .write(move |c| {
            c.execute("DELETE FROM setup_token", [])?;
            if users_exist(c)? {
                return Ok(false);
            }
            c.execute(
                "INSERT INTO setup_token (token_hash, created_at) VALUES (?1, ?2)",
                params![h, now()],
            )?;
            Ok(true)
        })
        .await?;
    if !fresh {
        return Ok(None);
    }
    if env.is_some() {
        tracing::info!("no users yet; setup token taken from OPENFAULT_SETUP_TOKEN");
    } else {
        tracing::warn!(
            "no users yet. Open /setup, or bootstrap with this one-time setup token (POST /api/v1/setup or the MCP tool setup_instance): {tok}"
        );
    }
    *g = Some(tok.clone());
    Ok(g.clone())
}

/// After the first user exists: forget the in-memory token (the row is deleted in the same tx).
pub async fn consumed(st: &AppState) {
    *st.setup_token.lock().await = None;
}

/// `bearer` is a live setup token.
pub async fn check(st: &AppState, bearer: &str) -> bool {
    let h = auth::hash_token(bearer);
    st.db
        .read(move |c| {
            c.query_row(
                "SELECT EXISTS (SELECT 1 FROM setup_token WHERE token_hash = ?1) AND NOT EXISTS (SELECT 1 FROM users)",
                [h],
                |r| r.get(0),
            )
        })
        .await
        .unwrap_or(false)
}

/// Shared by the wizard and `bootstrap`: org 1 + owner + first project, deletes the setup token.
/// None if a user already exists.
#[allow(clippy::too_many_arguments)]
pub fn create_instance(
    c: &mut Connection,
    org: &str,
    org_slug: &str,
    name: &str,
    email: &str,
    pw_hash: &str,
    project: &str,
    plat: &str,
    after: impl FnOnce(&Connection, i64, i64) -> rusqlite::Result<()>,
) -> rusqlite::Result<Option<(i64, i64, String)>> {
    let tx = c.transaction()?;
    if users_exist(&tx)? {
        return Ok(None);
    }
    tx.execute(
        "INSERT INTO orgs (id, name, slug, created_at) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, slug = excluded.slug",
        params![org, org_slug, now()],
    )?;
    tx.execute(
        "INSERT INTO users (email, name, pw_hash, is_owner, role, created_at) VALUES (?1, ?2, ?3, 1, 'owner', ?4)",
        params![email, name, pw_hash, now()],
    )?;
    let uid = tx.last_insert_rowid();
    let slug = slugify(project);
    let pid = ui::insert_project(&tx, project, &slug, platform(plat))?;
    tx.execute("DELETE FROM setup_token", [])?;
    after(&tx, uid, pid)?;
    tx.commit()?;
    Ok(Some((uid, pid, slug)))
}

/// `POST /api/v1/setup` and MCP `setup_instance`. `token_hash` must still be the live setup token.
pub async fn bootstrap(
    st: &AppState,
    bearer: &str,
    r: SetupReq,
    host: Option<&str>,
) -> Result<Value, ApiErr> {
    let bad = |m: &str| ApiErr(StatusCode::UNPROCESSABLE_ENTITY, m.into());
    let email = r.owner.email.trim().to_lowercase();
    if let Some(e) = ui::setup_error(&r.org.name, &email, &r.owner.password, &r.project.name) {
        return Err(bad(e));
    }
    if let Some(s) = &r.smtp {
        crate::mail::build(s).map_err(|e| bad(&format!("smtp: {e}")))?;
    }
    let pw = auth::hash_password(r.owner.password.clone()).await;
    let raw = auth::new_api_token();
    let (th, h) = (auth::hash_token(&raw), auth::hash_token(bearer));
    let org_slug = slugify(r.org.slug.as_deref().unwrap_or(&r.org.name));
    let SetupReq {
        org,
        owner,
        project,
        smtp,
        push_relay,
        base_url,
    } = r;
    let done = st
        .db
        .write(move |c| {
            let live: bool = c.query_row(
                "SELECT EXISTS (SELECT 1 FROM setup_token WHERE token_hash = ?1)",
                [&h],
                |r| r.get(0),
            )?;
            if !live && !users_exist(c)? {
                return Ok(Err(StatusCode::UNAUTHORIZED));
            }
            let made = create_instance(
                c,
                org.name.trim(),
                &org_slug,
                owner.name.trim(),
                &email,
                &pw,
                project.name.trim(),
                &project.platform,
                |tx, uid, _| {
                    let mut s = config::load(tx);
                    s.base_url = base_url.filter(|u| !u.trim().is_empty());
                    s.smtp = smtp;
                    s.push_relay = push_relay;
                    config::store(tx, &s)?;
                    tx.execute(
                        "INSERT INTO api_tokens (token_hash, user_id, scopes, label) VALUES (?1, ?2, 'write', 'setup')",
                        params![th, uid],
                    )?;
                    Ok(())
                },
            )?;
            let Some((_, pid, slug)) = made else {
                return Ok(Err(StatusCode::GONE));
            };
            let key: String = c.query_row(
                "SELECT public_key FROM dsn_keys WHERE project_id = ?1",
                [pid],
                |r| r.get(0),
            )?;
            Ok(Ok((pid, slug, key, project.name, platform(&project.platform).to_string())))
        })
        .await
        .map_err(|e| ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (pid, slug, key, name, plat) = done.map_err(|s| {
        ApiErr(
            s,
            if s == StatusCode::GONE {
                "already set up".into()
            } else {
                "invalid setup token".into()
            },
        )
    })?;
    config::invalidate(&st.db);
    consumed(st).await;
    let base = config::base_or(&st.db, host);
    Ok(json!({
        "api_token": raw,
        "dsn": dsn(&base, &key, pid),
        "project": {"id": pid, "slug": slug, "name": name.trim(), "platform": plat},
        "login_url": format!("{base}/login"),
    }))
}

/// `POST /api/v1/setup` with `Authorization: Bearer <setup token>`. 410 once a user exists,
/// then 401 for a bad token, and only then is the body parsed (415/422).
pub async fn rest(State(st): State<AppState>, req: Request) -> Result<Json<Value>, ApiErr> {
    let h = req.headers().clone();
    let bearer = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let exists = st
        .db
        .read(users_exist)
        .await
        .map_err(|e| ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if exists {
        return Err(ApiErr(StatusCode::GONE, "already set up".into()));
    }
    if !check(&st, &bearer).await {
        return Err(ApiErr(
            StatusCode::UNAUTHORIZED,
            "invalid setup token".into(),
        ));
    }
    let Json(r) = Json::<SetupReq>::from_request(req, &st)
        .await
        .map_err(|e| ApiErr(e.status(), e.body_text()))?;
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok());
    bootstrap(&st, &bearer, r, host).await.map(Json)
}
