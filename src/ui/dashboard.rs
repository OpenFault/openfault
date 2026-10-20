//! Cross-project dashboard (PLAN §8).
use super::{
    E, Me, R,
    chart::{self, Chart},
    render,
};
use crate::{
    AppState,
    issues::{self as svc, IssueRow, Range},
};
use askama::Template;
use axum::{
    Router,
    extract::{Query, State},
    routing::get,
};
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new().route("/", get(home))
}

#[derive(Deserialize)]
struct Q {
    range: Option<String>,
}

pub struct Card {
    pub name: String,
    pub slug: String,
    pub chart: Chart,
    pub received: i64,
    pub unresolved: i64,
    pub new_issues: i64,
    pub regressions: i64,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct Dash {
    me: Me,
    range: &'static str,
    label: &'static str,
    received: i64,
    dropped: i64,
    unresolved: i64,
    new_issues: i64,
    regressions: i64,
    cards: Vec<Card>,
    overall: Chart,
    top: Vec<IssueRow>,
    empty: bool,
}

async fn home(me: Me, State(st): State<AppState>, Query(q): Query<Q>) -> R {
    let (range, key, label) = match q.range.as_deref() {
        Some("7d") => (Range::Week, "7d", "last 7 days"),
        Some("30d") => (Range::Month, "30d", "last 30 days"),
        _ => (Range::Day, "24h", "last 24 hours"),
    };
    let ids: Vec<i64> = me.projects.iter().map(|p| p.id).collect();
    let s = svc::stats(&st.db, &ids, range).await.map_err(E::from)?;
    let cards: Vec<Card> = s
        .per_project
        .iter()
        .map(|p| {
            let vals: Vec<i64> = p.buckets.iter().map(|b| b.1).collect();
            Card {
                name: p.name.clone(),
                slug: p.slug.clone(),
                chart: chart::area(format!("p{}", p.project_id), &vals, 56),
                received: vals.iter().sum(),
                unresolved: p.unresolved,
                new_issues: p.new_issues,
                regressions: p.regressions,
            }
        })
        .collect();
    let (_, step, _) = range.buckets(crate::db::now());
    let ts: Vec<i64> = s
        .per_project
        .first()
        .map(|p| p.buckets.iter().map(|b| b.0).collect())
        .unwrap_or_default();
    let totals: Vec<i64> = (0..ts.len())
        .map(|i| s.per_project.iter().map(|p| p.buckets[i].1).sum())
        .collect();
    let overall = chart::bars(&totals, &chart::labels(&ts, step), 96, true);
    let sum = |f: fn(&Card) -> i64| cards.iter().map(f).sum::<i64>();
    let (unresolved, new_issues, regressions) = (
        sum(|c| c.unresolved),
        sum(|c| c.new_issues),
        sum(|c| c.regressions),
    );
    // ponytail: "empty" = nothing ever arrived in range and nothing open; good enough for a first-run hint.
    let empty = s.total_received == 0 && unresolved == 0 && s.top_issues.is_empty();
    render(Dash {
        me,
        range: key,
        label,
        received: s.total_received,
        dropped: s.total_dropped,
        unresolved,
        new_issues,
        regressions,
        cards,
        overall,
        top: s.top_issues,
        empty,
    })
}
