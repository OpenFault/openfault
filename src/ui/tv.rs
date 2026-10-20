//! TV mode (PLAN §8): one dark, chrome-free screen a team leaves on. Session auth like every page.
use super::{
    E, Me, R, base_url,
    chart::{self, Chart},
    project_for, render,
};
use crate::{
    AppState,
    db::now,
    issues::{self as svc, IssueFilter, IssueRow, Range, Sort},
};
use askama::Template;
use axum::{
    Router,
    extract::{Query, State},
    http::HeaderMap,
    routing::get,
};
use rusqlite::params;
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new().route("/tv", get(tv))
}

#[derive(Deserialize)]
struct Q {
    project: Option<String>,
}

pub struct TvCard {
    pub name: String,
    pub chart: Chart,
    pub unresolved: i64,
    pub received: i64,
    /// last 12h vs the 12h before, e.g. "+40%"; None when there's nothing to compare
    pub trend: Option<(bool, String)>,
}

#[derive(Template)]
#[template(path = "tv.html")]
struct Tv {
    me: Me,
    org: String,
    scope: String,
    title: Option<String>,
    unresolved: i64,
    received_1h: i64,
    received_24h: i64,
    new_issues: i64,
    regressions: i64,
    hour: Chart,
    cards: Vec<TvCard>,
    latest: Vec<IssueRow>,
    qr: String,
}

fn trend(b: &[i64]) -> Option<(bool, String)> {
    let (old, new) = b.split_at(b.len() / 2);
    let (o, n): (i64, i64) = (old.iter().sum(), new.iter().sum());
    if o == 0 {
        return None;
    }
    let pct = (n - o) * 100 / o;
    Some((
        pct > 0,
        format!("{}{}%", if pct > 0 { "+" } else { "" }, pct),
    ))
}

/// A QR for phones; the one place the `qrcode` crate is used.
pub(super) fn qr(url: &str) -> String {
    qrcode::QrCode::new(url.as_bytes())
        .map(|c| {
            let s = c
                .render::<qrcode::render::svg::Color>()
                .quiet_zone(false)
                .dark_color(qrcode::render::svg::Color("currentColor"))
                .light_color(qrcode::render::svg::Color("transparent"))
                .build();
            // drop the XML prolog so it inlines
            s[s.find("<svg").unwrap_or(0)..].replacen(
                "<svg",
                "<svg class=\"qr\" aria-hidden=\"true\"",
                1,
            )
        })
        .unwrap_or_default()
}

async fn tv(mut me: Me, State(st): State<AppState>, h: HeaderMap, Query(q): Query<Q>) -> R {
    if let Some(slug) = q.project.filter(|s| !s.is_empty()) {
        me.scope = Some(project_for(&st, &me, slug).await?);
    }
    let pids: Vec<i64> = match &me.scope {
        Some(p) => vec![p.id],
        None => me.projects.iter().map(|p| p.id).collect(),
    };
    let s = svc::stats(&st.db, &pids, Range::Day)
        .await
        .map_err(E::from)?;
    let t = now();
    let first = t - t % 300 - 11 * 300;
    let ids = pids.clone();
    let (org, series): (String, Vec<i64>) = st
        .db
        .read(move |c| {
            let org = c.query_row("SELECT name FROM orgs WHERE id = 1", [], |r| r.get(0))?;
            let mut series = vec![0i64; 12];
            let mut q = c.prepare_cached(
                "SELECT bucket_ts, sum(count) FROM project_5min
                 WHERE project_id IN (SELECT value FROM json_each(?1)) AND bucket_ts >= ?2 GROUP BY bucket_ts",
            )?;
            for r in q.query_map(params![serde_json::json!(ids).to_string(), first], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })? {
                let (b, n) = r?;
                if let Some(x) = series.get_mut(((b - first) / 300) as usize) {
                    *x += n;
                }
            }
            Ok((org, series))
        })
        .await?;
    let latest = if pids.is_empty() {
        vec![]
    } else {
        let f = IssueFilter {
            project_ids: pids.clone(),
            sort: Some(Sort::FirstSeen),
            limit: 8,
            ..Default::default()
        };
        svc::list_issues(&st.db, &f).await.map_err(E::from)?.0
    };
    let cards: Vec<TvCard> = s
        .per_project
        .iter()
        .map(|p| {
            let vals: Vec<i64> = p.buckets.iter().map(|b| b.1).collect();
            TvCard {
                name: p.name.clone(),
                chart: chart::area(format!("tv{}", p.project_id), &vals, 72),
                unresolved: p.unresolved,
                received: vals.iter().sum(),
                trend: trend(&vals),
            }
        })
        .collect();
    let hour_ts: Vec<i64> = (0..12).map(|i| first + i * 300).collect();
    let base = base_url(&st, &h);
    let (scope, link) = match &me.scope {
        Some(p) => (
            format!("project:{}", p.id),
            format!("{base}/projects/{}/issues", p.slug),
        ),
        None => ("all".into(), format!("{base}/")),
    };
    render(Tv {
        org,
        scope,
        title: me.scope.as_ref().map(|p| p.name.clone()),
        unresolved: cards.iter().map(|c| c.unresolved).sum(),
        received_1h: series.iter().sum(),
        received_24h: s.total_received,
        new_issues: s.per_project.iter().map(|p| p.new_issues).sum(),
        regressions: s.per_project.iter().map(|p| p.regressions).sum(),
        hour: chart::bars(&series, &chart::labels(&hour_ts, 300), 160, true),
        cards,
        latest,
        qr: qr(&link),
        me,
    })
}
