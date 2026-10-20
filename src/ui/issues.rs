//! Issue list, detail, actions, saved views, Markdown export (PLAN §8).
use super::{
    E, Me, R, abs, chart, enc, flash, forbidden, initials, n, not_found, project_for, render,
};
use crate::{
    AppState,
    auth::CurrentUser,
    db::now,
    issues::{self as svc, EventDetail, IssueAction, IssueDetail, IssueFilter, IssueRow, Sort},
};
use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/issues", get(list_all))
        .route("/projects/{slug}/issues", get(list_project))
        .route("/issues/bulk", post(bulk))
        .route("/issues/{id}", get(detail))
        .route("/issues/{id}/events/{eid}", get(event))
        .route("/issues/{id}/action", post(action))
        .route("/issues/{id}/delete", get(delete_page).post(delete))
        .route("/views", post(save_view))
        .route("/views/{id}/delete", post(delete_view))
}

// ---------- list ----------

#[derive(Deserialize, Default)]
pub struct ListQ {
    q: Option<String>,
    sort: Option<String>,
    cursor: Option<String>,
}

fn sort_str(s: Sort) -> &'static str {
    match s {
        Sort::LastSeen => "last_seen",
        Sort::FirstSeen => "first_seen",
        Sort::Events => "events",
        Sort::Users => "users",
    }
}

/// `q` with every `key:` token removed and `key:val` appended (when given).
fn with_token(q: &str, key: &str, val: Option<&str>) -> String {
    let pre = format!("{key}:");
    let mut t: Vec<String> = q
        .split_whitespace()
        .filter(|t| !t.starts_with(&pre))
        .map(String::from)
        .collect();
    if let Some(v) = val {
        t.push(if v.contains(' ') {
            format!("{pre}\"{v}\"")
        } else {
            format!("{pre}{v}")
        });
    }
    t.join(" ")
}

pub struct Row {
    pub r: IssueRow,
    pub bars: chart::Chart,
    pub who: String,
}

pub struct Chip {
    pub label: &'static str,
    pub current: Option<String>,
    pub opts: Vec<(String, String)>,
    pub clear: String,
}

