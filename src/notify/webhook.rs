//! Per-project webhooks: Slack (blocks), Discord (embeds), generic JSON (+ optional HMAC-SHA256).
use super::{Info, fmt_time, label, one_line};
use crate::db::{Db, now};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::LazyLock;
use std::time::Duration;

pub const KINDS: [&str; 3] = ["slack", "discord", "generic"];

/// Push relay client (operator-configured URL, may be private). No redirects.
pub static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| client(false));
/// Webhook client: no redirects (a 30x to a private host would bypass `validate_url`), and every
/// resolved address is checked at connect time, so DNS pointing at / rebinding to a private IP fails.
static HOOKS: LazyLock<reqwest::Client> = LazyLock::new(|| client(true));

fn client(public_only: bool) -> reqwest::Client {
    let b = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("OpenFault/", env!("CARGO_PKG_VERSION")));
    let b = if public_only {
        b.dns_resolver(std::sync::Arc::new(PublicOnly))
    } else {
        b
    };
    b.build().expect("http client")
}

fn allow_private() -> bool {
    std::env::var("OPENFAULT_ALLOW_PRIVATE_WEBHOOKS").as_deref() == Ok("1")
}

/// ponytail: an HTTP(S)_PROXY env var bypasses this (the proxy resolves the host).
struct PublicOnly;
impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|a| allow_private() || !private_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("host resolves only to private addresses".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[derive(Clone, Debug)]
pub struct Hook {
    pub id: i64,
    pub kind: String,
    pub url: String,
    pub secret: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebhookRow {
    pub id: i64,
    pub kind: String,
    pub url: String,
    pub enabled: bool,
    pub has_secret: bool,
    pub last_error: Option<String>,
    pub last_ok_at: Option<i64>,
}

fn private_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || o[0] == 0
        || o[0] >= 224 // multicast, reserved, broadcast
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking
        || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => private_v4(v4),
        IpAddr::V6(v6) => {
            let g = v6.segments();
            let s = g[0];
            let nat64 = g[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s & 0xfe00) == 0xfc00 // unique local
                || (s & 0xffc0) == 0xfe80 // link local
                // ::ffff:a.b.c.d, ::a.b.c.d and 64:ff9b::a.b.c.d embed a v4 address
                || v6.to_ipv4().is_some_and(private_v4)
                || (nat64 && private_v4(Ipv4Addr::from(((g[6] as u32) << 16) | g[7] as u32)))
        }
    }
}

/// https only; no localhost or literal private/link-local IPs unless OPENFAULT_ALLOW_PRIVATE_WEBHOOKS=1,
/// which also allows plain http to loopback. Hostnames are checked again after DNS (see `PublicOnly`).
pub fn validate_url(u: &str) -> Result<reqwest::Url, String> {
    if u.len() > 2048 {
        return Err("URL too long".into());
    }
    let url = reqwest::Url::parse(u).map_err(|e| format!("invalid URL: {e}"))?;
    let allow = allow_private();
    let host = url.host_str().ok_or("URL needs a host")?;
    let (private, loopback) = match host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        Ok(ip) => (private_ip(ip), ip.is_loopback()),
        Err(_) => {
            let d = host.trim_end_matches('.').to_ascii_lowercase();
            let lh = d == "localhost" || d.ends_with(".localhost");
            (lh || d.ends_with(".local") || d.ends_with(".internal"), lh)
        }
    };
    if private && !allow {
        return Err("private, local and loopback addresses are not allowed (set OPENFAULT_ALLOW_PRIVATE_WEBHOOKS=1)".into());
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if allow && loopback => Ok(url),
        _ => Err("webhook URL must use https".into()),
    }
}

pub async fn create(
    db: &Db,
    project_id: i64,
    kind: &str,
    url: &str,
    secret: Option<String>,
) -> Result<i64, String> {
    if !KINDS.contains(&kind) {
        return Err("kind must be slack|discord|generic".into());
    }
    validate_url(url)?;
    let (kind, url) = (kind.to_string(), url.to_string());
    let secret = secret.filter(|s| !s.is_empty());
    db.write(move |c| {
        c.execute(
            "INSERT INTO webhooks (project_id, kind, url, secret) VALUES (?1, ?2, ?3, ?4)",
            params![project_id, kind, url, secret],
        )?;
        Ok(c.last_insert_rowid())
    })
    .await
    .map_err(|e| e.to_string())
}

