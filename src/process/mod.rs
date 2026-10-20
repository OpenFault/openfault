//! Inbox → issues/events. One loop, batches of ≤100 in one write transaction.
pub mod group;
pub mod normalize;
pub mod symbolicate;

use crate::db::{Db, now};
use crate::live::LiveEvent;
use crate::notify;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

const BATCH: i64 = 100;
const USERS_CAP: i64 = 10_000;

pub async fn run(db: Db, notify: Arc<Notify>) {
    loop {
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
        loop {
            match db.write(process_batch).await {
                Ok((n, live)) => {
                    // after commit; Err only means nobody is listening.
                    for e in live {
                        let _ = db.live.send(e);
                    }
                    if n < BATCH as usize {
                        break;
                    }
                }
                Err(e) => {
                    // ponytail: a poison row rolls the batch back and retries forever; dead-letter it if this ever logs twice.
                    tracing::error!("processor batch failed: {e}");
                    break;
                }
            }
        }
    }
}

pub fn process_batch(c: &mut Connection) -> rusqlite::Result<(usize, Vec<LiveEvent>)> {
    let tx = c.transaction()?;
    let rows: Vec<(i64, i64, i64, Vec<u8>)> = tx
        .prepare("SELECT id, project_id, received_at, body FROM inbox ORDER BY id LIMIT ?1")?
        .query_map([BATCH], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let Some(max_id) = rows.last().map(|r| r.0) else {
        return Ok((0, vec![]));
    };
    let mut live = Vec::with_capacity(rows.len());
    // project in-app rules, read once per project per batch.
    let mut rules: std::collections::HashMap<i64, Vec<normalize::Rule>> = Default::default();
    for (_, pid, received, body) in &rows {
        if !rules.contains_key(pid) {
            let text: String = tx
                .query_row(
                    "SELECT coalesce(json_extract(settings_json, '$.inapp_rules'), '') FROM projects WHERE id = ?1",
                    [pid],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_default();
            rules.insert(*pid, normalize::parse_rules(&text));
        }
        let ev = zstd::decode_all(&body[..])
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        match ev {
            // duplicate delivery/retry: no counters, no tick; the inbox row is still deleted below.
            Some(ev) if is_dup(&tx, &ev, *received)? => {
                tracing::info!(
                    project_id = pid,
                    event_id = s(&ev, "event_id"),
                    "duplicate event skipped"
                )
            }
            Some(mut ev) => {
                // before grouping so hashes use resolved names; re-encode only if frames changed.
                symbolicate::symbolicate(&tx, *pid, &mut ev);
                let ruled = stamp_in_app(&mut ev, &rules[pid]);
                // re-encode failure keeps the original payload rather than storing an empty one.
                let resolved = (ruled || ev.get("symbolicated").is_some())
                    .then(|| serde_json::to_vec(&ev).ok())
                    .flatten()
                    .and_then(|j| zstd::encode_all(&j[..], 3).ok());
                live.push(process_one(
                    &tx,
                    &rules[pid],
                    *pid,
                    *received,
                    &ev,
                    resolved.as_deref().unwrap_or(body),
                )?)
            }
            None => tracing::warn!(project_id = pid, "dropping undecodable inbox row"),
        }
    }
    tx.execute("DELETE FROM inbox WHERE id <= ?1", [max_id])?;
    tx.commit()?;
    Ok((rows.len(), live))
}

/// Records the event_id in `seen_events` (so sampled-out events dedupe too); true if it was already seen.
fn is_dup(tx: &Transaction, ev: &Value, received: i64) -> rusqlite::Result<bool> {
    let Some(id) = s(ev, "event_id") else {
        return Ok(false);
    };
    // `events` check covers rows stored before seen_events existed.
    Ok(tx.execute(
        "INSERT OR IGNORE INTO seen_events (id, received_at) SELECT ?1, ?2 WHERE NOT EXISTS (SELECT 1 FROM events WHERE id = ?1)",
        params![id, received],
    )? == 0)
}

/// With project rules, stores the final in-app flag on each frame as `of_in_app` so the UI shows what grouping used.
fn stamp_in_app(ev: &mut Value, rules: &[normalize::Rule]) -> bool {
    if rules.is_empty() {
        return false;
    }
    let excs = match ev.get_mut("exception") {
        Some(Value::Array(a)) => a,
        Some(o) => match o.get_mut("values").and_then(Value::as_array_mut) {
            Some(a) => a,
            None => return false,
        },
        None => return false,
    };
    for f in excs
        .iter_mut()
        .filter_map(|e| {
            e.pointer_mut("/stacktrace/frames")
                .and_then(Value::as_array_mut)
        })
        .flatten()
    {
        f["of_in_app"] = normalize::classify_with(f, rules).0.into();
    }
    true
}

fn s<'a>(ev: &'a Value, k: &str) -> Option<&'a str> {
    ev.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn semver(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.rsplit('@').next()?.trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let t = (
        it.next()??,
        it.next().unwrap_or(Some(0))?,
        it.next().unwrap_or(Some(0))?,
    );
    it.next().is_none().then_some(t)
}

fn upsert_release(tx: &Transaction, pid: i64, version: &str, t: i64) -> rusqlite::Result<()> {
    let (is_semver, key) = match semver(version) {
        Some((a, b, c)) => (1, format!("{a:010}.{b:010}.{c:010}")),
        None => (0, format!("{t:020}")),
    };
    tx.execute(
        "INSERT OR IGNORE INTO releases (project_id, version, is_semver, sort_key, first_seen) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![pid, version, is_semver, key, t],
    )?;
    Ok(())
}

/// `a` sorts before `b`: semver if both parse, else first-seen order (PLAN §5).
fn release_before(tx: &Transaction, pid: i64, a: &str, b: &str) -> rusqlite::Result<bool> {
    let get = |v: &str| {
        tx.query_row(
            "SELECT is_semver, sort_key, first_seen FROM releases WHERE project_id = ?1 AND version = ?2",
            params![pid, v],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)),
        )
        .optional()
    };
    Ok(match (get(a)?, get(b)?) {
        (Some((1, ka, _)), Some((1, kb, _))) => ka < kb,
        (Some((_, _, fa)), Some((_, _, fb))) => fa < fb,
        _ => false,
    })
}

