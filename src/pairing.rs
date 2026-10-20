//! QR device pairing for the iOS app (PLAN §11): a signed-in user starts a one-time code,
//! the phone claims it and gets its own `of_…` write token.
use crate::{
    AppState,
    api::{ApiErr, scope_from},
    auth::{CurrentUser, hash_token, new_api_token, random},
    db::{Db, now},
    ui::visible_projects,
};
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, FromRequestParts, Path, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    middleware,
    routing::{delete, post},
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// No 0/O/1/I. 32 symbols, so `byte % 32` is unbiased; 8 chars = 40 bits.
const ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const TTL: i64 = 600;
const MAX_OPEN: i64 = 5;
const NOT_FOUND: &str = "pairing code not found";

fn ae(code: StatusCode, msg: &str) -> ApiErr {
    ApiErr(code, msg.into())
}
fn internal(e: impl std::fmt::Display) -> ApiErr {
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Query-value percent-encoding: everything but RFC 3986 unreserved.
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Accepts "abcd-efgh", "ABCD EFGH", etc.
fn code_hash(code: &str) -> String {
    let c: String = code
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    hash_token(&c)
}

#[derive(Serialize, Debug)]
pub struct PairingStart {
    pub code: String,
    pub expires_at: i64,
    pub qr_payload: String,
}

pub async fn start_pairing(db: &Db, user_id: i64, base_url: &str) -> Result<PairingStart, ApiErr> {
    let code: String = random::<8>()
        .iter()
        .map(|b| ALPHABET[(b % 32) as usize] as char)
        .collect();
    let (h, t) = (hash_token(&code), now());
    let ok = db
        .write(move |c| {
            c.execute("DELETE FROM pairings WHERE expires_at < ?1", [t])?;
            let open: i64 = c.query_row(
                "SELECT count(*) FROM pairings WHERE user_id = ?1 AND claimed_at IS NULL",
                [user_id],
                |r| r.get(0),
            )?;
            if open >= MAX_OPEN {
                return Ok(false);
            }
            c.execute(
                "INSERT INTO pairings (code_hash, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
                params![h, user_id, t, t + TTL],
            )?;
            Ok(true)
        })
        .await
        .map_err(internal)?;
    if !ok {
        return Err(ae(
            StatusCode::TOO_MANY_REQUESTS,
            "too many open pairing codes; wait for one to expire",
        ));
    }
    Ok(PairingStart {
        qr_payload: format!("openfault://pair?host={}&code={code}", pct(base_url)),
        code,
        expires_at: t + TTL,
    })
}

#[derive(Serialize, Debug)]
pub struct Claimed {
    pub token: String,
    pub base_url: Option<String>,
    pub device_id: i64,
    pub user: Value,
    pub projects: Vec<Value>,
}

/// Invalid, expired, used and cancelled codes all answer the same 404.
pub async fn claim(
    st: &AppState,
    code: &str,
    platform: &str,
    device_name: &str,
    push_token: Option<String>,
) -> Result<Claimed, ApiErr> {
    if platform.is_empty() || platform.len() > 32 || device_name.len() > 100 {
        return Err(ae(StatusCode::UNPROCESSABLE_ENTITY, "invalid device"));
    }
    if push_token
        .as_ref()
        .is_some_and(|p| p.is_empty() || p.len() > 512)
    {
        return Err(ae(StatusCode::UNPROCESSABLE_ENTITY, "invalid push_token"));
    }
    let token = new_api_token();
    let (h, th, t) = (code_hash(code), hash_token(&token), now());
    let (platform, label) = (platform.to_string(), format!("iOS: {device_name}"));
    let name = device_name.to_string();
    let r = st
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            let Some(uid) = tx
                .query_row(
                    "SELECT user_id FROM pairings WHERE code_hash = ?1 AND claimed_at IS NULL AND expires_at > ?2",
                    params![h, t],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let tid: i64 = tx.query_row(
                "INSERT INTO api_tokens (token_hash, user_id, scopes, label) VALUES (?1, ?2, 'write', ?3) RETURNING id",
                params![th, uid, label],
                |r| r.get(0),
            )?;
            tx.execute(
                "UPDATE pairings SET claimed_at = ?2, token_id = ?3 WHERE code_hash = ?1",
                params![h, t, tid],
            )?;
            // re-pairing the same phone: revoke the token its previous pairing minted (NULL token matches nothing).
            tx.execute(
                "DELETE FROM api_tokens WHERE id = (SELECT token_id FROM devices WHERE user_id = ?1 AND push_token = ?2)",
                params![uid, push_token],
            )?;
            let device_id: i64 = tx.query_row(
                    "INSERT INTO devices (user_id, platform, push_token, created_at, name, token_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (user_id, push_token) DO UPDATE SET platform = excluded.platform, name = excluded.name, token_id = excluded.token_id
                     RETURNING id",
                    params![uid, platform, push_token, t, name, tid],
                    |r| r.get::<_, i64>(0),
                )?;
            let u = tx.query_row(
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
            )?;
            tx.commit()?;
            Ok(Some((u, device_id)))
        })
        .await
        .map_err(internal)?;
    let (u, device_id) = r.ok_or_else(|| ae(StatusCode::NOT_FOUND, NOT_FOUND))?;
    let projects = visible_projects(st, &u)
        .await
        .map_err(|_| ae(StatusCode::INTERNAL_SERVER_ERROR, "project lookup failed"))?
        .into_iter()
        .map(|p| json!({"id": p.id, "slug": p.slug, "name": p.name}))
        .collect();
    Ok(Claimed {
        token,
        base_url: crate::config::get(&st.db).base_url.clone(),
        device_id,
        user: json!({"id": u.id, "name": u.name, "email": u.email}),
        projects,
    })
}

