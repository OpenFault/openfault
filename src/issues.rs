//! Issue queries + actions shared by the UI, REST API and MCP (PLAN §7).
use crate::db::{Db, now};
use crate::process::normalize::classify;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as Sql};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `assignee:me` parses to `Some(ME)`; callers swap in the current user's id. Matches nobody if forgotten.
pub const ME: i64 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    LastSeen,
    FirstSeen,
    Events,
    Users,
}

impl Sort {
    fn col(self) -> &'static str {
        match self {
            Sort::LastSeen => "i.last_seen",
            Sort::FirstSeen => "i.first_seen",
            Sort::Events => "i.event_count",
            Sort::Users => "i.user_count",
        }
    }
    pub fn parse(s: &str) -> Option<Sort> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Default, Clone, Debug)]
pub struct IssueFilter {
    pub project_ids: Vec<i64>,
    pub status: Option<String>,
    pub level: Option<String>,
    pub release: Option<String>,
    pub environment: Option<String>,
    pub assignee_id: Option<i64>,
    pub approved: Option<bool>,
    pub text: Option<String>,
    /// last_seen >= since
    pub since: Option<i64>,
    /// last_seen <= until
    pub until: Option<i64>,
    pub sort: Option<Sort>,
    pub cursor: Option<String>,
    pub limit: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssueRow {
    pub id: i64,
    pub project_id: i64,
    pub project_slug: String,
    pub title: String,
    pub culprit: String,
    pub level: String,
    pub status: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub event_count: i64,
    pub user_count: i64,
    pub last_release: Option<String>,
    pub assignee_id: Option<i64>,
    pub approved_at: Option<i64>,
    pub fix_link: Option<String>,
    /// last 24 hourly counts, oldest first
    pub sparkline: Vec<i64>,
}

/// Tokens: `is:unresolved|resolved|muted level:x release:x env:x assignee:me|<id> approved:yes|no`;
/// everything else (bare or "quoted") is free text.
pub fn parse_query(q: &str, mut f: IssueFilter) -> IssueFilter {
    let mut text = Vec::new();
    let mut toks = Vec::new();
    let (mut cur, mut quoted) = (String::new(), false);
    for ch in q.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => toks.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    toks.push(cur);
    for t in toks.into_iter().filter(|t| !t.is_empty()) {
        let Some((k, v)) = t.split_once(':').filter(|(_, v)| !v.is_empty()) else {
            text.push(t);
            continue;
        };
        let v = v.to_string();
        match k {
            "is" if matches!(v.as_str(), "unresolved" | "resolved" | "muted") => f.status = Some(v),
            "level" => f.level = Some(v),
            "release" => f.release = Some(v),
            "env" => f.environment = Some(v),
            "assignee" if v == "me" => f.assignee_id = Some(ME),
            "assignee" if v.parse::<i64>().is_ok() => f.assignee_id = v.parse().ok(),
            "approved" if v == "yes" => f.approved = Some(true),
            "approved" if v == "no" => f.approved = Some(false),
            _ => text.push(t),
        }
    }
    if !text.is_empty() {
        f.text = Some(text.join(" "));
    }
    f
}

const ROW_COLS: &str =
    "i.id, i.project_id, p.slug, i.title, i.culprit, i.level, i.status, i.first_seen, i.last_seen,
    i.event_count, i.user_count, i.last_release, i.assignee_id, i.approved_at, i.fix_link";

fn issue_row(r: &rusqlite::Row) -> rusqlite::Result<IssueRow> {
    Ok(IssueRow {
        id: r.get(0)?,
        project_id: r.get(1)?,
        project_slug: r.get(2)?,
        title: r.get(3)?,
        culprit: r.get(4)?,
        level: r.get(5)?,
        status: r.get(6)?,
        first_seen: r.get(7)?,
        last_seen: r.get(8)?,
        event_count: r.get(9)?,
        user_count: r.get(10)?,
        last_release: r.get(11)?,
        assignee_id: r.get(12)?,
        approved_at: r.get(13)?,
        fix_link: r.get(14)?,
        sparkline: vec![],
    })
}

/// One query for every row's last-24h buckets (not one per row).
fn fill_sparklines(c: &Connection, rows: &mut [IssueRow]) -> rusqlite::Result<()> {
    let start = now() / 3600 * 3600 - 23 * 3600;
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    for row in rows.iter_mut() {
        row.sparkline = vec![0; 24];
    }
    if ids.is_empty() {
        return Ok(());
    }
    let mut st = c.prepare(&format!(
        "SELECT issue_id, hour_ts, count FROM issue_hourly WHERE issue_id IN ({}) AND hour_ts >= ?1",
        ids_in(&ids)
    ))?;
    for h in st.query_map([start], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (id, ts, n) = h?;
        if let Some(slot) = rows
            .iter_mut()
            .find(|r| r.id == id)
            .and_then(|r| r.sparkline.get_mut(((ts - start) / 3600) as usize))
        {
            *slot = n;
        }
    }
    Ok(())
}

fn encode_cursor(v: i64, id: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("{v}:{id}"))
}
fn decode_cursor(c: &str) -> Option<(i64, i64)> {
    let s = String::from_utf8(URL_SAFE_NO_PAD.decode(c).ok()?).ok()?;
    let (a, b) = s.split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

fn ids_in(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

fn list_sync(c: &Connection, f: &IssueFilter) -> rusqlite::Result<(Vec<IssueRow>, Option<String>)> {
    let sort = f.sort.unwrap_or(Sort::LastSeen);
    let col = sort.col();
    let limit = f.limit.clamp(1, 100) as i64;
    let mut w = vec![format!("i.project_id IN ({})", ids_in(&f.project_ids))];
    let mut p: Vec<Sql> = vec![];
    let mut add = |sql: &str, v: Vec<Sql>| {
        let mut n = p.len();
        w.push(
            sql.split('?')
                .enumerate()
                .map(|(i, part)| {
                    if i == 0 {
                        part.to_string()
                    } else {
                        n += 1;
                        format!("?{n}{part}")
                    }
                })
                .collect(),
        );
        p.extend(v);
    };
    if let Some(s) = &f.status {
        add("i.status = ?", vec![s.clone().into()]);
    }
    if let Some(s) = &f.level {
        add("i.level = ?", vec![s.clone().into()]);
    }
    if let Some(s) = &f.release {
        add(
            "(i.last_release = ? OR i.first_release = ? OR EXISTS (SELECT 1 FROM events e WHERE e.issue_id = i.id AND e.release = ?))",
            vec![s.clone().into(); 3],
        );
    }
    if let Some(s) = &f.environment {
        add(
            "EXISTS (SELECT 1 FROM events e WHERE e.issue_id = i.id AND e.environment = ?)",
            vec![s.clone().into()],
        );
    }
    if let Some(a) = f.assignee_id {
        add("i.assignee_id = ?", vec![a.into()]);
    }
    match f.approved {
        Some(true) => add("i.approved_at IS NOT NULL", vec![]),
        Some(false) => add("i.approved_at IS NULL", vec![]),
        None => {}
    }
    if let Some(t) = &f.text {
        let like = format!(
            "%{}%",
            t.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        add(
            "(i.title LIKE ? ESCAPE '\\' OR i.culprit LIKE ? ESCAPE '\\')",
            vec![like.clone().into(), like.into()],
        );
    }
    if let Some(s) = f.since {
        add("i.last_seen >= ?", vec![s.into()]);
    }
    if let Some(u) = f.until {
        add("i.last_seen <= ?", vec![u.into()]);
    }
    if let Some((v, id)) = f.cursor.as_deref().and_then(decode_cursor) {
        add(
            &format!("({col} < ? OR ({col} = ? AND i.id < ?))"),
            vec![v.into(), v.into(), id.into()],
        );
    }
    let sql = format!(
        "SELECT {ROW_COLS} FROM issues i JOIN projects p ON p.id = i.project_id WHERE {} ORDER BY {col} DESC, i.id DESC LIMIT {}",
        w.join(" AND "),
        limit + 1
    );
    let mut rows: Vec<IssueRow> = c
        .prepare(&sql)?
        .query_map(params_from_iter(p), issue_row)?
        .collect::<rusqlite::Result<_>>()?;
    let next = if rows.len() as i64 > limit {
        rows.truncate(limit as usize);
        rows.last().map(|r| {
            let v = match sort {
                Sort::LastSeen => r.last_seen,
                Sort::FirstSeen => r.first_seen,
                Sort::Events => r.event_count,
                Sort::Users => r.user_count,
            };
            encode_cursor(v, r.id)
        })
    } else {
        None
    };
    fill_sparklines(c, &mut rows)?;
    Ok((rows, next))
}

pub async fn list_issues(
    db: &Db,
    f: &IssueFilter,
) -> anyhow::Result<(Vec<IssueRow>, Option<String>)> {
    anyhow::ensure!(!f.project_ids.is_empty(), "project_ids must not be empty");
    let f = f.clone();
    Ok(db.read(move |c| list_sync(c, &f)).await?)
}

// ---------- detail ----------

#[derive(Clone, Debug, Serialize)]
pub struct Frame {
    pub filename: Option<String>,
    pub abs_path: Option<String>,
    pub function: Option<String>,
    pub module: Option<String>,
    pub lineno: Option<i64>,
    pub colno: Option<i64>,
    pub in_app: bool,
    pub context_line: Option<String>,
    pub pre_context: Vec<String>,
    pub post_context: Vec<String>,
    pub raw: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExceptionView {
    #[serde(rename = "type")]
    pub type_: String,
    pub value: String,
    pub mechanism: Option<String>,
    /// crash frame LAST, as sent; the UI reverses.
    pub frames: Vec<Frame>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventDetail {
    pub id: String,
    pub issue_id: i64,
    pub project_id: i64,
    pub timestamp: i64,
    pub received_at: i64,
    pub release: Option<String>,
    pub environment: Option<String>,
    pub level: Option<String>,
    pub platform: Option<String>,
    pub message: Option<String>,
    pub exceptions: Vec<ExceptionView>,
    pub breadcrumbs: Vec<Value>,
    pub request: Option<Value>,
    pub user: Option<Value>,
    pub tags: Vec<(String, String)>,
    pub contexts: Value,
    pub sdk: Option<String>,
    pub symbolicated: bool,
    pub raw: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct Activity {
    pub id: i64,
    pub user_id: Option<i64>,
    pub user_name: Option<String>,
    pub kind: String,
    pub data: Value,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssueDetail {
    pub row: IssueRow,
    pub fingerprint_hashes: Vec<String>,
    pub resolved_in_release: Option<String>,
    pub resolved_next_release: bool,
    pub mute_until: Option<i64>,
    pub mute_after_count: Option<i64>,
    pub mute_window_min: Option<i64>,
    pub approved_by: Option<i64>,
    pub releases_seen: Vec<String>,
    pub environments: Vec<String>,
    pub activity: Vec<Activity>,
    pub latest_event: Option<EventDetail>,
    pub stored_count: i64,
}

fn js(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}
fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}
/// Sentry sends some lists bare and some as `{"values": [...]}`.
fn values(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a.clone(),
        Some(o) => o
            .get("values")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        None => vec![],
    }
}
fn obj(v: Option<&Value>) -> Option<Value> {
    v.filter(|v| v.is_object()).cloned()
}

fn frame(f: &Value) -> Frame {
    Frame {
        filename: js(f, "filename"),
        abs_path: js(f, "abs_path"),
        function: js(f, "function"),
        module: js(f, "module"),
        lineno: f.get("lineno").and_then(Value::as_i64),
        colno: f.get("colno").and_then(Value::as_i64),
        // of_in_app: set at processing when project rules apply; older events re-derive.
        in_app: f
            .get("of_in_app")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| classify(f).0),
        context_line: js(f, "context_line"),
        pre_context: strs(f.get("pre_context")),
        post_context: strs(f.get("post_context")),
        raw: f.get("raw").cloned(),
    }
}

fn event_detail(
    id: String,
    issue_id: i64,
    project_id: i64,
    received_at: i64,
    ts: Option<i64>,
    payload: &[u8],
) -> EventDetail {
    let ev: Value = zstd::decode_all(payload)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let message = match (ev.get("logentry"), ev.get("message")) {
        (Some(l), _) if l.is_object() => js(l, "formatted").or_else(|| js(l, "message")),
        (_, Some(Value::String(m))) => Some(m.clone()),
        (_, Some(m)) => js(m, "formatted").or_else(|| js(m, "message")),
        _ => None,
    };
    let exceptions = values(ev.get("exception"))
        .iter()
        .map(|e| ExceptionView {
            type_: js(e, "type").unwrap_or_default(),
            value: js(e, "value").unwrap_or_default(),
            mechanism: e.get("mechanism").and_then(|m| js(m, "type")),
            frames: e
                .pointer("/stacktrace/frames")
                .and_then(Value::as_array)
                .map(|fs| fs.iter().map(frame).collect())
                .unwrap_or_default(),
        })
        .collect();
    let tags = match ev.get("tags") {
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_str().map_or_else(|| v.to_string(), String::from),
                )
            })
            .collect(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|t| {
                Some((
                    t.get(0)?.as_str()?.to_string(),
                    t.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect(),
        _ => vec![],
    };
    EventDetail {
        id,
        issue_id,
        project_id,
        timestamp: ts.unwrap_or(received_at),
        received_at,
        release: js(&ev, "release"),
        environment: js(&ev, "environment"),
        level: js(&ev, "level"),
        platform: js(&ev, "platform"),
        message,
        exceptions,
        breadcrumbs: {
            // ponytail: last 100 (the SDK default max); a hostile payload can't balloon API/MCP output.
            let mut b = values(ev.get("breadcrumbs"));
            b.drain(..b.len().saturating_sub(100));
            b
        },
        request: obj(ev.get("request")),
        user: obj(ev.get("user")),
        tags,
        contexts: ev.get("contexts").cloned().unwrap_or(Value::Null),
        sdk: ev.get("sdk").map(|s| {
            format!(
                "{} {}",
                js(s, "name").unwrap_or_default(),
                js(s, "version").unwrap_or_default()
            )
            .trim()
            .to_string()
        }),
        symbolicated: ev
            .get("symbolicated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        raw: ev,
    }
}

const EVENT_COLS: &str = "id, issue_id, project_id, received_at, timestamp, payload";
fn event_row(r: &rusqlite::Row) -> rusqlite::Result<EventDetail> {
    let payload: Vec<u8> = r.get(5)?;
    Ok(event_detail(
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        &payload,
    ))
}

pub async fn get_event(db: &Db, event_id: &str) -> anyhow::Result<Option<EventDetail>> {
    let id = event_id.to_string();
    Ok(db
        .read(move |c| {
            c.query_row(
                &format!("SELECT {EVENT_COLS} FROM events WHERE id = ?1"),
                [id],
                event_row,
            )
            .optional()
        })
        .await?)
}

/// Fix-approval gate for token clients (REST + MCP): frames, breadcrumbs, request, events and raw
/// payloads only once a person approved the issue. Session UI is not gated.
pub async fn fix_approved(db: &Db, issue_id: i64) -> anyhow::Result<bool> {
    Ok(db
        .read(move |c| {
            c.query_row(
                "SELECT approved_at IS NOT NULL FROM issues WHERE id = ?1",
                [issue_id],
                |r| r.get(0),
            )
            .optional()
        })
        .await?
        .unwrap_or(false))
}

/// `user_id` can see the issue's project (owner/admin: all; member: project_access = direct ∪ teams), so may be assigned.
pub async fn assignable(db: &Db, issue_id: i64, user_id: i64) -> anyhow::Result<bool> {
    Ok(db
        .read(move |c| {
            c.query_row(
                "SELECT EXISTS (SELECT 1 FROM users u, issues i WHERE u.id = ?2 AND i.id = ?1 AND (u.role != 'member'
                   OR i.project_id IN (SELECT project_id FROM project_access WHERE user_id = u.id)))",
                params![issue_id, user_id],
                |r| r.get(0),
            )
        })
        .await?)
}

pub async fn get_issue(db: &Db, id: i64) -> anyhow::Result<Option<IssueDetail>> {
    Ok(db
        .read(move |c| {
            let Some(mut row) = c
                .query_row(&format!("SELECT {ROW_COLS} FROM issues i JOIN projects p ON p.id = i.project_id WHERE i.id = ?1"), [id], issue_row)
                .optional()?
            else {
                return Ok(None);
            };
            fill_sparklines(c, std::slice::from_mut(&mut row))?;
            #[allow(clippy::type_complexity)]
            let (rir, rnr, mu, mac, mwm, ab, sc): (Option<String>, bool, Option<i64>, Option<i64>, Option<i64>, Option<i64>, i64) = c.query_row(
                "SELECT resolved_in_release, resolved_next_release, mute_until, mute_after_count, mute_window_min, approved_by, stored_count FROM issues WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )?;
            let col = |sql: &str| -> rusqlite::Result<Vec<String>> { c.prepare(sql)?.query_map([id], |r| r.get(0))?.collect() };
            let fingerprint_hashes = col("SELECT hash FROM issue_hashes WHERE issue_id = ?1 ORDER BY rowid")?;
            let releases_seen = col("SELECT DISTINCT release FROM events WHERE issue_id = ?1 AND release IS NOT NULL ORDER BY release")?;
            let environments = col("SELECT DISTINCT environment FROM events WHERE issue_id = ?1 AND environment IS NOT NULL ORDER BY environment")?;
            let activity = c
                .prepare(
                    "SELECT a.id, a.user_id, u.name, a.kind, a.data_json, a.created_at FROM issue_activity a
                     LEFT JOIN users u ON u.id = a.user_id WHERE a.issue_id = ?1 ORDER BY a.created_at DESC, a.id DESC",
                )?
                .query_map([id], |r| {
                    Ok(Activity {
                        id: r.get(0)?,
                        user_id: r.get(1)?,
                        user_name: r.get(2)?,
                        kind: r.get(3)?,
                        data: r.get::<_, Option<String>>(4)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null),
                        created_at: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            let latest_event = c
                .query_row(
                    &format!("SELECT {EVENT_COLS} FROM events WHERE issue_id = ?1 ORDER BY received_at DESC, id DESC LIMIT 1"),
                    [id],
                    event_row,
                )
                .optional()?;
            Ok(Some(IssueDetail {
                row,
                fingerprint_hashes,
                resolved_in_release: rir,
                resolved_next_release: rnr,
                mute_until: mu,
                mute_after_count: mac,
                mute_window_min: mwm,
                approved_by: ab,
                releases_seen,
                environments,
                activity,
                latest_event,
                stored_count: sc,
            }))
        })
        .await?)
}

#[derive(Clone, Debug, Serialize)]
pub struct EventSummary {
    pub id: String,
    pub timestamp: i64,
    pub release: Option<String>,
    pub environment: Option<String>,
    pub user_label: Option<String>,
}

pub async fn list_events(
    db: &Db,
    issue_id: i64,
    cursor: Option<String>,
    limit: u32,
) -> anyhow::Result<(Vec<EventSummary>, Option<String>)> {
    let limit = limit.clamp(1, 100) as usize;
    let cur = cursor.as_deref().and_then(decode_cursor);
    Ok(db
        .read(move |c| {
            // cursor = (received_at, rowid) of the last event; event ids are uuids
            let (v, rowid) = cur.unwrap_or((i64::MAX, i64::MAX));
            let mut rows = c
                .prepare(
                    "SELECT rowid, id, received_at, timestamp, release, environment, payload FROM events
                     WHERE issue_id = ?1 AND (received_at < ?2 OR (received_at = ?2 AND rowid < ?3))
                     ORDER BY received_at DESC, rowid DESC LIMIT ?4",
                )?
                .query_map(params![issue_id, v, rowid, limit as i64 + 1], |r| {
                    let payload: Vec<u8> = r.get(6)?;
                    let user: Option<Value> = zstd::decode_all(&payload[..])
                        .ok()
                        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                        .and_then(|e| e.get("user").cloned());
                    let label = user.and_then(|u| ["email", "username", "id"].iter().find_map(|k| u.get(*k).map(|v| v.as_str().map_or_else(|| v.to_string(), String::from))));
                    let received: i64 = r.get(2)?;
                    Ok((
                        r.get::<_, i64>(0)?,
                        received,
                        EventSummary {
                            id: r.get(1)?,
                            timestamp: r.get::<_, Option<i64>>(3)?.unwrap_or(received),
                            release: r.get(4)?,
                            environment: r.get(5)?,
                            user_label: label,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let next = (rows.len() > limit).then(|| {
                rows.truncate(limit);
                let (rid, rec, _) = rows.last().unwrap();
                encode_cursor(*rec, *rid)
            });
            Ok((rows.into_iter().map(|r| r.2).collect(), next))
        })
        .await?)
}

/// (older, newer) event ids around `event_id` within the issue.
pub async fn adjacent_events(
    db: &Db,
    issue_id: i64,
    event_id: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let eid = event_id.to_string();
    Ok(db
        .read(move |c| {
            let Some((rec, rid)) = c
                .query_row("SELECT received_at, rowid FROM events WHERE id = ?1 AND issue_id = ?2", params![eid, issue_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })
                .optional()?
            else {
                return Ok((None, None));
            };
            let older = c
                .query_row(
                    "SELECT id FROM events WHERE issue_id = ?1 AND (received_at < ?2 OR (received_at = ?2 AND rowid < ?3))
                     ORDER BY received_at DESC, rowid DESC LIMIT 1",
                    params![issue_id, rec, rid],
                    |r| r.get(0),
                )
                .optional()?;
            let newer = c
                .query_row(
                    "SELECT id FROM events WHERE issue_id = ?1 AND (received_at > ?2 OR (received_at = ?2 AND rowid > ?3))
                     ORDER BY received_at ASC, rowid ASC LIMIT 1",
                    params![issue_id, rec, rid],
                    |r| r.get(0),
                )
                .optional()?;
            Ok((older, newer))
        })
        .await?)
}

// ---------- actions ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum IssueAction {
    Resolve {
        #[serde(default)]
        in_release: Option<String>,
        #[serde(default)]
        next_release: bool,
    },
    Unresolve,
    Mute {
        #[serde(default)]
        until: Option<i64>,
        #[serde(default)]
        after_count: Option<i64>,
        #[serde(default)]
        window_min: Option<i64>,
    },
    Unmute,
    Approve,
    Unapprove,
    Assign {
        #[serde(default)]
        user_id: Option<i64>,
    },
    LinkFix {
        url: String,
        #[serde(default)]
        note: Option<String>,
    },
    Comment {
        text: String,
    },
    Delete,
}

pub async fn apply_action(
    db: &Db,
    issue_id: i64,
    actor_user_id: i64,
    action: IssueAction,
) -> anyhow::Result<()> {
    let found = db
        .write(move |c| {
            let tx = c.transaction()?;
            let Some((pid, title, level)) = tx
                .query_row("SELECT project_id, title, level FROM issues WHERE id = ?1", [issue_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })
                .optional()?
            else {
                return Ok(None);
            };
            let t = now();
            let mut data = serde_json::to_value(&action).unwrap_or(Value::Null);
            let kind = data.as_object_mut().and_then(|o| o.remove("action")).and_then(|k| k.as_str().map(String::from)).unwrap_or_default();
            let up = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<()> {
                let mut all: Vec<&dyn rusqlite::ToSql> = vec![&issue_id];
                all.extend_from_slice(p);
                tx.execute(sql, all.as_slice()).map(|_| ())
            };
            match &action {
                IssueAction::Resolve { in_release, next_release } => up(
                    // next_release with no releases yet = plain resolve; baseline = latest release now.
                    "UPDATE issues SET status = 'resolved', resolved_in_release = ?2,
                       resolved_next_release = ?3 AND EXISTS (SELECT 1 FROM releases WHERE project_id = issues.project_id),
                       resolve_baseline = CASE WHEN ?3 THEN (SELECT version FROM releases WHERE project_id = issues.project_id
                         ORDER BY is_semver DESC, sort_key DESC LIMIT 1) END,
                       mute_until = NULL, mute_after_count = NULL, mute_window_min = NULL WHERE id = ?1",
                    &[in_release, next_release],
                )?,
                IssueAction::Unresolve => up(
                    "UPDATE issues SET status = 'unresolved', resolved_in_release = NULL, resolved_next_release = 0 WHERE id = ?1",
                    &[],
                )?,
                IssueAction::Mute { until, after_count, window_min } => up(
                    "UPDATE issues SET status = 'muted', mute_until = ?2, mute_after_count = ?3, mute_window_min = ?4 WHERE id = ?1",
                    &[until, after_count, window_min],
                )?,
                IssueAction::Unmute => up(
                    "UPDATE issues SET status = 'unresolved', mute_until = NULL, mute_after_count = NULL, mute_window_min = NULL WHERE id = ?1",
                    &[],
                )?,
                IssueAction::Approve => up("UPDATE issues SET approved_by = ?2, approved_at = ?3 WHERE id = ?1", &[&actor_user_id, &t])?,
                IssueAction::Unapprove => up("UPDATE issues SET approved_by = NULL, approved_at = NULL WHERE id = ?1", &[])?,
                IssueAction::Assign { user_id } => up("UPDATE issues SET assignee_id = ?2 WHERE id = ?1", &[user_id])?,
                IssueAction::LinkFix { url, .. } => up("UPDATE issues SET fix_link = ?2 WHERE id = ?1", &[url])?,
                IssueAction::Comment { .. } => {}
                IssueAction::Delete => {
                    // ponytail: no activity row for Delete; it would be deleted with the issue.
                    for t in ["events", "issue_hashes", "issue_hourly", "issue_users", "issue_activity"] {
                        tx.execute(&format!("DELETE FROM {t} WHERE issue_id = ?1"), [issue_id])?;
                    }
                    tx.execute("DELETE FROM issues WHERE id = ?1", [issue_id])?;
                    tx.commit()?;
                    return Ok(Some(None));
                }
            }
            tx.execute(
                "INSERT INTO issue_activity (issue_id, user_id, kind, data_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![issue_id, actor_user_id, kind, data.to_string(), t],
            )?;
            tx.commit()?;
            let live = match &action {
                IssueAction::Resolve { .. } => "resolved",
                IssueAction::Mute { .. } => "muted",
                IssueAction::Unresolve | IssueAction::Unmute => "unresolved",
                IssueAction::Approve => "approved",
                _ => return Ok(Some(None)),
            };
            Ok(Some(Some(crate::live::LiveEvent::new(pid, issue_id, &title, &level, live, t))))
        })
        .await?;
    let Some(live) = found else {
        anyhow::bail!("issue not found");
    };
    if let Some(e) = live {
        let _ = db.live.send(e);
    }
    Ok(())
}

// ---------- releases ----------

#[derive(Clone, Debug, Serialize)]
pub struct ReleaseRow {
    pub version: String,
    pub first_seen: i64,
    pub is_semver: bool,
    pub commit_sha: Option<String>,
    pub url: Option<String>,
    pub new_issues: i64,
    pub resolved_issues: i64,
    /// stored events only (store-by-exception samples above 25/hour).
    pub event_count: i64,
    /// semver or first-seen order key (see process::upsert_release)
    #[serde(skip)]
    pub sort_key: String,
}

pub async fn list_releases(
    db: &Db,
    project_id: i64,
    limit: u32,
) -> anyhow::Result<Vec<ReleaseRow>> {
    Ok(db
        .read(move |c| {
            c.prepare(
                "SELECT r.version, r.first_seen, r.is_semver, r.commit_sha, r.url,
                   (SELECT count(*) FROM issues WHERE project_id = r.project_id AND first_release = r.version),
                   (SELECT count(*) FROM issues WHERE project_id = r.project_id AND status = 'resolved' AND resolved_in_release = r.version),
                   (SELECT count(*) FROM events WHERE project_id = r.project_id AND release = r.version),
                   r.sort_key
                 FROM releases r WHERE r.project_id = ?1 ORDER BY r.is_semver DESC, r.sort_key DESC LIMIT ?2",
            )?
            .query_map(params![project_id, limit.clamp(1, 100)], |r| {
                Ok(ReleaseRow {
                    version: r.get(0)?,
                    first_seen: r.get(1)?,
                    is_semver: r.get(2)?,
                    commit_sha: r.get(3)?,
                    url: r.get(4)?,
                    new_issues: r.get(5)?,
                    resolved_issues: r.get(6)?,
                    event_count: r.get(7)?,
                    sort_key: r.get(8)?,
                })
            })?
            .collect()
        })
        .await?)
}

// ---------- stats ----------

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Range {
    Day,
    Week,
    Month,
}

impl Range {
    /// (first bucket ts, bucket seconds, bucket count); buckets are UTC hours / days ending now.
    pub fn buckets(self, t: i64) -> (i64, i64, i64) {
        let (step, n) = match self {
            Range::Day => (3600, 24),
            Range::Week => (86400, 7),
            Range::Month => (86400, 30),
        };
        (t / step * step - (n - 1) * step, step, n)
    }
    pub fn parse(s: &str) -> Option<Range> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProjectSeries {
    pub project_id: i64,
    pub slug: String,
    pub name: String,
    /// (bucket ts, received, stored)
    pub buckets: Vec<(i64, i64, i64)>,
    pub unresolved: i64,
    pub new_issues: i64,
    pub regressions: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Stats {
    pub per_project: Vec<ProjectSeries>,
    pub top_issues: Vec<IssueRow>,
    pub total_received: i64,
    pub total_dropped: i64,
}

pub async fn stats(db: &Db, project_ids: &[i64], range: Range) -> anyhow::Result<Stats> {
    let ids = project_ids.to_vec();
    let (start, step, n) = range.buckets(now());
    let top = if ids.is_empty() {
        vec![]
    } else {
        let f = IssueFilter {
            project_ids: ids.clone(),
            since: Some(start),
            sort: Some(Sort::Users),
            limit: 10,
            ..Default::default()
        };
        list_issues(db, &f).await?.0
    };
    let (per_project, total_received, total_dropped) = db
        .read(move |c| {
            let (mut tr, mut td) = (0, 0);
            let mut out = vec![];
            for pid in ids {
                let Some((slug, name)) =
                    c.query_row("SELECT slug, name FROM projects WHERE id = ?1", [pid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
                else {
                    continue;
                };
                let mut buckets: Vec<(i64, i64, i64)> = (0..n).map(|i| (start + i * step, 0, 0)).collect();
                let mut st = c.prepare_cached("SELECT hour_ts, received, stored, dropped FROM project_hourly WHERE project_id = ?1 AND hour_ts >= ?2")?;
                for r in st.query_map(params![pid, start], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))? {
                    let (h, rec, sto, dro) = r?;
                    if let Some(b) = buckets.get_mut(((h - start) / step) as usize) {
                        b.1 += rec;
                        b.2 += sto;
                        tr += rec;
                        td += dro;
                    }
                }
                let q = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, params![pid, start], |r| r.get(0)) };
                let unresolved: i64 = c.query_row("SELECT count(*) FROM issues WHERE project_id = ?1 AND status = 'unresolved'", [pid], |r| r.get(0))?;
                out.push(ProjectSeries {
                    project_id: pid,
                    slug,
                    name,
                    buckets,
                    unresolved,
                    new_issues: q("SELECT count(*) FROM issues WHERE project_id = ?1 AND first_seen >= ?2")?,
                    regressions: q(
                        "SELECT count(*) FROM issue_activity a JOIN issues i ON i.id = a.issue_id
                         WHERE i.project_id = ?1 AND a.kind = 'regressed' AND a.created_at >= ?2",
                    )?,
                });
            }
            Ok((out, tr, td))
        })
        .await?;
    Ok(Stats {
        per_project,
        top_issues: top,
        total_received,
        total_dropped,
    })
}

/// Event-data text for API/MCP output: control chars (except \n \t) stripped, capped at 2000 chars,
/// ``` fences broken with zero-width spaces so data can't close our fenced block.
pub fn untrusted(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(2000)
        .collect();
    if s.chars().count() > 2000 {
        out.push('…');
    }
    while out.contains("```") {
        out = out.replace("```", "`\u{200b}`\u{200b}`");
    }
    out
}

/// `untrusted` applied to every string inside a JSON value.
pub fn untrusted_json(v: &mut Value) {
    match v {
        Value::String(s) => *s = untrusted(s),
        Value::Array(a) => a.iter_mut().for_each(untrusted_json),
        Value::Object(o) => o.values_mut().for_each(untrusted_json),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_tokens() {
        let f = parse_query(
            r#"is:resolved level:error "cannot read" foo env:prod assignee:me approved:no release:1.2 x:y"#,
            IssueFilter::default(),
        );
        assert_eq!(f.status.as_deref(), Some("resolved"));
        assert_eq!(f.level.as_deref(), Some("error"));
        assert_eq!(f.environment.as_deref(), Some("prod"));
        assert_eq!(f.release.as_deref(), Some("1.2"));
        assert_eq!(f.assignee_id, Some(ME));
        assert_eq!(f.approved, Some(false));
        assert_eq!(f.text.as_deref(), Some("cannot read foo x:y"));
        let f = parse_query("is:bogus assignee:7", IssueFilter::default());
        assert_eq!(
            (f.status, f.assignee_id, f.text.as_deref()),
            (None, Some(7), Some("is:bogus"))
        );
    }

    #[test]
    fn breadcrumbs_capped_to_last_100() {
        let crumbs: Vec<Value> = (0..150)
            .map(|i| json!({"message": i.to_string()}))
            .collect();
        let raw = serde_json::to_vec(&json!({"breadcrumbs": {"values": crumbs}})).unwrap();
        let e = event_detail(
            "e".into(),
            1,
            1,
            0,
            None,
            &zstd::encode_all(&raw[..], 1).unwrap(),
        );
        assert_eq!(e.breadcrumbs.len(), 100);
        assert_eq!(e.breadcrumbs[0]["message"], "50");
    }

    #[test]
    fn untrusted_strips_and_fences() {
        let s = untrusted("a\u{1b}[31mb\u{0}c\n```rm -rf```\t");
        assert_eq!(s.matches("```").count(), 0);
        assert!(s.starts_with("a[31mbc\n`"));
        assert!(s.ends_with('\t'));
        assert_eq!(untrusted(&"x".repeat(5000)).chars().count(), 2001);
        let mut v = json!({"a": ["```"]});
        untrusted_json(&mut v);
        assert!(!v.to_string().contains("```"));
    }
}
