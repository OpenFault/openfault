//! Notification pipeline (PLAN §10). Triggers `enqueue` a `notif_queue` row inside the ingest
//! transaction (never blocks ingest); `run` drains the queue every 2s and fans out to email,
//! webhooks and push. Rows stay 24h as the Settings delivery log (PLAN §13b: no silent failures).
pub mod email;
pub mod push;
pub mod webhook;

use crate::db::{Db, now};
use crate::issues::untrusted;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;

pub const KINDS: [&str; 4] = ["new_issue", "regression", "unmuted", "spike"];
const MAX_ATTEMPTS: i64 = 3;
const DEDUPE_SECS: i64 = 3600;
pub const MAX_PENDING: i64 = 500;

pub struct Notification {
    pub project_id: i64,
    pub issue_id: i64,
    pub kind: &'static str,
    pub title: String,
    pub level: String,
    pub url: String,
}

impl Notification {
    pub fn new(
        project_id: i64,
        issue_id: i64,
        kind: &'static str,
        title: &str,
        level: &str,
    ) -> Self {
        Notification {
            project_id,
            issue_id,
            kind,
            title: title.to_string(),
            level: level.to_string(),
            url: issue_url(issue_id),
        }
    }
}

/// OPENFAULT_BASE_URL without the trailing slash. ponytail: localhost fallback for dev; main.rs requires it with SMTP.
pub fn base_url() -> String {
    std::env::var("OPENFAULT_BASE_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| "http://localhost:8080".into())
}

pub fn issue_url(id: i64) -> String {
    format!("{}/issues/{id}", base_url())
}

pub fn label(kind: &str) -> &'static str {
    match kind {
        "new_issue" => "New issue",
        "regression" => "Regression",
        "unmuted" => "Unmuted",
        "spike" => "Spike",
        _ => "Test",
    }
}

/// Call with the ingest transaction (or any connection); the worker picks the row up.
/// Bounded: past MAX_PENDING undelivered rows per project (a flood of new issues, a dead SMTP
/// server) new triggers are dropped with a warning instead of growing the queue.
pub fn enqueue(c: &Connection, n: &Notification) -> rusqlite::Result<()> {
    let payload = json!({"title": n.title, "level": n.level, "url": n.url}).to_string();
    let added = c.execute(
        "INSERT INTO notif_queue (project_id, issue_id, kind, payload_json, created_at) SELECT ?1, ?2, ?3, ?4, ?5
         WHERE (SELECT count(*) FROM notif_queue WHERE project_id = ?1 AND done_at IS NULL AND attempts < ?6) < ?7",
        params![n.project_id, n.issue_id, n.kind, payload, now(), MAX_ATTEMPTS, MAX_PENDING],
    )?;
    if added == 0 {
        c.execute(
            "UPDATE projects SET notif_dropped = notif_dropped + 1 WHERE id = ?1",
            [n.project_id],
        )?;
        tracing::warn!(
            project_id = n.project_id,
            issue_id = n.issue_id,
            "notification queue full, {} dropped",
            n.kind
        );
    }
    Ok(())
}

