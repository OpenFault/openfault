//! Inbox → issues/events. One loop, batches of ≤100 in one write transaction.
pub mod group;
pub mod normalize;

use crate::db::{Db, now};
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
                Ok(n) if n == BATCH as usize => continue,
                Ok(_) => break,
                Err(e) => {
                    // ponytail: a poison row rolls the batch back and retries forever; dead-letter it if this ever logs twice.
                    tracing::error!("processor batch failed: {e}");
                    break;
                }
            }
        }
    }
}

pub fn process_batch(c: &mut Connection) -> rusqlite::Result<usize> {
    let tx = c.transaction()?;
    let rows: Vec<(i64, i64, i64, Vec<u8>)> = tx
        .prepare("SELECT id, project_id, received_at, body FROM inbox ORDER BY id LIMIT ?1")?
        .query_map([BATCH], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let Some(max_id) = rows.last().map(|r| r.0) else {
        return Ok(0);
    };
    for (_, pid, received, body) in &rows {
        let ev = zstd::decode_all(&body[..])
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        match ev {
            Some(ev) => process_one(&tx, *pid, *received, &ev, body)?,
            None => tracing::warn!(project_id = pid, "dropping undecodable inbox row"),
        }
    }
    tx.execute("DELETE FROM inbox WHERE id <= ?1", [max_id])?;
    tx.commit()?;
    Ok(rows.len())
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
    pid: i64,
    received: i64,
    ev: &Value,
    payload: &[u8],
) -> rusqlite::Result<()> {
    let g = group::group(ev);
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
    let (issue_id, prev_release) = match found {
        Some(id) => {
            let (status, resolved_in, last_rel): (String, Option<String>, Option<String>) = tx
                .query_row(
                    "SELECT status, resolved_in_release, last_release FROM issues WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
            let regressed = status == "resolved"
                && match (resolved_in.as_deref(), release) {
                    (Some(fixed), Some(r)) => !release_before(tx, pid, r, fixed)?,
                    _ => true,
                };
            if regressed {
                tx.execute(
                    "UPDATE issues SET status = 'unresolved' WHERE id = ?1",
                    [id],
                )?;
                tx.execute(
                    "INSERT INTO issue_activity (issue_id, kind, data_json, created_at) VALUES (?1, 'regressed', ?2, ?3)",
                    params![id, serde_json::json!({"release": release}).to_string(), received],
                )?;
            }
            (id, last_rel)
        }
        None => {
            tx.execute(
                "INSERT INTO issues (project_id, fingerprint, title, culprit, level, platform, first_seen, last_seen, first_release, last_release)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?8)",
                params![pid, g.hashes[0], g.title, g.culprit, level, s(ev, "platform"), received, release],
            )?;
            (tx.last_insert_rowid(), None)
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
        params![issue_id, store as i64, new_user as i64, received, level, release],
    )?;
    tx.execute(
        "INSERT INTO project_hourly (project_id, hour_ts, received, stored, dropped) VALUES (?1, ?2, 1, ?3, ?4)
         ON CONFLICT DO UPDATE SET received = received + 1, stored = stored + excluded.stored, dropped = dropped + excluded.dropped",
        params![pid, hour, store as i64, (!store) as i64],
    )?;
    Ok(())
}

/// Hourly: retention deletes in small batches, WAL checkpoint, incremental vacuum.
pub async fn housekeeping(db: Db) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
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
    fn semver_parse() {
        assert_eq!(super::semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(super::semver("app@2.0"), Some((2, 0, 0)));
        assert_eq!(super::semver("1.4.0-rc.1"), Some((1, 4, 0)));
        assert_eq!(super::semver("abc123"), None);
    }
}