#[derive(Template)]
#[template(path = "issues.html")]
struct List {
    me: Me,
    base: String,
    q: String,
    sort: &'static str,
    tabs: Vec<(&'static str, String, bool)>,
    chips: Vec<Chip>,
    rows: Vec<Row>,
    more: Option<String>,
    next: String,
    filtered: bool,
    status: String,
}

pub struct Member {
    pub id: i64,
    pub name: String,
    pub initials: String,
}

async fn members(st: &AppState) -> Result<Vec<Member>, E> {
    Ok(st
        .db
        .read(|c| {
            c.prepare("SELECT id, name, email FROM users ORDER BY name, email")?
                .query_map([], |r| {
                    let (name, email): (String, String) = (r.get(1)?, r.get(2)?);
                    Ok(Member {
                        id: r.get(0)?,
                        initials: initials(&name, &email),
                        name: if name.is_empty() { email } else { name },
                    })
                })?
                .collect()
        })
        .await?)
}

async fn list_all(me: Me, st: State<AppState>, h: HeaderMap, Query(lq): Query<ListQ>) -> R {
    list(me, st, h, lq).await
}

async fn list_project(
    mut me: Me,
    st: State<AppState>,
    h: HeaderMap,
    Path(slug): Path<String>,
    Query(lq): Query<ListQ>,
) -> R {
    me.scope = Some(project_for(&st, &me, slug).await?);
    list(me, st, h, lq).await
}

async fn list(me: Me, State(st): State<AppState>, h: HeaderMap, lq: ListQ) -> R {
    let base = me.base("issues");
    let q = lq.q.unwrap_or_else(|| "is:unresolved".into());
    let sort = lq
        .sort
        .as_deref()
        .and_then(Sort::parse)
        .unwrap_or(Sort::LastSeen);
    let pids: Vec<i64> = match &me.scope {
        Some(p) => vec![p.id],
        None => me.projects.iter().map(|p| p.id).collect(),
    };
    let mut f = svc::parse_query(
        &q,
        IssueFilter {
            project_ids: pids.clone(),
            sort: Some(sort),
            cursor: lq.cursor,
            limit: 25,
            ..Default::default()
        },
    );
    if f.assignee_id == Some(svc::ME) {
        f.assignee_id = Some(me.id);
    }
    let (rows, next) = if pids.is_empty() {
        (vec![], None)
    } else {
        svc::list_issues(&st.db, &f).await.map_err(E::from)?
    };
    let people = members(&st).await?;
    let link = |q: &str| format!("{base}?q={}&sort={}", enc(q), sort_str(sort));
    let rows = rows
        .into_iter()
        .map(|r| Row {
            bars: chart::bars(&r.sparkline, &[], 16, false),
            who: r
                .assignee_id
                .and_then(|a| people.iter().find(|m| m.id == a))
                .map(|m| m.initials.clone())
                .unwrap_or_default(),
            r,
        })
        .collect();
    let tabs = [
        ("Unresolved", Some("unresolved")),
        ("Resolved", Some("resolved")),
        ("Muted", Some("muted")),
        ("All", None),
    ]
    .into_iter()
    .map(|(l, v)| (l, link(&with_token(&q, "is", v)), f.status.as_deref() == v))
    .collect();
    let (releases, envs) = facets(&st, pids).await?;
    let chip = |label, key: &str, cur: Option<String>, opts: Vec<(String, String)>| Chip {
        label,
        current: cur,
        opts: opts
            .into_iter()
            .map(|(l, v)| (l, link(&with_token(&q, key, Some(&v)))))
            .collect(),
        clear: link(&with_token(&q, key, None)),
    };
    let pair = |v: &str| (v.to_string(), v.to_string());
    let assignee = f.assignee_id.map(|a| {
        people
            .iter()
            .find(|m| m.id == a)
            .map_or_else(|| a.to_string(), |m| m.name.clone())
    });
    let mut who: Vec<(String, String)> = vec![("Me".into(), "me".into())];
    who.extend(people.iter().map(|m| (m.name.clone(), m.id.to_string())));
    let chips = vec![
        chip(
            "Level",
            "level",
            f.level.clone(),
            ["fatal", "error", "warning", "info"].map(pair).to_vec(),
        ),
        chip(
            "Release",
            "release",
            f.release.clone(),
            releases.iter().map(|r| pair(r)).collect(),
        ),
        chip(
            "Environment",
            "env",
            f.environment.clone(),
            envs.iter().map(|r| pair(r)).collect(),
        ),
        chip("Assignee", "assignee", assignee, who),
    ];
    let more = next.map(|c| format!("{}&cursor={}", link(&q), enc(&c)));
    let filtered = q.split_whitespace().any(|t| !t.starts_with("is:"));
    let page = List {
        next: format!("{base}?q={}&sort={}", enc(&q), sort_str(sort)),
        me,
        base,
        q,
        sort: sort_str(sort),
        tabs,
        chips,
        rows,
        more,
        filtered,
        status: f.status.clone().unwrap_or_default(),
    };
    let _ = h; // htmx requests get the full page; hx-select picks the fragment.
    render(page)
}

/// Recent releases + environments across `pids` for the filter chips.
async fn facets(st: &AppState, pids: Vec<i64>) -> Result<(Vec<String>, Vec<String>), E> {
    let ids = pids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if ids.is_empty() {
        return Ok((vec![], vec![]));
    }
    Ok(st
        .db
        .read(move |c| {
            let releases = c
                .prepare(&format!("SELECT version FROM releases WHERE project_id IN ({ids}) ORDER BY first_seen DESC LIMIT 10"))?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            // ponytail: scans 30 days of events via the (project_id, received_at) index; cache if it shows in profiles.
            let envs = c
                .prepare(&format!("SELECT DISTINCT environment FROM events WHERE project_id IN ({ids}) AND received_at > ?1 AND environment IS NOT NULL LIMIT 10"))?
                .query_map([now() - 30 * 86400], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok((releases, envs))
        })
        .await?)
}

#[derive(Deserialize)]
struct ViewForm {
    name: String,
    q: String,
    sort: String,
}

async fn save_view(me: CurrentUser, State(st): State<AppState>, Form(f): Form<ViewForm>) -> R {
    let name: String = f.name.trim().chars().take(60).collect();
    if name.is_empty() {
        return flash("/issues", "Give the view a name");
    }
    let sort = sort_str(Sort::parse(&f.sort).unwrap_or(Sort::LastSeen));
    let (q2, uid) = (f.q.clone(), me.id);
    st.db
        .write(move |c| {
            c.execute(
                "INSERT INTO saved_views (user_id, name, query, sort, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![uid, name, q2, sort, now()],
            )
        })
        .await?;
    flash(
        &format!("/issues?q={}&sort={sort}", enc(&f.q)),
        "View saved",
    )
}

async fn delete_view(me: CurrentUser, State(st): State<AppState>, Path(id): Path<i64>) -> R {
    let uid = me.id;
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM saved_views WHERE id = ?1 AND user_id = ?2",
                [id, uid],
            )
        })
        .await?;
    flash("/issues", "View deleted")
}