/// Spike: this hour's count ≥ 20 and > 10× the median of the previous 24 hourly counts (missing
/// hours = 0). At most once per issue per 6h; stamps `issues.last_spike_at` when it fires.
pub fn check_spike(
    c: &Connection,
    issue_id: i64,
    hour: i64,
    count: i64,
    t: i64,
) -> rusqlite::Result<bool> {
    if count < 20 {
        return Ok(false);
    }
    let last: Option<i64> = c.query_row(
        "SELECT last_spike_at FROM issues WHERE id = ?1",
        [issue_id],
        |r| r.get(0),
    )?;
    if last.is_some_and(|l| t - l < 6 * 3600) {
        return Ok(false);
    }
    let mut v: Vec<i64> = c
        .prepare("SELECT count FROM issue_hourly WHERE issue_id = ?1 AND hour_ts >= ?2 - 86400 AND hour_ts < ?2")?
        .query_map(params![issue_id, hour], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    v.resize(24, 0);
    v.sort_unstable();
    // count > 10 × (v[11]+v[12])/2, kept in integers.
    if count * 2 <= 10 * (v[11] + v[12]) {
        return Ok(false);
    }
    c.execute(
        "UPDATE issues SET last_spike_at = ?2 WHERE id = ?1",
        params![issue_id, t],
    )?;
    Ok(true)
}

/// Columns added after v1 shipped its CREATE TABLEs, added only if missing (PRAGMA table_info),
/// so real errors surface. Runs in `Db::open` right after schema.sql.
/// ponytail: stand-in for a migration framework, see schema.sql header.
pub fn migrate(c: &Connection) -> rusqlite::Result<()> {
    for (table, col, ty) in [
        ("issues", "last_spike_at", "INTEGER"),
        ("webhooks", "secret", "TEXT"),
        ("webhooks", "last_ok_at", "INTEGER"),
        ("devices", "name", "TEXT"),
        ("devices", "token_id", "INTEGER"),
        ("projects", "notif_dropped", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        let has: bool = c.query_row(
            "SELECT count(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
            [table, col],
            |r| r.get(0),
        )?;
        if !has {
            c.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"), [])?;
        }
    }
    // devices.push_token became nullable (pairing without APNs permission); SQLite needs a rebuild.
    let notnull: bool = c.query_row(
        "SELECT \"notnull\" FROM pragma_table_info('devices') WHERE name = 'push_token'",
        [],
        |r| r.get(0),
    )?;
    if notnull {
        c.execute_batch(
            "BEGIN;
             CREATE TABLE devices_new (
               id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, platform TEXT NOT NULL,
               push_token TEXT, created_at INTEGER NOT NULL, name TEXT, token_id INTEGER);
             INSERT INTO devices_new SELECT id, user_id, platform, push_token, created_at, name, token_id FROM devices;
             DROP TABLE devices;
             ALTER TABLE devices_new RENAME TO devices;
             CREATE UNIQUE INDEX devices_user_token ON devices(user_id, push_token);
             COMMIT;",
        )?;
    }
    Ok(())
}

// ---------- preferences ----------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Pref {
    pub project_id: Option<i64>,
    pub kind: String,
    pub email: bool,
    pub push: bool,
}

/// The user's stored rows. Resolution: project row, else `project_id NULL` row, else email+push on.
pub async fn prefs_for(db: &Db, user_id: i64) -> crate::db::Result<Vec<Pref>> {
    db.read(move |c| {
        c.prepare("SELECT project_id, kind, email, push FROM notif_prefs WHERE user_id = ?1 ORDER BY project_id, kind")?
            .query_map([user_id], |r| {
                Ok(Pref { project_id: r.get(0)?, kind: r.get(1)?, email: r.get(2)?, push: r.get(3)? })
            })?
            .collect()
    })
    .await
}

/// Replaces all of the user's rows. Callers validate kinds (KINDS) and project access.
pub async fn set_prefs(db: &Db, user_id: i64, prefs: Vec<Pref>) -> crate::db::Result<()> {
    db.write(move |c| {
        let tx = c.transaction()?;
        tx.execute("DELETE FROM notif_prefs WHERE user_id = ?1", [user_id])?;
        for p in &prefs {
            tx.execute(
                "INSERT INTO notif_prefs (user_id, project_id, kind, email, push) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![user_id, p.project_id, p.kind, p.email, p.push],
            )?;
        }
        tx.commit()
    })
    .await
}

// ---------- delivery log ----------

#[derive(Clone, Debug, Serialize)]
pub struct Delivery {
    pub id: i64,
    pub issue_id: i64,
    pub kind: String,
    pub title: String,
    pub created_at: i64,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub done_at: Option<i64>,
    /// channel deliveries (emails, pushes, webhooks) recorded for this issue+kind since the row was queued.
    pub sent: i64,
}

impl Delivery {
    /// "sent" | "failed" | "pending"
    pub fn status(&self) -> &'static str {
        match (self.done_at, self.attempts >= MAX_ATTEMPTS) {
            (Some(_), _) => "sent",
            (None, true) => "failed",
            _ => "pending",
        }
    }
}