/// Secrets are never returned, only `has_secret`.
pub async fn list(db: &Db, project_id: i64) -> crate::db::Result<Vec<WebhookRow>> {
    db.read(move |c| {
        c.prepare("SELECT id, kind, url, enabled, secret IS NOT NULL, last_error, last_ok_at FROM webhooks WHERE project_id = ?1 ORDER BY id")?
            .query_map([project_id], |r| {
                Ok(WebhookRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    url: r.get(2)?,
                    enabled: r.get(3)?,
                    has_secret: r.get(4)?,
                    last_error: r.get(5)?,
                    last_ok_at: r.get(6)?,
                })
            })?
            .collect()
    })
    .await
}

/// Scoped by project so a form can't touch another project's hook. Returns false if not found.
pub async fn set_enabled(
    db: &Db,
    project_id: i64,
    id: i64,
    enabled: bool,
) -> crate::db::Result<bool> {
    db.write(move |c| {
        c.execute(
            "UPDATE webhooks SET enabled = ?3 WHERE id = ?1 AND project_id = ?2",
            params![id, project_id, enabled],
        )
        .map(|n| n == 1)
    })
    .await
}

pub async fn delete(db: &Db, project_id: i64, id: i64) -> crate::db::Result<bool> {
    db.write(move |c| {
        c.execute(
            "DELETE FROM webhooks WHERE id = ?1 AND project_id = ?2",
            params![id, project_id],
        )
        .map(|n| n == 1)
    })
    .await
}

pub(super) fn enabled(c: &Connection, project_id: i64) -> rusqlite::Result<Vec<Hook>> {
    c.prepare("SELECT id, kind, url, secret FROM webhooks WHERE project_id = ?1 AND enabled = 1 ORDER BY id")?
        .query_map([project_id], |r| Ok(Hook { id: r.get(0)?, kind: r.get(1)?, url: r.get(2)?, secret: r.get(3)? }))?
        .collect()
}

pub(super) fn record(c: &Connection, id: i64, err: Option<&str>, t: i64) -> rusqlite::Result<()> {
    match err {
        None => c.execute(
            "UPDATE webhooks SET last_ok_at = ?2, last_error = NULL WHERE id = ?1",
            params![id, t],
        )?,
        Some(e) => c.execute(
            "UPDATE webhooks SET last_error = ?2 WHERE id = ?1",
            params![id, e],
        )?,
    };
    Ok(())
}

/// Slack mrkdwn: escape the three control characters so `<!channel>` etc. can't be injected.
fn slack_esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn body(kind: &str, i: &Info) -> Value {
    let head = format!("[{}] {}: {}", i.project, label(&i.kind), i.title);
    let release = i.release.clone().unwrap_or_else(|| "-".into());
    match kind {
        "slack" => {
            let f = |k: &str, v: &str| json!({"type": "mrkdwn", "text": format!("*{k}*\n{}", slack_esc(v))});
            json!({
                "text": slack_esc(&one_line(&head, 300)),
                "blocks": [
                    {"type": "header", "text": {"type": "plain_text", "text": one_line(&format!("{}: {}", label(&i.kind), i.title), 150)}},
                    {"type": "section", "text": {"type": "mrkdwn", "text": format!("`{}`", slack_esc(&one_line(&i.culprit, 300)).replace('`', "'"))}},
                    {"type": "section", "fields": [
                        f("Project", &one_line(&i.project, 100)), f("Level", &i.level),
                        f("Events", &i.events.to_string()), f("Users", &i.users.to_string()),
                        f("Release", &one_line(&release, 100)), f("Last seen", &fmt_time(i.last_seen)),
                    ]},
                    {"type": "actions", "elements": [{"type": "button", "text": {"type": "plain_text", "text": "Open issue"}, "url": i.url}]},
                ]
            })
        }
        "discord" => {
            let f = |k: &str, v: String| json!({"name": k, "value": one_line(&v, 1000), "inline": true});
            json!({
                "allowed_mentions": {"parse": []},
                "embeds": [{
                    "title": one_line(&head, 256),
                    "url": i.url,
                    "description": one_line(&i.culprit, 2000),
                    "fields": [
                        f("Level", i.level.clone()), f("Events", i.events.to_string()),
                        f("Users", i.users.to_string()), f("Release", release),
                    ],
                    "timestamp": jiff::Timestamp::from_second(i.last_seen).map(|t| t.to_string()).unwrap_or_default(),
                }]
            })
        }
        _ => json!({
            "kind": i.kind,
            "project": i.project,
            "issue": {
                "id": i.id, "title": i.title, "level": i.level, "url": i.url, "events": i.events, "users": i.users,
                "first_seen": i.first_seen, "last_seen": i.last_seen, "release": i.release,
            }
        }),
    }
}