// ---------- actions ----------

async fn issue_project(st: &AppState, id: i64) -> Result<Option<i64>, E> {
    Ok(st
        .db
        .read(move |c| {
            c.query_row("SELECT project_id FROM issues WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
        })
        .await?)
}

/// Visible issue whose project `me` administers (org admin, project admin, or team admin).
async fn can_delete(st: &AppState, me: &Me, id: i64) -> Result<(), E> {
    check(st, me, id).await?;
    let pid = issue_project(st, id).await?.ok_or_else(not_found)?;
    if crate::teams::project_admin(&st.db, me, pid).await? {
        Ok(())
    } else {
        Err(forbidden())
    }
}

async fn check(st: &AppState, me: &Me, id: i64) -> Result<(), E> {
    match issue_project(st, id).await? {
        Some(p) if me.projects.iter().any(|x| x.id == p) => Ok(()),
        _ => Err(not_found()),
    }
}

/// Only local paths, so `next` can't become an open redirect.
fn local(next: Option<&String>, fallback: &str) -> String {
    match next {
        // browsers drop tab/newline from URLs, so "/\t/evil" would become "//evil".
        Some(n)
            if n.starts_with('/')
                && !n.starts_with("//")
                && !n.contains('\\')
                && !n.chars().any(char::is_control) =>
        {
            n.clone()
        }
        _ => fallback.to_string(),
    }
}

fn parse_action(field: &dyn Fn(&str) -> Option<String>) -> Result<(IssueAction, &'static str), E> {
    let num = |k: &str| field(k).and_then(|v| v.trim().parse::<i64>().ok());
    let bad = |m: &str| E(StatusCode::UNPROCESSABLE_ENTITY, m.into());
    Ok(match field("action").as_deref().unwrap_or("") {
        "resolve" => {
            let in_release = field("in_release")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let next_release = field("next_release").is_some();
            let msg = if in_release.is_some() || next_release {
                "Resolves with the release"
            } else {
                "Resolved"
            };
            (
                IssueAction::Resolve {
                    in_release,
                    next_release,
                },
                msg,
            )
        }
        "unresolve" => (IssueAction::Unresolve, "Reopened"),
        "mute" => {
            let until = field("for").and_then(|d| match d.as_str() {
                "1h" => Some(3600),
                "1d" => Some(86400),
                "1w" => Some(7 * 86400),
                _ => None,
            });
            let after_count = num("after_count").filter(|n| *n > 0);
            let window_min = after_count.and(num("window_min").filter(|n| *n > 0));
            (
                IssueAction::Mute {
                    until: until.map(|s| now() + s),
                    after_count,
                    window_min,
                },
                "Muted",
            )
        }
        "unmute" => (IssueAction::Unmute, "Unmuted"),
        "approve" => (IssueAction::Approve, "Approved for fix"),
        "unapprove" => (IssueAction::Unapprove, "Approval removed"),
        "assign" => (
            IssueAction::Assign {
                user_id: num("user_id"),
            },
            "Assignee updated",
        ),
        "link_fix" => {
            let url = field("url").unwrap_or_default().trim().to_string();
            if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() > 2000 {
                return Err(bad("The fix link must be an http(s) URL."));
            }
            let note = field("note")
                .map(|s| s.trim().chars().take(2000).collect::<String>())
                .filter(|s| !s.is_empty());
            (IssueAction::LinkFix { url, note }, "Fix linked")
        }
        "comment" => {
            let text = field("text")
                .unwrap_or_default()
                .trim()
                .chars()
                .take(10_000)
                .collect::<String>();
            if text.is_empty() {
                return Err(bad("Write something first."));
            }
            (IssueAction::Comment { text }, "Comment added")
        }
        _ => return Err(bad("Unknown action.")),
    })
}

type Fields = Form<Vec<(String, String)>>;
fn getter(f: &[(String, String)]) -> impl Fn(&str) -> Option<String> + '_ {
    |k| f.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

async fn action(me: Me, State(st): State<AppState>, Path(id): Path<i64>, Form(f): Fields) -> R {
    check(&st, &me, id).await?;
    let field = getter(&f);
    let (a, msg) = parse_action(&field)?;
    if let IssueAction::Assign { user_id: Some(u) } = a
        && !svc::assignable(&st.db, id, u).await?
    {
        return Err(E(
            StatusCode::UNPROCESSABLE_ENTITY,
            "That person can't see this project.".into(),
        ));
    }
    svc::apply_action(&st.db, id, me.id, a)
        .await
        .map_err(E::from)?;
    flash(
        &local(field("next").as_ref(), &format!("/issues/{id}")),
        msg,
    )
}

async fn bulk(me: Me, State(st): State<AppState>, Form(f): Fields) -> R {
    let field = getter(&f);
    let next = local(field("next").as_ref(), "/issues");
    let ids: Vec<i64> = f
        .iter()
        .filter(|(k, _)| k == "id")
        .filter_map(|(_, v)| v.parse().ok())
        .collect();
    if ids.is_empty() {
        return flash(&next, "Select at least one issue");
    }
    let what = field("action").unwrap_or_default();
    let (_, msg) = parse_action(&|k| {
        if k == "action" {
            Some(what.clone())
        } else {
            None
        }
    })?;
    if !matches!(what.as_str(), "resolve" | "mute" | "approve") {
        return Err(E(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Unknown action.".into(),
        ));
    }
    // all-or-nothing: one invisible id rejects the batch before anything changes.
    for id in &ids {
        check(&st, &me, *id).await?;
    }
    for id in &ids {
        let (a, _) = parse_action(&|k| {
            if k == "action" {
                Some(what.clone())
            } else {
                None
            }
        })?;
        svc::apply_action(&st.db, *id, me.id, a)
            .await
            .map_err(E::from)?;
    }
    flash(
        &next,
        &format!(
            "{msg}: {} issue{}",
            ids.len(),
            if ids.len() == 1 { "" } else { "s" }
        ),
    )
}

#[derive(Template)]
#[template(path = "delete.html")]
struct DeletePage {
    me: Me,
    id: i64,
    title: String,
    slug: String,
}

async fn delete_page(me: Me, State(st): State<AppState>, Path(id): Path<i64>) -> R {
    can_delete(&st, &me, id).await?;
    let d = svc::get_issue(&st.db, id)
        .await
        .map_err(E::from)?
        .ok_or_else(not_found)?;
    let mut me = me;
    me.scope = me
        .projects
        .iter()
        .find(|p| p.id == d.row.project_id)
        .cloned();
    render(DeletePage {
        me,
        id,
        title: d.row.title,
        slug: d.row.project_slug,
    })
}

async fn delete(me: Me, State(st): State<AppState>, Path(id): Path<i64>) -> R {
    can_delete(&st, &me, id).await?;
    svc::apply_action(&st.db, id, me.id, IssueAction::Delete)
        .await
        .map_err(E::from)?;
    flash("/issues", "Issue deleted")
}

// ---------- detail ----------

pub struct FrameV {
    pub func: String,
    pub loc: String,
    pub in_app: bool,
    pub lines: Vec<(i64, String, bool)>,
}
pub struct Group {
    pub vendor: bool,
    pub frames: Vec<FrameV>,
}
pub struct ExcV {
    pub type_: String,
    pub value: String,
    pub mechanism: Option<String>,
    pub groups: Vec<Group>,
}
pub struct Crumb {
    pub icon: &'static str,
    pub cat: String,
    pub msg: String,
    /// "(no message)" placeholder, rendered muted
    pub muted: bool,
    /// http status suffix; `err` = status >= 400
    pub status: String,
    pub err: bool,
    pub level: String,
    pub when: String,
    pub at: String,
    pub data: Option<String>,
}
pub struct Req {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub body: Option<String>,
}
pub struct ActV {
    pub who: String,
    pub initials: String,
    pub text: String,
    pub detail: Option<String>,
    pub at: i64,
}
#[derive(Default)]
pub struct Nav {
    pub older: Option<String>,
    pub newer: Option<String>,
    pub oldest: Option<String>,
    pub newest: Option<String>,
}

fn s(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), String::from)
}
fn pretty(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}
fn ts(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|t| t.as_millisecond() as f64 / 1000.0),
        _ => None,
    }
}