/// Last `limit` queue rows (≤ 24h old) for Settings. Titles are event data: already `untrusted`, still escape in templates.
pub async fn recent_deliveries(
    db: &Db,
    project_id: i64,
    limit: i64,
) -> crate::db::Result<Vec<Delivery>> {
    db.read(move |c| {
        c.prepare(
            "SELECT q.id, q.issue_id, q.kind, coalesce(json_extract(q.payload_json, '$.title'), ''), q.created_at, q.attempts, q.last_error, q.done_at,
               (SELECT count(*) FROM notif_sent s WHERE s.issue_id = q.issue_id AND s.kind = q.kind AND s.sent_at >= q.created_at)
             FROM notif_queue q WHERE q.project_id = ?1 ORDER BY q.id DESC LIMIT ?2",
        )?
        .query_map(params![project_id, limit], |r| {
            Ok(Delivery {
                id: r.get(0)?,
                issue_id: r.get(1)?,
                kind: r.get(2)?,
                title: untrusted(&r.get::<_, String>(3)?),
                created_at: r.get(4)?,
                attempts: r.get(5)?,
                last_error: r.get(6)?,
                done_at: r.get(7)?,
                sent: r.get(8)?,
            })
        })?
        .collect()
    })
    .await
}

// ---------- worker ----------

/// What every channel renders. Event-data strings are already passed through `issues::untrusted`.
#[derive(Clone, Debug, Serialize)]
pub struct Info {
    pub kind: String,
    pub project: String,
    pub id: i64,
    pub title: String,
    pub culprit: String,
    pub level: String,
    pub url: String,
    pub events: i64,
    pub users: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub release: Option<String>,
}

struct Recipient {
    id: i64,
    email: String,
    email_on: bool,
    push_on: bool,
}

struct Ctx {
    info: Info,
    users: Vec<Recipient>,
    /// (user_id, push_token), ios only
    tokens: Vec<(i64, String)>,
    hooks: Vec<webhook::Hook>,
    /// (user_id, channel) already delivered within the dedupe window
    sent: HashSet<(i64, String)>,
}