fn timestamp(ev: &Value) -> Option<i64> {
    match ev.get("timestamp")? {
        Value::Number(n) => n.as_f64().map(|f| f as i64),
        Value::String(s) => s.parse::<jiff::Timestamp>().ok().map(|t| t.as_second()),
        _ => None,
    }
}

fn process_one(
    tx: &Transaction,
    rules: &[normalize::Rule],
    pid: i64,
    received: i64,
    ev: &Value,
    payload: &[u8],
) -> rusqlite::Result<LiveEvent> {
    let g = group::group_with(ev, rules);
    let release = s(ev, "release");
    let level = s(ev, "level").unwrap_or("error");
    let user = ev.get("user");
    let user_hash = ["id", "email", "ip_address", "username"]
        .iter()
        .find_map(|k| {
            user.and_then(|u| u.get(*k))
                .filter(|v| !v.is_null())
                .map(|v| v.to_string())
        })
        .map(|u| blake3::hash(u.as_bytes()).to_hex()[..32].to_string());
    if let Some(r) = release {
        upsert_release(tx, pid, r, received)?;
    }

    let mut found = None;
    for hash in &g.hashes {
        found = tx
            .query_row(
                "SELECT issue_id FROM issue_hashes WHERE project_id = ?1 AND hash = ?2",
                params![pid, hash],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        if found.is_some() {
            break;
        }
    }
    // SDK clock if plausible, else receive time (issues and events then agree).
    let seen = timestamp(ev)
        .filter(|t| (received - 7 * 86400..=received + 300).contains(t))
        .unwrap_or(received);
    let mut kind = "event";
    let (issue_id, prev_release, title) = match found {
        Some(id) => {
            #[allow(clippy::type_complexity)]
            let (status, resolved_in, last_rel, mute_until, after, window, title): (
                String,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                String,
            ) = tx.query_row(
                "SELECT status, resolved_in_release, last_release, mute_until, mute_after_count, mute_window_min, title FROM issues WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )?;
            if status == "muted" {
                // ponytail: volume counts stored events (+ this one); above 25/hour sampling undercounts, use issue_hourly if that matters.
                let unmute = if mute_until.is_some_and(|u| received >= u) {
                    Some("unmuted_expired")
                } else if let (Some(n), Some(w)) = (after, window) {
                    let seen: i64 = tx.query_row(
                        "SELECT count(*) FROM events WHERE issue_id = ?1 AND received_at >= ?2
                           AND received_at > coalesce((SELECT max(created_at) FROM issue_activity WHERE issue_id = ?1 AND kind = 'mute'), 0)",
                        params![id, received - w * 60],
                        |r| r.get(0),
                    )?;
                    (seen + 1 >= n).then_some("unmuted_volume")
                } else {
                    None
                };
                if let Some(k) = unmute {
                    kind = "unresolved";
                    tx.execute(
                        "UPDATE issues SET status = 'unresolved', mute_until = NULL, mute_after_count = NULL, mute_window_min = NULL WHERE id = ?1",
                        [id],
                    )?;
                    tx.execute(
                        "INSERT INTO issue_activity (issue_id, kind, data_json, created_at) VALUES (?1, ?2, '{}', ?3)",
                        params![id, k, received],
                    )?;
                }
            }
            let regressed = status == "resolved"
                && match (resolved_in.as_deref(), release) {
                    (Some(fixed), Some(r)) => !release_before(tx, pid, r, fixed)?,
                    _ => true,
                };
            if regressed {
                kind = "regression";
                tx.execute(
                    "UPDATE issues SET status = 'unresolved' WHERE id = ?1",
                    [id],
                )?;
                tx.execute(
                    "INSERT INTO issue_activity (issue_id, kind, data_json, created_at) VALUES (?1, 'regressed', ?2, ?3)",
                    params![id, serde_json::json!({"release": release}).to_string(), received],
                )?;
            }
            (id, last_rel, title)
        }
        None => {
            kind = "new_issue";
            tx.execute(
                "INSERT INTO issues (project_id, fingerprint, title, culprit, level, platform, first_seen, last_seen, first_release, last_release)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?8)",
                params![pid, g.hashes[0], g.title, g.culprit, level, s(ev, "platform"), seen, release],
            )?;
            (tx.last_insert_rowid(), None, g.title.clone())
        }
    };
    for hash in &g.hashes {
        tx.execute(
            "INSERT OR IGNORE INTO issue_hashes (project_id, hash, issue_id) VALUES (?1, ?2, ?3)",
            params![pid, hash, issue_id],
        )?;
    }

    let mut new_user = false;
    if let Some(uh) = &user_hash {
        let updated = tx.execute(
            "UPDATE issue_users SET last_seen = ?3 WHERE issue_id = ?1 AND user_hash = ?2",
            params![issue_id, uh, received],
        )?;
        if updated == 0 {
            new_user = tx.execute(
                "INSERT INTO issue_users (issue_id, user_hash, last_seen)
                 SELECT ?1, ?2, ?3 WHERE (SELECT count(*) FROM issue_users WHERE issue_id = ?1) < ?4",
                params![issue_id, uh, received, USERS_CAP],
            )? == 1;
        }
    }

    let hour = received - received % 3600;
    let hour_count: i64 = tx.query_row(
        "INSERT INTO issue_hourly (issue_id, hour_ts, count, user_count_est) VALUES (?1, ?2, 1, ?3)
         ON CONFLICT DO UPDATE SET count = count + 1, user_count_est = user_count_est + excluded.user_count_est RETURNING count",
        params![issue_id, hour, new_user as i64],
        |r| r.get(0),
    )?;
    // store by exception: first 25/hour, then 1 in 20, always on an unseen release or user.
    // ponytail: "unseen environment" not tracked; add an issue_envs table if environments start mattering.
    let store = hour_count <= 25
        || hour_count % 20 == 0
        || new_user
        || (release.is_some() && release != prev_release.as_deref());
    if store {
        let event_id = s(ev, "event_id").unwrap_or_default();
        tx.execute(
            "INSERT OR IGNORE INTO events (id, issue_id, project_id, received_at, timestamp, release, environment, user_hash, sampled, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![event_id, issue_id, pid, received, timestamp(ev), release, s(ev, "environment"), user_hash, (hour_count > 25) as i64, payload],
        )?;
    }
    tx.execute(
        "UPDATE issues SET event_count = event_count + 1, stored_count = stored_count + ?2, user_count = user_count + ?3,
           last_seen = max(last_seen, ?4), first_seen = min(first_seen, ?4), level = ?5,
           last_release = coalesce(?6, last_release), first_release = coalesce(first_release, ?6)
         WHERE id = ?1",
        params![issue_id, store as i64, new_user as i64, seen, level, release],
    )?;
    tx.execute(
        "INSERT INTO project_5min (project_id, bucket_ts, count) VALUES (?1, ?2, 1)
         ON CONFLICT DO UPDATE SET count = count + 1",
        params![pid, received - received % 300],
    )?;
    tx.execute(
        "INSERT INTO project_hourly (project_id, hour_ts, received, stored, dropped) VALUES (?1, ?2, 1, ?3, ?4)
         ON CONFLICT DO UPDATE SET received = received + 1, stored = stored + excluded.stored, dropped = dropped + excluded.dropped",
        params![pid, hour, store as i64, (!store) as i64],
    )?;
    // same decision points as the live tick above; delivery happens in notify::run.
    let notif = match kind {
        "new_issue" | "regression" => Some(kind),
        "unresolved" => Some("unmuted"),
        _ => None,
    };
    if let Some(k) = notif {
        notify::enqueue(
            tx,
            &notify::Notification::new(pid, issue_id, k, &title, level),
        )?;
    }
    // ponytail: checked every 10th event of the hour (20, 30, …) to keep the median query off the hot path.
    if hour_count % 10 == 0 && notify::check_spike(tx, issue_id, hour, hour_count, received)? {
        notify::enqueue(
            tx,
            &notify::Notification::new(pid, issue_id, "spike", &title, level),
        )?;
    }
    Ok(LiveEvent::new(pid, issue_id, &title, level, kind, received))
}

/// Expired auth rows and abandoned upload chunks.
pub fn purge(c: &Connection, t: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM sessions WHERE expires_at < ?1", [t])?;
    // ponytail: rows are deleted on use, so "used" resets are already gone.
    c.execute("DELETE FROM password_resets WHERE expires_at < ?1", [t])?;
    c.execute("DELETE FROM invites WHERE expires_at < ?1", [t - 7 * 86400])?;
    c.execute("DELETE FROM chunks WHERE created_at < ?1", [t - 3600])?;
    c.execute("DELETE FROM project_5min WHERE bucket_ts < ?1", [t - 86400])?;
    // ponytail: 7 days covers SDK retries; a later redelivery counts again.
    c.execute(
        "DELETE FROM seen_events WHERE received_at < ?1",
        [t - 7 * 86400],
    )?;
    Ok(())
}

/// Muted issues whose `mute_until` passed, even with no new events. The on-event path in process_one stays.
pub fn expire_mutes(c: &Connection, t: i64) -> rusqlite::Result<usize> {
    // one transaction: a crash can't leave rows enqueued but still muted (or the reverse).
    let c = c.unchecked_transaction()?;
    let expired: Vec<(i64, i64, String, String)> = c
        .prepare("SELECT id, project_id, title, level FROM issues WHERE status = 'muted' AND mute_until <= ?1")?
        .query_map([t], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, pid, title, level) in &expired {
        notify::enqueue(
            &c,
            &notify::Notification::new(*pid, *id, "unmuted", title, level),
        )?;
    }
    c.execute(
        "INSERT INTO issue_activity (issue_id, kind, data_json, created_at)
         SELECT id, 'unmuted_expired', '{}', ?1 FROM issues WHERE status = 'muted' AND mute_until <= ?1",
        [t],
    )?;
    let n = c.execute(
        "UPDATE issues SET status = 'unresolved', mute_until = NULL, mute_after_count = NULL, mute_window_min = NULL
         WHERE status = 'muted' AND mute_until <= ?1",
        [t],
    )?;
    c.commit()?;
    Ok(n)
}

/// Every 60s: expire mutes. Hourly: retention deletes in small batches, WAL checkpoint, incremental vacuum.
pub async fn housekeeping(db: Db) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut n = 0u64;
    loop {
        tick.tick().await;
        n = n.wrapping_add(1);
        if let Err(e) = db.write(|c| expire_mutes(c, now())).await {
            tracing::error!("mute expiry failed: {e}");
        }
        if n % 60 != 1 {
            continue;
        }
        let r = db
            .write(|c| {
                let projects: Vec<(i64, i64)> = c
                    .prepare("SELECT id, retention_days FROM projects")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                let mut deleted = 0;
                for (pid, days) in projects {
                    let cutoff = now() - days * 86400;
                    loop {
                        let n = c.execute(
                            "DELETE FROM events WHERE rowid IN (SELECT rowid FROM events WHERE project_id = ?1 AND received_at < ?2 LIMIT 500)",
                            params![pid, cutoff],
                        )?;
                        deleted += n;
                        if n < 500 {
                            break;
                        }
                    }
                }
                purge(c, now())?;
                c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
                c.execute_batch("PRAGMA incremental_vacuum;")?;
                Ok(deleted)
            })
            .await;
        match r {
            Ok(n) => tracing::info!(deleted = n, "housekeeping done"),
            Err(e) => tracing::error!("housekeeping failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn expire_mutes_without_events() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch(
            "INSERT INTO orgs (id, name, slug, created_at) VALUES (1, 'o', 'o', 0);
             INSERT INTO projects (id, org_id, slug, name, created_at) VALUES (1, 1, 'p', 'p', 0);
             INSERT INTO issues (id, project_id, fingerprint, title, status, mute_until, first_seen, last_seen)
             VALUES (1, 1, 'a', 't', 'muted', 99, 0, 0), (2, 1, 'b', 't', 'muted', 101, 0, 0);",
        )
        .unwrap();
        assert_eq!(super::expire_mutes(&c, 100).unwrap(), 1);
        let (st, kind): (String, String) = c
            .query_row("SELECT status, (SELECT group_concat(kind) FROM issue_activity) FROM issues WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(
            (st.as_str(), kind.as_str()),
            ("unresolved", "unmuted_expired")
        );
    }

    #[test]
    fn purge_drops_expired_sessions() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch(
            "INSERT INTO users (id, email, pw_hash, role, created_at) VALUES (1, 'a@b.c', 'x', 'admin', 0);
             INSERT INTO sessions (id_hash, user_id, expires_at) VALUES ('old', 1, 99), ('new', 1, 101);",
        )
        .unwrap();
        super::purge(&c, 100).unwrap();
        let left: String = c
            .query_row("SELECT group_concat(id_hash) FROM sessions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, "new");
    }

    #[test]
    fn semver_parse() {
        assert_eq!(super::semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(super::semver("app@2.0"), Some((2, 0, 0)));
        assert_eq!(super::semver("1.4.0-rc.1"), Some((1, 4, 0)));
        assert_eq!(super::semver("abc123"), None);
    }
}