fn frame_v(f: &svc::Frame, raw: bool) -> FrameV {
    let r = if raw { f.raw.as_ref() } else { None };
    let file = r
        .and_then(|r| r.get("abs_path").or(r.get("filename")))
        .map(s)
        .or_else(|| f.filename.clone().or(f.abs_path.clone()))
        .unwrap_or_else(|| "<unknown>".into());
    let line = r
        .and_then(|r| r.get("lineno"))
        .and_then(Value::as_i64)
        .or(f.lineno);
    let col = r
        .and_then(|r| r.get("colno"))
        .and_then(Value::as_i64)
        .or(f.colno);
    let loc = match (line, col) {
        (Some(l), Some(c)) => format!("{file}:{l}:{c}"),
        (Some(l), None) => format!("{file}:{l}"),
        _ => file,
    };
    let mut lines = vec![];
    if let (Some(cl), Some(l), false) = (&f.context_line, f.lineno, raw) {
        let pre = &f.pre_context[f.pre_context.len().saturating_sub(3)..];
        for (i, t) in pre.iter().enumerate() {
            lines.push((l - (pre.len() - i) as i64, t.clone(), false));
        }
        lines.push((l, cl.clone(), true));
        for (i, t) in f.post_context.iter().take(3).enumerate() {
            lines.push((l + 1 + i as i64, t.clone(), false));
        }
    }
    FrameV {
        func: f.function.clone().unwrap_or_else(|| "<anonymous>".into()),
        loc,
        in_app: f.in_app,
        lines,
    }
}