fn load(
    c: &Connection,
    pid: i64,
    iid: i64,
    kind: &str,
    url: String,
) -> rusqlite::Result<Option<Ctx>> {
    let info = c
        .query_row(
            "SELECT p.name, i.title, i.culprit, i.level, i.event_count, i.user_count, i.first_seen, i.last_seen, i.last_release
             FROM issues i JOIN projects p ON p.id = i.project_id WHERE i.id = ?1",
            [iid],
            |r| {
                Ok(Info {
                    kind: kind.to_string(),
                    project: untrusted(&r.get::<_, String>(0)?),
                    id: iid,
                    title: untrusted(&r.get::<_, String>(1)?),
                    culprit: untrusted(&r.get::<_, String>(2)?),
                    level: untrusted(&r.get::<_, String>(3)?),
                    url: url.clone(),
                    events: r.get(4)?,
                    users: r.get(5)?,
                    first_seen: r.get(6)?,
                    last_seen: r.get(7)?,
                    release: r.get::<_, Option<String>>(8)?.map(|s| untrusted(&s)),
                })
            },
        )
        .optional()?;
    let Some(info) = info else { return Ok(None) };
    // project access = ui::VISIBLE: admins/owner see all, members via project_members.
    let pref = |col: &str| {
        format!(
            "coalesce((SELECT {col} FROM notif_prefs WHERE user_id = u.id AND project_id = ?1 AND kind = ?2 LIMIT 1),
                      (SELECT {col} FROM notif_prefs WHERE user_id = u.id AND project_id IS NULL AND kind = ?2 LIMIT 1), 1)"
        )
    };
    let users: Vec<Recipient> = c
        .prepare(&format!(
            "SELECT u.id, u.email, {}, {} FROM users u
             WHERE u.role != 'member' OR u.is_owner = 1 OR u.id IN (SELECT user_id FROM project_members WHERE project_id = ?1)",
            pref("email"),
            pref("push")
        ))?
        .query_map(params![pid, kind], |r| {
            Ok(Recipient { id: r.get(0)?, email: r.get(1)?, email_on: r.get(2)?, push_on: r.get(3)? })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let push_users: Vec<i64> = users.iter().filter(|u| u.push_on).map(|u| u.id).collect();
    let mut tokens = vec![];
    let mut st =
        c.prepare("SELECT push_token FROM devices WHERE user_id = ?1 AND platform = 'ios' AND push_token IS NOT NULL")?;
    for uid in push_users {
        for t in st.query_map([uid], |r| r.get::<_, String>(0))? {
            tokens.push((uid, t?));
        }
    }
    let hooks = webhook::enabled(c, pid)?;
    let sent = c
        .prepare("SELECT user_id, channel FROM notif_sent WHERE issue_id = ?1 AND kind = ?2 AND sent_at > ?3")?
        .query_map(params![iid, kind, now() - DEDUPE_SECS], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(Ctx {
        info,
        users,
        tokens,
        hooks,
        sent,
    }))
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Health {
    pub queued: i64,
    pub failed_24h: i64,
    pub dropped: i64,
}

/// Settings counters (PLAN §13b): pending deliveries, deliveries that gave up in the last 24h,
/// and triggers dropped by the MAX_PENDING cap (lifetime).
pub async fn project_health(db: &Db, project_id: i64) -> crate::db::Result<Health> {
    db.read(move |c| {
        c.query_row(
            "SELECT
               (SELECT count(*) FROM notif_queue WHERE project_id = ?1 AND done_at IS NULL AND attempts < ?2),
               (SELECT count(*) FROM notif_queue WHERE project_id = ?1 AND done_at IS NULL AND attempts >= ?2 AND created_at >= ?3),
               coalesce((SELECT notif_dropped FROM projects WHERE id = ?1), 0)",
            params![project_id, MAX_ATTEMPTS, now() - 86400],
            |r| Ok(Health { queued: r.get(0)?, failed_24h: r.get(1)?, dropped: r.get(2)? }),
        )
    })
    .await
}

/// One pass over pending rows. Returns how many rows were attempted.
pub async fn drain(db: &Db) -> anyhow::Result<usize> {
    let rows: Vec<(i64, i64, i64, String, String)> = db
        .read(|c| {
            c.prepare(
                "SELECT id, project_id, issue_id, kind, coalesce(json_extract(payload_json, '$.url'), '')
                 FROM notif_queue WHERE done_at IS NULL AND attempts < ?1 AND created_at + attempts * attempts * 60 <= ?2
                 ORDER BY id LIMIT 50",
            )?
            .query_map([MAX_ATTEMPTS, now()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect()
        })
        .await?;
    for (qid, pid, iid, kind, url) in &rows {
        deliver(db, *qid, *pid, *iid, kind.clone(), url.clone()).await?;
    }
    Ok(rows.len())
}

async fn deliver(
    db: &Db,
    qid: i64,
    pid: i64,
    iid: i64,
    kind: String,
    url: String,
) -> anyhow::Result<()> {
    let k = kind.clone();
    let ctx = db.read(move |c| load(c, pid, iid, &k, url)).await?;
    let Some(ctx) = ctx else {
        db.write(move |c| {
            c.execute(
                "UPDATE notif_queue SET done_at = ?2, last_error = 'issue deleted' WHERE id = ?1",
                params![qid, now()],
            )
        })
        .await?;
        return Ok(());
    };
    let done = |uid: i64, ch: &str| ctx.sent.contains(&(uid, ch.to_string()));
    let mut sent: Vec<(i64, String)> = vec![];
    let mut errs: Vec<String> = vec![];

    if crate::mail::configured() || cfg!(debug_assertions) {
        for u in ctx
            .users
            .iter()
            .filter(|u| u.email_on && !done(u.id, "email"))
        {
            match email::send(&ctx.info, &u.email).await {
                Ok(()) => sent.push((u.id, "email".into())),
                Err(e) => errs.push(format!("email to {}: {e}", u.email)),
            }
        }
    }

    let mut dead: Vec<String> = vec![];
    let toks: Vec<&(i64, String)> = ctx
        .tokens
        .iter()
        .filter(|(uid, _)| !done(*uid, "push"))
        .collect();
    if let Some(relay) = push::relay()
        && !toks.is_empty()
    {
        let list: Vec<String> = toks.iter().map(|t| t.1.clone()).collect();
        match push::send(&relay, &ctx.info, &list).await {
            Ok(results) => {
                let mut ok: HashSet<i64> = HashSet::new();
                for r in results {
                    let uid = toks.iter().find(|t| t.1 == r.token).map(|t| t.0);
                    if r.ok() {
                        ok.extend(uid);
                    } else if r.dead() {
                        dead.push(r.token);
                    } else {
                        errs.push(format!(
                            "push: {} {}",
                            r.status,
                            r.reason.unwrap_or_default()
                        ));
                    }
                }
                sent.extend(ok.into_iter().map(|u| (u, "push".to_string())));
            }
            Err(e) => errs.push(format!("push relay: {e}")),
        }
    }

    let mut hook_results: Vec<(i64, Option<String>)> = vec![];
    for h in &ctx.hooks {
        let ch = format!("webhook:{}", h.id);
        if done(0, &ch) {
            continue;
        }
        let r = webhook::send(h, &ctx.info).await;
        if r.is_ok() {
            sent.push((0, ch));
        }
        let r = r.err();
        if let Some(e) = &r {
            errs.push(format!("webhook {}: {e}", h.id));
        }
        hook_results.push((h.id, r));
    }

    let err = (!errs.is_empty()).then(|| errs.join("; "));
    db.write(move |c| {
        let tx = c.transaction()?;
        let t = now();
        for (uid, ch) in &sent {
            tx.execute(
                "INSERT INTO notif_sent (user_id, issue_id, kind, channel, sent_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![uid, iid, kind, ch, t],
            )?;
        }
        for tok in &dead {
            tx.execute("DELETE FROM devices WHERE push_token = ?1", [tok])?;
        }
        for (id, e) in &hook_results {
            webhook::record(&tx, *id, e.as_deref(), t)?;
        }
        tx.execute(
            "UPDATE notif_queue SET attempts = attempts + 1, last_error = ?2, done_at = CASE WHEN ?2 IS NULL THEN ?3 END WHERE id = ?1",
            params![qid, err, t],
        )?;
        tx.commit()
    })
    .await?;
    Ok(())
}

/// Delivery log + dedupe rows older than 24h.
pub fn purge(c: &Connection, t: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM notif_queue WHERE created_at < ?1", [t - 86400])?;
    c.execute("DELETE FROM notif_sent WHERE sent_at < ?1", [t - 86400])?;
    Ok(())
}

/// Background worker: drain every 2s, purge every minute.
/// Retries back off on the row's age: attempt 2 after 1 min, attempt 3 after 4 min.
pub async fn run(db: Db) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut n = 0u64;
    loop {
        tick.tick().await;
        if let Err(e) = drain(&db).await {
            tracing::error!("notification pass failed: {e}");
        }
        n = n.wrapping_add(1);
        if n.is_multiple_of(30)
            && let Err(e) = db.write(|c| purge(c, now())).await
        {
            tracing::error!("notification purge failed: {e}");
        }
    }
}

/// One line, ≤ `n` chars: for subjects, headers, push bodies.
pub fn one_line(s: &str, n: usize) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.chars().count() > n {
        s.chars().take(n - 1).chain(['…']).collect()
    } else {
        s
    }
}

pub fn fmt_time(t: i64) -> String {
    jiff::Timestamp::from_second(t)
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}
