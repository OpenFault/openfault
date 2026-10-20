//! Releases list (PLAN §8).
use super::{E, Me, R, enc, project_for, render, to};
use crate::{
    AppState,
    issues::{self as svc, ReleaseRow},
};
use askama::Template;
use axum::{
    Router,
    extract::{Path, State},
    routing::get,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/releases", get(all))
        .route("/releases/{version}", get(one))
        .route("/projects/{slug}/releases", get(project))
}

#[derive(Template)]
#[template(path = "releases.html")]
struct Releases {
    me: Me,
    rows: Vec<(String, ReleaseRow)>,
}

async fn all(me: Me, st: State<AppState>) -> R {
    list(me, st).await
}

async fn project(mut me: Me, st: State<AppState>, Path(slug): Path<String>) -> R {
    me.scope = Some(project_for(&st, &me, slug).await?);
    list(me, st).await
}

async fn list(me: Me, State(st): State<AppState>) -> R {
    let mut rows = vec![];
    for p in me
        .projects
        .iter()
        .filter(|p| me.scope.as_ref().is_none_or(|s| s.id == p.id))
    {
        for r in svc::list_releases(&st.db, p.id, 50)
            .await
            .map_err(E::from)?
        {
            rows.push((p.slug.clone(), r));
        }
    }
    // newest first: semver order, then first-seen order for non-semver versions
    rows.sort_by(|a, b| (b.1.is_semver, &b.1.sort_key).cmp(&(a.1.is_semver, &a.1.sort_key)));
    rows.truncate(100);
    render(Releases { me, rows })
}

async fn one(Path(v): Path<String>) -> R {
    let tok = if v.contains(' ') {
        format!("release:\"{v}\"")
    } else {
        format!("release:{v}")
    };
    to(&format!("/issues?q={}", enc(&tok)))
}