/// Exceptions outermost first; frames crash-first; consecutive vendor frames grouped.
fn exceptions(ev: &EventDetail, raw: bool) -> Vec<ExcV> {
    ev.exceptions
        .iter()
        .rev()
        .map(|e| {
            let any_app = e.frames.iter().any(|f| f.in_app);
            let mut groups: Vec<Group> = vec![];
            for f in e.frames.iter().rev() {
                let vendor = any_app && !f.in_app;
                match groups.last_mut() {
                    Some(g) if vendor && g.vendor => g.frames.push(frame_v(f, raw)),
                    _ => groups.push(Group {
                        vendor,
                        frames: vec![frame_v(f, raw)],
                    }),
                }
            }
            ExcV {
                type_: e.type_.clone(),
                value: e.value.clone(),
                mechanism: e.mechanism.clone(),
                groups,
            }
        })
        .collect()
}

/// Non-empty string form; null / "null" / "None" count as absent.
fn opt(v: Option<&Value>) -> Option<String> {
    let v = s(v.filter(|v| !v.is_null())?);
    (!v.is_empty() && v != "null" && v != "None").then_some(v)
}

/// Summary line for a crumb without a message, derived from its data (sentry-rails etc.).
fn summary(cat: &str, d: &Value) -> Option<(String, String, bool)> {
    let g = |k: &str| opt(d.get(k));
    if let Some(sql) = g("sql") {
        let mut t: String = sql.chars().take(160).collect();
        if sql.chars().count() > 160 {
            t.push('…');
        }
        return Some((
            g("name").map_or(t.clone(), |n| format!("{n} {t}")),
            "".into(),
            false,
        ));
    }
    if let (Some(n), Some(c)) = (g("record_count"), g("class_name")) {
        return Some((format!("{n} × {c}"), "".into(), false));
    }
    if cat.starts_with("cache") {
        let hit = d.get("hit").and_then(Value::as_bool);
        let h = hit.map_or("", |h| if h { " hit" } else { " miss" });
        return g("key").map(|k| (format!("{k}{h}"), "".into(), false));
    }
    if let (Some(m), Some(u)) = (g("method"), g("url")) {
        let code = d.get("status_code").or(d.get("status"));
        let st = opt(code).map_or(String::new(), |c| format!(" → {c}"));
        let err = code.and_then(|c| c.as_i64().or_else(|| c.as_str()?.parse().ok())) >= Some(400);
        return Some((format!("{m} {u}"), st, err));
    }
    if let (Some(f), Some(t)) = (g("from"), g("to")) {
        return Some((format!("{f} → {t}"), "".into(), false));
    }
    g("message")
        .or_else(|| {
            d.as_object()?
                .values()
                .find_map(|v| opt(Some(v).filter(|v| v.is_string())))
        })
        .map(|m| (m, "".into(), false))
}

