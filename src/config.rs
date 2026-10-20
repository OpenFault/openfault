//! Instance settings (PLAN §0): `orgs.settings_json` is the source of truth, edited in the setup
//! wizard, Settings → Organization or over MCP. Env vars still win (the UI shows "set by environment").
//! Secrets (SMTP password, relay token) live here too: the SQLite file is the trust boundary. They are
//! never rendered back into forms and never logged.
use crate::db::Db;
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Default, Clone, JsonSchema)]
#[serde(default)]
pub struct Smtp {
    pub host: String,
    /// 0 = the mode's default (587 starttls, 465 tls, 25 none)
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    /// starttls | tls | none
    pub mode: String,
}

#[derive(Serialize, Deserialize, Default, Clone, JsonSchema)]
#[serde(default)]
pub struct Relay {
    pub url: String,
    pub token: String,
}

/// S3-compatible backups (src/backup.rs). Empty endpoint = AWS (`https://s3.{region}.amazonaws.com`).
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Backup {
    pub enabled: bool,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
    /// 0-23
    pub hour_utc: u8,
    pub keep: u32,
    /// `{endpoint}/{bucket}/{key}` (R2, MinIO) instead of `{bucket}.{host}`
    pub path_style: bool,
}

impl Default for Backup {
    fn default() -> Self {
        Backup {
            enabled: false,
            endpoint: String::new(),
            region: "us-east-1".into(),
            bucket: String::new(),
            prefix: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            hour_utc: 3,
            keep: 14,
            path_style: true,
        }
    }
}

/// What `orgs.settings_json` holds (other keys in the JSON are left alone on save).
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Stored {
    pub base_url: Option<String>,
    pub smtp: Option<Smtp>,
    pub push_relay: Option<Relay>,
    pub allow_private_webhooks: bool,
    pub backup: Option<Backup>,
}

/// Stored + env overrides.
pub struct Config {
    pub stored: Stored,
    pub base_url: Option<String>,
    pub base_env: bool,
    pub smtp: Option<crate::mail::Smtp>,
    pub smtp_env: bool,
    /// why SMTP settings were ignored (bad from address, …)
    pub smtp_error: Option<String>,
    pub relay: Option<Relay>,
    pub relay_env: bool,
    pub allow_private: bool,
    pub allow_env: bool,
    /// stored + OPENFAULT_BACKUP_S3_*; None unless enabled with a bucket
    pub backup: Option<Backup>,
    /// OPENFAULT_BACKUP_S3_BUCKET is set (the form is read-only then)
    pub backup_env: bool,
}

pub type Cache = Arc<Mutex<Option<(Instant, Arc<Config>)>>>;
const TTL: Duration = Duration::from_secs(60);

/// Mirrors the last loaded allow_private for the webhook DNS resolver, which has no Db.
/// ponytail: process-global; fine with one org per process.
pub static ALLOW_PRIVATE: AtomicBool = AtomicBool::new(false);

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

pub fn load(c: &Connection) -> Stored {
    c.query_row(
        "SELECT settings_json FROM orgs ORDER BY id LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|s| serde_json::from_str(&s).ok())
    .unwrap_or_default()
}

/// Writes `s` into org 1's settings_json (other keys kept).
pub fn store(c: &Connection, s: &Stored) -> rusqlite::Result<()> {
    let j = |v: serde_json::Result<String>| v.unwrap_or_else(|_| "null".into());
    c.execute(
        "UPDATE orgs SET settings_json = json_set(settings_json, '$.base_url', json(?1), '$.smtp', json(?2),
           '$.push_relay', json(?3), '$.allow_private_webhooks', json(?4), '$.backup', json(?5)) WHERE id = 1",
        [
            j(serde_json::to_string(&s.base_url)),
            j(serde_json::to_string(&s.smtp)),
            j(serde_json::to_string(&s.push_relay)),
            j(serde_json::to_string(&s.allow_private_webhooks)),
            j(serde_json::to_string(&s.backup)),
        ],
    )?;
    Ok(())
}

/// OPENFAULT_BASE_URL, else the stored one; no trailing slash.
pub fn base(stored: &Stored) -> Option<String> {
    env("OPENFAULT_BASE_URL")
        .or_else(|| stored.base_url.clone())
        .filter(|u| !u.is_empty())
        .map(|u| u.trim_end_matches('/').to_string())
}