// ---------- routes ----------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/pair/start", post(start))
        .route("/pair/claim", post(claim_route))
        .route("/pair/{code}", delete(cancel))
        // session-cookie callers: same cross-site guard as the web UI.
        .route_layer(middleware::from_fn(crate::auth::csrf))
}

/// Session cookie (web UI) or a Bearer token with write scope → user id.
struct Caller(i64);

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiErr;
    async fn from_request_parts(p: &mut Parts, st: &AppState) -> Result<Self, ApiErr> {
        if p.headers.contains_key(header::AUTHORIZATION) {
            let s = scope_from(st, &p.headers).await?;
            s.writer()?;
            return Ok(Caller(s.user.id));
        }
        CurrentUser::from_request_parts(p, st)
            .await
            .map(|u| Caller(u.id))
            .map_err(|_| ae(StatusCode::UNAUTHORIZED, "sign in required"))
    }
}

async fn start(
    Caller(uid): Caller,
    State(st): State<AppState>,
    h: HeaderMap,
) -> Result<Json<PairingStart>, ApiErr> {
    start_pairing(&st.db, uid, &crate::ui::base_url(&st, &h))
        .await
        .map(Json)
}

async fn cancel(
    Caller(uid): Caller,
    State(st): State<AppState>,
    Path(code): Path<String>,
) -> Result<StatusCode, ApiErr> {
    let h = code_hash(&code);
    let n = st
        .db
        .write(move |c| {
            c.execute(
                "DELETE FROM pairings WHERE code_hash = ?1 AND user_id = ?2 AND claimed_at IS NULL",
                params![h, uid],
            )
        })
        .await
        .map_err(internal)?;
    if n == 0 {
        Err(ae(StatusCode::NOT_FOUND, NOT_FOUND))
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

#[derive(Deserialize)]
struct ClaimReq {
    code: String,
    platform: String,
    device_name: String,
    push_token: Option<String>,
}

type Peer = Option<Extension<ConnectInfo<std::net::SocketAddr>>>;

/// ponytail: copy of ui::limited's IP logic, but counts failures only, keyed "pair:{ip}" in the shared map.
fn client_key(h: &HeaderMap, peer: Peer) -> String {
    let xff = (std::env::var("OPENFAULT_TRUST_PROXY").as_deref() == Ok("1"))
        .then(|| h.get("x-forwarded-for")?.to_str().ok()?.split(',').next())
        .flatten()
        .map(|s| s.trim().to_string());
    let ip = xff.or(peer.map(|Extension(ConnectInfo(a))| a.ip().to_string()));
    format!("pair:{}", ip.unwrap_or_default())
}

async fn claim_route(
    State(st): State<AppState>,
    peer: Peer,
    h: HeaderMap,
    Json(b): Json<ClaimReq>,
) -> Result<Json<Claimed>, ApiErr> {
    let key = client_key(&h, peer);
    let t = now();
    {
        let mut m = st.attempts.lock().unwrap();
        if m.len() > 10_000 {
            m.clear();
        }
        let e = m.entry(key.clone()).or_insert((0, t));
        if t - e.1 >= 900 {
            *e = (0, t);
        }
        if e.0 >= 10 {
            return Err(ae(
                StatusCode::TOO_MANY_REQUESTS,
                "too many attempts, try again later",
            ));
        }
    }
    let r = claim(&st, &b.code, &b.platform, &b.device_name, b.push_token).await;
    if matches!(r, Err(ApiErr(StatusCode::NOT_FOUND, _))) {
        st.attempts.lock().unwrap().entry(key).or_insert((0, t)).0 += 1;
    }
    r.map(Json)
}