fn crumbs(ev: &EventDetail) -> Vec<Crumb> {
    let crash = ev.timestamp as f64;
    ev.breadcrumbs
        .iter()
        .map(|b| {
            let cat = b
                .get("category")
                .or(b.get("type"))
                .map(s)
                .unwrap_or_else(|| "default".into());
            let icon = match cat.split('.').next().unwrap_or("") {
                "http" | "fetch" | "xhr" => "arrows-left-right",
                "navigation" => "arrow-right",
                "console" => "terminal",
                "ui" => "cursor-click",
                "query" | "sql" => "database",
                _ => "dot-outline",
            };
            let when = ts(b.get("timestamp")).map_or_else(String::new, |t| {
                let d = t - crash;
                if d.abs() < 60.0 {
                    format!("{d:+.1}s")
                } else {
                    format!("{:+.1}m", d / 60.0)
                }
            });
            let at = ts(b.get("timestamp"))
                .and_then(|t| jiff::Timestamp::from_millisecond((t * 1000.0) as i64).ok())
                .map_or(String::new(), |t| t.to_string());
            let (msg, status, err) = opt(b.get("message"))
                .map(|m| (m, String::new(), false))
                .or_else(|| summary(&cat, b.get("data")?))
                .unwrap_or_default();
            Crumb {
                icon,
                muted: msg.is_empty(),
                msg: if msg.is_empty() {
                    "(no message)".into()
                } else {
                    msg
                },
                status,
                err,
                cat,
                level: opt(b.get("level")).unwrap_or_default(),
                when,
                at,
                data: b.get("data").filter(|d| !d.is_null()).map(pretty),
            }
        })
        .collect()
}

fn pairs(v: Option<&Value>) -> Vec<(String, String)> {
    match v {
        Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), s(v))).collect(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|p| Some((s(p.get(0)?), s(p.get(1)?))))
            .collect(),
        Some(Value::String(q)) if !q.is_empty() => vec![("".into(), q.clone())],
        _ => vec![],
    }
}

fn contexts(ev: &EventDetail) -> Vec<(String, String)> {
    let mut out = vec![];
    if let Value::Object(m) = &ev.contexts {
        for (k, v) in m {
            match v {
                Value::Object(f) => out.extend(
                    f.iter()
                        .filter(|(fk, _)| *fk != "type")
                        .map(|(fk, fv)| (format!("{k}.{fk}"), s(fv))),
                ),
                v => out.push((k.clone(), s(v))),
            }
        }
    }
    if let Some(sdk) = &ev.sdk {
        out.push(("sdk".into(), sdk.clone()));
    }
    out
}

fn activity(d: &IssueDetail) -> Vec<ActV> {
    d.activity
        .iter()
        .map(|a| {
            let g = |k: &str| a.data.get(k).filter(|v| !v.is_null()).map(s);
            let text = match a.kind.as_str() {
                "resolve" => match (
                    g("in_release"),
                    a.data.get("next_release").and_then(Value::as_bool),
                ) {
                    (Some(r), _) => format!("marked resolved in {r}"),
                    (_, Some(true)) => "marked resolved in the next release".into(),
                    _ => "resolved the issue".into(),
                },
                "unresolve" => "reopened the issue".into(),
                "mute" => "muted the issue".into(),
                "unmute" => "unmuted the issue".into(),
                "approve" => "approved it for a fix".into(),
                "unapprove" => "removed the fix approval".into(),
                "assign" => {
                    if g("user_id").is_some() {
                        "changed the assignee".into()
                    } else {
                        "unassigned the issue".into()
                    }
                }
                "link_fix" => "linked a fix".into(),
                "comment" => "commented".into(),
                "regressed" => "Regressed: seen again after being resolved".into(),
                k => k.replace('_', " "),
            };
            let who = a
                .user_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| {
                    if a.user_id.is_some() {
                        "Someone".into()
                    } else {
                        "OpenFault".into()
                    }
                });
            ActV {
                initials: initials(&who, ""),
                who,
                text,
                detail: g("text").or(g("url").map(|u| match g("note") {
                    Some(n) => format!("{u}\n{n}"),
                    None => u,
                })),
                at: a.created_at,
            }
        })
        .collect()
}