pub fn effective(stored: Stored) -> Config {
    let trim = |u: String| u.trim_end_matches('/').to_string();
    let base_env = env("OPENFAULT_BASE_URL");
    let smtp_url = env("OPENFAULT_SMTP_URL");
    let (smtp, smtp_error) = match (&smtp_url, &stored.smtp) {
        (Some(u), _) => split(crate::mail::parse(u), "OPENFAULT_SMTP_URL"),
        (None, Some(s)) if !s.host.is_empty() => split(crate::mail::build(s), "SMTP settings"),
        _ => (None, None),
    };
    let relay_url = env("OPENFAULT_PUSH_RELAY_URL");
    let relay = match &relay_url {
        Some(u) => env("OPENFAULT_PUSH_RELAY_TOKEN").map(|token| Relay {
            url: u.clone(),
            token,
        }),
        None => stored.push_relay.clone(),
    }
    .filter(|r| !r.url.is_empty() && !r.token.is_empty())
    .map(|r| Relay {
        url: trim(r.url),
        ..r
    });
    let allow_env = std::env::var("OPENFAULT_ALLOW_PRIVATE_WEBHOOKS").as_deref() == Ok("1");
    let allow_private = allow_env || stored.allow_private_webhooks;
    ALLOW_PRIVATE.store(allow_private, Ordering::Relaxed);
    let backup_env = env("OPENFAULT_BACKUP_S3_BUCKET").is_some();
    let backup = backup_from_env(stored.backup.clone().unwrap_or_default());
    Config {
        backup: Some(backup).filter(|b| b.enabled && !b.bucket.is_empty()),
        backup_env,
        base_url: base(&stored),
        base_env: base_env.is_some(),
        smtp,
        smtp_env: smtp_url.is_some(),
        smtp_error,
        relay,
        relay_env: relay_url.is_some(),
        allow_private,
        allow_env,
        stored,
    }
}

/// Env vars win per field; OPENFAULT_BACKUP_S3_BUCKET also turns backups on.
pub fn backup_from_env(mut b: Backup) -> Backup {
    let e = |k: &str| env(&format!("OPENFAULT_BACKUP_S3_{k}"));
    for (k, f) in [
        ("ENDPOINT", &mut b.endpoint),
        ("REGION", &mut b.region),
        ("BUCKET", &mut b.bucket),
        ("PREFIX", &mut b.prefix),
        ("ACCESS_KEY", &mut b.access_key),
        ("SECRET_KEY", &mut b.secret_key),
    ] {
        if let Some(v) = e(k) {
            *f = v;
        }
    }
    if e("BUCKET").is_some() {
        b.enabled = true;
    }
    if let Some(h) = e("HOUR").and_then(|v| v.parse().ok()).filter(|h| *h < 24) {
        b.hour_utc = h;
    }
    if let Some(k) = e("KEEP").and_then(|v| v.parse().ok()) {
        b.keep = k;
    }
    b
}

fn split(
    r: Result<crate::mail::Smtp, String>,
    what: &str,
) -> (Option<crate::mail::Smtp>, Option<String>) {
    match r {
        Ok(s) => (Some(s), None),
        Err(e) => {
            tracing::error!("{what} ignored: {e}");
            (None, Some(e))
        }
    }
}

/// Effective config, cached 60s per Db. Saves call `invalidate`, so the TTL only matters for
/// edits made outside this process.
pub fn get(db: &Db) -> Arc<Config> {
    let mut g = db.cfg.lock().unwrap();
    if let Some((t, c)) = g.as_ref()
        && t.elapsed() < TTL
    {
        return c.clone();
    }
    let c = Arc::new(effective(db.read_now(|c| Ok(load(c))).unwrap_or_default()));
    *g = Some((Instant::now(), c.clone()));
    c
}

pub fn invalidate(db: &Db) {
    *db.cfg.lock().unwrap() = None;
}

/// Read-modify-write of the stored settings.
pub async fn update(
    db: &Db,
    f: impl FnOnce(&mut Stored) + Send + 'static,
) -> crate::db::Result<()> {
    db.write(move |c| {
        let mut s = load(c);
        f(&mut s);
        store(c, &s)
    })
    .await?;
    invalidate(db);
    get(db); // refreshes ALLOW_PRIVATE
    Ok(())
}

/// Base URL for links, else http://{Host}.
pub fn base_or(db: &Db, host: Option<&str>) -> String {
    get(db)
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", host.unwrap_or("localhost:8080")))
}
