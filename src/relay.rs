//! `openfault relay`: holds the APNs key so self-hosted servers don't have to (PLAN §10).
//! Env: APNS_KEY_P8 (PEM) or APNS_KEY_P8_FILE, APNS_KEY_ID, APNS_TEAM_ID, APNS_BUNDLE_ID,
//! RELAY_TOKEN, APNS_SANDBOX=1 (development host), PORT (default 8080).
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, Mutex};

const JWT_TTL: i64 = 50 * 60;
const MAX_TOKENS: usize = 500;

pub struct Apns {
    key: EcdsaKeyPair,
    key_id: String,
    team_id: String,
    topic: String,
    host: &'static str,
    relay_token: String,
    jwt: Mutex<(String, i64)>,
    http: reqwest::Client,
}

/// PKCS#8 PEM (Apple's .p8) → signing key.
pub fn load_key(pem: &str) -> Result<EcdsaKeyPair, String> {
    let b64: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("APNS_KEY_P8 is not PEM: {e}"))?;
    EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &der,
        &ring::rand::SystemRandom::new(),
    )
    .map_err(|e| format!("APNS_KEY_P8 is not a P-256 PKCS#8 key: {e}"))
}

/// ES256 provider token: header {alg, kid}, claims {iss: team id, iat}.
pub fn jwt(key: &EcdsaKeyPair, key_id: &str, team_id: &str, iat: i64) -> Result<String, String> {
    let head = B64.encode(json!({"alg": "ES256", "kid": key_id}).to_string());
    let claims = B64.encode(json!({"iss": team_id, "iat": iat}).to_string());
    let input = format!("{head}.{claims}");
    let sig = key
        .sign(&ring::rand::SystemRandom::new(), input.as_bytes())
        .map_err(|_| "signing failed".to_string())?;
    Ok(format!("{input}.{}", B64.encode(sig.as_ref())))
}

impl Apns {
    pub fn from_env() -> Result<Apns, String> {
        let var = |k: &str| {
            std::env::var(k)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or(format!("{k} is required"))
        };
        let pem = match var("APNS_KEY_P8") {
            Ok(p) => p.replace("\\n", "\n"),
            Err(_) => {
                let f = var("APNS_KEY_P8_FILE")
                    .map_err(|_| "APNS_KEY_P8 or APNS_KEY_P8_FILE is required".to_string())?;
                std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?
            }
        };
        let sandbox = std::env::var("APNS_SANDBOX").as_deref() == Ok("1");
        Ok(Apns {
            key: load_key(&pem)?,
            key_id: var("APNS_KEY_ID")?,
            team_id: var("APNS_TEAM_ID")?,
            topic: var("APNS_BUNDLE_ID")?,
            host: if sandbox {
                "api.sandbox.push.apple.com"
            } else {
                "api.push.apple.com"
            },
            relay_token: var("RELAY_TOKEN")?,
            jwt: Mutex::new((String::new(), 0)),
            http: reqwest::Client::builder()
                .http2_prior_knowledge()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| e.to_string())?,
        })
    }

    /// Cached; Apple rejects tokens older than 1h and refreshes more often than every 20 min.
    fn token(&self) -> Result<String, String> {
        let now = crate::db::now();
        let mut g = self.jwt.lock().unwrap();
        if g.0.is_empty() || now - g.1 >= JWT_TTL {
            *g = (jwt(&self.key, &self.key_id, &self.team_id, now)?, now);
        }
        Ok(g.0.clone())
    }
}

#[derive(Deserialize)]
struct PushReq {
    tokens: Vec<String>,
    title: String,
    body: String,
    url: String,
    collapse_id: Option<String>,
    badge: Option<u32>,
}

fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