#[derive(Template)]
#[template(path = "issue.html")]
struct Detail {
    me: Me,
    d: IssueDetail,
    ev: Option<EventDetail>,
    url: String,
    tab: &'static str,
    raw: bool,
    excs: Vec<ExcV>,
    crumbs: Vec<Crumb>,
    req: Option<Req>,
    ctx: Vec<(String, String)>,
    user: Vec<(String, String)>,
    events: Vec<svc::EventSummary>,
    events_more: Option<String>,
    nav: Nav,
    members: Vec<Member>,
    acts: Vec<ActV>,
    fingerprint: Option<String>,
    first_release: Option<String>,
    assignee: Option<String>,
    fix_href: Option<String>,
    fix_label: String,
    /// url + current tab, where actions return to
    here: String,
    spark: chart::Chart,
}

#[derive(Deserialize, Default)]
struct DetailQ {
    tab: Option<String>,
    raw: Option<String>,
    ecursor: Option<String>,
}

async fn detail(
    me: Me,
    st: State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DetailQ>,
) -> R {
    if let Some(id) = id.strip_suffix(".md").and_then(|i| i.parse().ok()) {
        return markdown(me, st, id).await;
    }
    let id: i64 = id.parse().map_err(|_| not_found())?;
    show(me, st, id, None, q).await
}

async fn event(
    me: Me,
    st: State<AppState>,
    Path((id, eid)): Path<(i64, String)>,
    Query(q): Query<DetailQ>,
) -> R {
    show(me, st, id, Some(eid), q).await
}

async fn load(st: &AppState, me: &Me, id: i64) -> Result<IssueDetail, E> {
    check(st, me, id).await?;
    svc::get_issue(&st.db, id)
        .await
        .map_err(E::from)?
        .ok_or_else(not_found)
}

async fn show(
    mut me: Me,
    State(st): State<AppState>,
    id: i64,
    eid: Option<String>,
    q: DetailQ,
) -> R {
    let d = load(&st, &me, id).await?;
    me.scope = me
        .projects
        .iter()
        .find(|p| p.id == d.row.project_id)
        .cloned();

    let tab = match q.tab.as_deref() {
        Some("breadcrumbs") => "breadcrumbs",
        Some("request") => "request",
        Some("tags") => "tags",
        Some("events") => "events",
        Some("activity") => "activity",
        Some("grouping") => "grouping",
        _ => "stack",
    };
    let raw = q.raw.is_some();
    let mut nav = Nav::default();
    let (first_release, oldest, newest): (Option<String>, Option<String>, Option<String>) = st
        .db
        .read(move |c| {
            let one = |sql: &str| c.query_row(sql, [id], |r| r.get(0)).optional().map(Option::flatten);
            Ok((
                one("SELECT first_release FROM issues WHERE id = ?1")?,
                one("SELECT id FROM events WHERE issue_id = ?1 ORDER BY received_at, rowid LIMIT 1")?,
                one("SELECT id FROM events WHERE issue_id = ?1 ORDER BY received_at DESC, rowid DESC LIMIT 1")?,
            ))
        })
        .await?;
    // newest by (received_at, rowid), the same order adjacent_events walks
    let ev = match eid.as_ref().or(newest.as_ref()) {
        Some(e) => Some(
            svc::get_event(&st.db, e)
                .await
                .map_err(E::from)?
                .filter(|e| e.issue_id == id)
                .ok_or_else(not_found)?,
        ),
        None => None,
    };
    if let Some(e) = &ev {
        let (older, newer) = svc::adjacent_events(&st.db, id, &e.id)
            .await
            .map_err(E::from)?;
        nav = Nav {
            older,
            newer,
            oldest: oldest.filter(|o| *o != e.id),
            newest: newest.filter(|n| *n != e.id),
        };
    }
    let (events, events_more) = if tab == "events" {
        svc::list_events(&st.db, id, q.ecursor, 25)
            .await
            .map_err(E::from)?
    } else {
        (vec![], None)
    };
    let url = match &eid {
        Some(e) => format!("/issues/{id}/events/{}", enc(e)),
        None => format!("/issues/{id}"),
    };
    let req = ev.as_ref().and_then(|e| e.request.as_ref()).map(|r| Req {
        method: r.get("method").map(s).unwrap_or_else(|| "GET".into()),
        url: r.get("url").map(s).unwrap_or_default(),
        headers: pairs(r.get("headers")),
        query: pairs(r.get("query_string")),
        body: r.get("data").filter(|d| !d.is_null()).map(pretty),
    });
    let members = members(&st).await?;
    let assignee = d
        .row
        .assignee_id
        .and_then(|a| members.iter().find(|m| m.id == a))
        .map(|m| m.name.clone());
    let fix_href = d
        .row
        .fix_link
        .clone()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"));
    // "acme/app#42" or a short commit for the chip; the full URL stays in the title
    let fix_label = fix_href.as_deref().map_or_else(String::new, |u| {
        let p: Vec<&str> = u.split('/').filter(|s| !s.is_empty()).collect();
        match p.as_slice() {
            [.., o, r, "pull" | "pulls" | "merge_requests", n] => format!("{o}/{r}#{n}"),
            [.., o, r, "commit", sha] => format!("{o}/{r}@{}", &sha[..sha.len().min(7)]),
            _ => p.get(1).map_or(u.to_string(), |h| h.to_string()),
        }
    });
    let here = if tab == "stack" {
        url.clone()
    } else {
        format!("{url}?tab={tab}")
    };
    let spark = chart::area(format!("i{id}"), &d.row.sparkline, 40);
    render(Detail {
        fix_label,
        here,
        spark,
        excs: ev.as_ref().map(|e| exceptions(e, raw)).unwrap_or_default(),
        crumbs: ev.as_ref().map(crumbs).unwrap_or_default(),
        ctx: ev.as_ref().map(contexts).unwrap_or_default(),
        user: pairs(ev.as_ref().and_then(|e| e.user.as_ref())),
        fingerprint: ev
            .as_ref()
            .and_then(|e| e.raw.get("fingerprint"))
            .and_then(Value::as_array)
            .map(|a| a.iter().map(s).collect::<Vec<_>>().join(", ")),
        acts: activity(&d),
        req,
        events,
        events_more,
        nav,
        members,
        first_release,
        assignee,
        fix_href,
        me,
        d,
        ev,
        url,
        tab,
        raw,
    })
}

