//! Passwords, sessions, API tokens, CSRF (PLAN §9).
use crate::{AppState, db::now};
use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier},
};
use axum::{
    extract::{FromRequestParts, Request},
    http::{HeaderMap, Method, StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{OptionalExtension, params};
use std::sync::LazyLock;
use tokio::sync::Semaphore;

pub const COOKIE: &str = "of_session";
const SESSION_TTL: i64 = 30 * 86400;
/// ponytail: argon2 default params (19 MiB each); 2 at a time caps hashing RAM at ~40 MB.
static HASHING: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("os rng");
    b
}

/// 32 random bytes, base64url. Used for sessions, invites, resets.
pub fn new_token() -> String {
    URL_SAFE_NO_PAD.encode(random::<32>())
}

pub fn hash_token(t: &str) -> String {
    blake3::hash(t.as_bytes()).to_hex().to_string()
}

pub async fn hash_password(pw: String) -> String {
    let _p = HASHING.acquire().await.unwrap();
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(pw.as_bytes())
            .expect("argon2")
            .to_string()
    })
    .await
    .unwrap()
}

pub async fn verify_password(pw: String, hash: String) -> bool {
    let _p = HASHING.acquire().await.unwrap();
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .verify_password(pw.as_bytes(), hash.as_str())
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

/// Verified against when the email is unknown, so both paths cost one argon2 run.
pub static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    Argon2::default()
        .hash_password(b"openfault-dummy")
        .expect("argon2")
        .to_string()
});

pub fn cookie<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('='))
}

fn secure() -> &'static str {
    if std::env::var("OPENFAULT_BASE_URL").is_ok_and(|u| u.starts_with("https")) {
        "; Secure"
    } else {
        ""
    }
}

/// Creates a session row and returns the Set-Cookie value.
pub async fn login(st: &AppState, user_id: i64) -> crate::db::Result<String> {
    let tok = new_token();
    let h = hash_token(&tok);
    st.db
        .write(move |c| {
            c.execute(
                "INSERT INTO sessions (id_hash, user_id, expires_at) VALUES (?1, ?2, ?3)",
                params![h, user_id, now() + SESSION_TTL],
            )
        })
        .await?;
    Ok(format!(
        "{COOKIE}={tok}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_TTL}{}",
        secure()
    ))
}

pub fn clear_cookie() -> String {
    format!(
        "{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{}",
        secure()
    )
}

#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: i64,
    pub email: String,
    pub name: String,
    pub role: String,
}

impl CurrentUser {
    pub fn is_admin(&self) -> bool {
        self.role != "member"
    }
}

/// Session cookie → user. No users yet → /setup; otherwise → /login.
impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = Redirect;
    async fn from_request_parts(p: &mut Parts, st: &AppState) -> Result<Self, Redirect> {
        let h = cookie(&p.headers, COOKIE).map(hash_token);
        let (user, users) = st
            .db
            .read(move |c| {
                let u = match h {
                    Some(h) => c
                        .query_row(
                            "SELECT u.id, u.email, u.name, u.role FROM sessions s JOIN users u ON u.id = s.user_id
                             WHERE s.id_hash = ?1 AND s.expires_at > ?2",
                            params![h, now()],
                            |r| {
                                Ok(CurrentUser {
                                    id: r.get(0)?,
                                    email: r.get(1)?,
                                    name: r.get(2)?,
                                    role: r.get(3)?,
                                })
                            },
                        )
                        .optional()?,
                    None => None,
                };
                let n: i64 = c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
                Ok((u, n))
            })
            .await
            .map_err(|_| Redirect::to("/login"))?;
        match user {
            Some(u) => Ok(u),
            None if users == 0 => Err(Redirect::to("/setup")),
            None => Err(Redirect::to("/login")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ApiToken {
    pub id: i64,
    pub user_id: i64,
    pub project_id: Option<i64>,
    pub scopes: String,
}

/// `bearer` is the raw `of_…` token. Updates last_used in the background.
pub async fn auth_token(state: &AppState, bearer: &str) -> Option<ApiToken> {
    let h = hash_token(bearer);
    let t = state
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, user_id, project_id, scopes FROM api_tokens WHERE token_hash = ?1",
                [h],
                |r| {
                    Ok(ApiToken {
                        id: r.get(0)?,
                        user_id: r.get(1)?,
                        project_id: r.get(2)?,
                        scopes: r.get(3)?,
                    })
                },
            )
            .optional()
        })
        .await
        .ok()??;
    let (db, id) = (state.db.clone(), t.id);
    tokio::spawn(async move {
        let _ = db
            .write(move |c| {
                c.execute(
                    "UPDATE api_tokens SET last_used = ?1 WHERE id = ?2",
                    params![now(), id],
                )
            })
            .await;
    });
    Some(t)
}

pub fn new_api_token() -> String {
    let hex: String = random::<32>().iter().map(|b| format!("{b:02x}")).collect();
    format!("of_{hex}")
}

/// CSRF: SameSite=Lax cookie + reject cross-site non-GET requests. Requests with neither
/// Sec-Fetch-Site nor Origin (curl, tests) pass; every browser sends one on POST.
pub async fn csrf(req: Request, next: Next) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        let h = req.headers();
        let get = |k| h.get(k).and_then(|v: &header::HeaderValue| v.to_str().ok());
        let ok = match (get("sec-fetch-site"), get("origin")) {
            (Some(s), _) => s == "same-origin" || s == "none",
            (None, Some(o)) => o.split_once("://").map(|x| x.1) == get("host"),
            (None, None) => true,
        };
        if !ok {
            return (StatusCode::FORBIDDEN, "cross-site request blocked").into_response();
        }
    }
    next.run(req).await
}