async fn push(State(a): State<Arc<Apns>>, h: HeaderMap, Json(r): Json<PushReq>) -> Response {
    let bearer = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // blake3::Hash equality is constant-time.
    if !bearer.is_some_and(|b| blake3::hash(b.as_bytes()) == blake3::hash(a.relay_token.as_bytes()))
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "bad token"})),
        )
            .into_response();
    }
    if r.tokens.len() > MAX_TOKENS {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": "too many tokens"})),
        )
            .into_response();
    }
    let jwt = match a.token() {
        Ok(t) => t,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
        }
    };
    let collapse: Option<String> = r
        .collapse_id
        .as_deref()
        .map(|c| c.chars().filter(char::is_ascii_graphic).take(64).collect());
    let mut aps = json!({"alert": {"title": trunc(&r.title, 200), "body": trunc(&r.body, 400)}, "sound": "default"});
    if let Some(c) = &collapse {
        aps["thread-id"] = c.clone().into();
    }
    if let Some(b) = r.badge {
        aps["badge"] = b.into();
    }
    let payload = json!({"aps": aps, "url": trunc(&r.url, 1000)}).to_string();
    let mut results = Vec::with_capacity(r.tokens.len());
    // ponytail: sequential over one HTTP/2 connection; join_all in chunks if teams get large.
    for tok in r.tokens {
        // tokens go into the URL path: hex only.
        if tok.is_empty() || tok.len() > 200 || !tok.bytes().all(|b| b.is_ascii_hexdigit()) {
            results.push(json!({"token": tok, "status": 400, "reason": "BadDeviceToken"}));
            continue;
        }
        let mut req = a
            .http
            .post(format!("https://{}/3/device/{tok}", a.host))
            .header("authorization", format!("bearer {jwt}"))
            .header("apns-topic", &a.topic)
            .header("apns-push-type", "alert")
            .header("apns-priority", "10")
            .body(payload.clone());
        if let Some(c) = &collapse {
            req = req.header("apns-collapse-id", c);
        }
        let (status, reason) = match req.send().await {
            Ok(res) => {
                let s = res.status().as_u16();
                let reason = if s == 200 {
                    None
                } else {
                    res.json::<serde_json::Value>()
                        .await
                        .ok()
                        .and_then(|v| v["reason"].as_str().map(String::from))
                };
                (s, reason)
            }
            Err(e) => (502, Some(e.without_url().to_string())),
        };
        if status == 403 && reason.as_deref() == Some("ExpiredProviderToken") {
            a.jwt.lock().unwrap().0.clear();
        }
        results.push(json!({"token": tok, "status": status, "reason": reason}));
    }
    Json(json!({"results": results})).into_response()
}

pub fn router(a: Arc<Apns>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/push", post(push))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .with_state(a)
}

pub async fn run() {
    let a = Apns::from_env().unwrap_or_else(|e| {
        eprintln!("openfault relay: {e}");
        std::process::exit(1);
    });
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind");
    tracing::info!(host = a.host, "push relay listening on :{port}");
    axum::serve(listener, router(Arc::new(a)))
        .await
        .expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, KeyPair, UnparsedPublicKey};

    #[test]
    fn bad_p8_is_an_error_not_a_panic() {
        for pem in [
            "",
            "garbage!!",
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----",
        ] {
            assert!(load_key(pem).is_err(), "{pem}");
        }
    }

    #[test]
    fn jwt_shape_and_signature() {
        let doc = EcdsaKeyPair::generate_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &ring::rand::SystemRandom::new(),
        )
        .unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(doc.as_ref())
        );
        let key = load_key(&pem).unwrap();
        let t = jwt(&key, "KEY123", "TEAM456", 1_700_000_000).unwrap();
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        let dec =
            |s: &str| serde_json::from_slice::<serde_json::Value>(&B64.decode(s).unwrap()).unwrap();
        assert_eq!(dec(parts[0]), json!({"alg": "ES256", "kid": "KEY123"}));
        assert_eq!(
            dec(parts[1]),
            json!({"iss": "TEAM456", "iat": 1_700_000_000})
        );
        let sig = B64.decode(parts[2]).unwrap();
        assert_eq!(sig.len(), 64, "JWS ES256 is raw r||s");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, key.public_key().as_ref())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }
}