async fn markdown(me: Me, State(st): State<AppState>, id: i64) -> R {
    let d = load(&st, &me, id).await?;
    let o = issue_markdown(&d, true);
    Ok(([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], o).into_response())
}

/// Plain Markdown for pasting into a ticket or an agent prompt. Unapproved (API) → summary only.
pub fn issue_markdown(d: &IssueDetail, approved: bool) -> String {
    let r = &d.row;
    let flat = |s: &str| svc::untrusted(s).replace(['\n', '\t'], " ");
    let mut o = format!(
        "# {}\n\n`{}` · {} · {}\n\n- Events: {}\n- Users affected: {}\n- First seen: {}\n- Last seen: {}\n",
        flat(&r.title),
        flat(&r.culprit),
        svc::untrusted(&r.level),
        r.status,
        n(&r.event_count),
        n(&r.user_count),
        abs(&r.first_seen),
        abs(&r.last_seen),
    );
    if let Some(rel) = &r.last_release {
        o += &format!("- Last release: {}\n", svc::untrusted(rel));
    }
    if !approved {
        o += "\nFix not approved. Ask a person to approve it in OpenFault.\n";
        return o;
    }
    if let Some(ev) = &d.latest_event {
        for e in exceptions(ev, false) {
            o += &format!(
                "\n## {}: {}\n\n```\n",
                svc::untrusted(&e.type_),
                svc::untrusted(&e.value)
            );
            for f in e.groups.iter().flat_map(|g| &g.frames).filter(|f| f.in_app) {
                o += &format!(
                    "at {} ({})\n",
                    svc::untrusted(&f.func),
                    svc::untrusted(&f.loc)
                );
                if let Some((_, l, _)) = f.lines.iter().find(|l| l.2) {
                    o += &format!("    {}\n", svc::untrusted(l.trim()));
                }
            }
            o += "```\n";
        }
        let cs = crumbs(ev);
        if !cs.is_empty() {
            o += "\n## Breadcrumbs\n\n```\n";
            for c in cs.iter().rev().take(20).rev() {
                o += &format!(
                    "{:>8} [{}] {}\n",
                    c.when,
                    svc::untrusted(&c.cat),
                    svc::untrusted(&c.msg)
                );
            }
            o += "```\n";
        }
    }
    o
}