/// `sha256=<hex hmac(secret, body)>`
pub fn signature(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

pub async fn send(h: &Hook, i: &Info) -> Result<(), String> {
    let url = validate_url(&h.url)?;
    let body = serde_json::to_vec(&body(&h.kind, i)).map_err(|e| e.to_string())?;
    let mut req = HOOKS.post(url).header("content-type", "application/json");
    if let Some(s) = &h.secret {
        // signed "{ts}.{body}" so a captured request can't be replayed later with a fresh timestamp.
        let ts = now().to_string();
        let signed = [ts.as_bytes(), b".", &body].concat();
        req = req
            .header("X-OpenFault-Timestamp", ts)
            .header("X-OpenFault-Signature", signature(s, &signed));
    }
    let res = req
        .body(body)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if res.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", res.status().as_u16()))
    }
}

/// Sends a sample payload and records last_ok_at / last_error on the row.
pub async fn test_webhook(db: &Db, id: i64) -> Result<(), String> {
    let hook = db
        .read(move |c| {
            c.query_row(
                "SELECT h.id, h.kind, h.url, h.secret, p.name FROM webhooks h JOIN projects p ON p.id = h.project_id WHERE h.id = ?1",
                [id],
                |r| Ok((Hook { id: r.get(0)?, kind: r.get(1)?, url: r.get(2)?, secret: r.get(3)? }, r.get::<_, String>(4)?)),
            )
        })
        .await
        .map_err(|_| "webhook not found".to_string())?;
    let (h, project) = hook;
    let t = now();
    let sample = Info {
        kind: "test".into(),
        project,
        id: 0,
        title: "Test notification from OpenFault".into(),
        culprit: "openfault.test".into(),
        level: "info".into(),
        url: format!("{}/settings", super::base_url()),
        events: 1,
        users: 1,
        first_seen: t,
        last_seen: t,
        release: None,
    };
    let r = send(&h, &sample).await;
    let e = r.clone().err();
    db.write(move |c| record(c, id, e.as_deref(), t))
        .await
        .map_err(|e| e.to_string())?;
    r
}

#[cfg(test)]
mod tests {
    use super::validate_url;

    #[test]
    fn url_rules() {
        assert!(validate_url("https://hooks.slack.com/services/x").is_ok());
        for bad in [
            "http://example.com/x",
            "https://localhost/x",
            "https://127.0.0.1/x",
            "https://10.1.2.3/x",
            "https://169.254.169.254/latest",
            "https://[::1]/x",
            "https://[fd00::1]/x",
            "https://[::ffff:192.168.0.1]/x",
            "https://[::ffff:127.0.0.1]/x",
            "https://[::127.0.0.1]/x",
            "https://[64:ff9b::a9fe:a9fe]/x",
            "https://[ff02::1]/x",
            "https://2130706433/x",
            "https://0177.0.0.1/x",
            "https://0x7f.1/x",
            "https://0/x",
            "https://224.0.0.1/x",
            "ftp://example.com",
        ] {
            assert!(validate_url(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn resolver_drops_private_addresses() {
        use reqwest::dns::Resolve;
        // a hostname passes validate_url; its addresses are checked at connect time.
        let r = super::PublicOnly
            .resolve("localhost".parse().unwrap())
            .await;
        assert!(r.is_err());
    }

    #[test]
    fn hmac_known_vector() {
        // RFC 4231 test case 2
        assert_eq!(
            super::signature("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
