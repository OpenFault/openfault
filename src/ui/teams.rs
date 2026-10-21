//! Settings → Teams: list/create (org admin), team page (team admins: members + projects).
use super::{E, Me, R, flash, forbidden, not_found, render};
use crate::{AppState, teams};
use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, State},
    routing::{get, post},
};
use rusqlite::params;
use serde::Deserialize;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings/teams", get(list).post(create))
        .route("/settings/teams/{slug}", get(page))
        .route("/settings/teams/{slug}/delete", post(delete))
        .route("/settings/teams/{slug}/members", post(set_member))
        .route(
            "/settings/teams/{slug}/members/{uid}/remove",
            post(remove_member),
        )
        .route("/settings/teams/{slug}/projects", post(add_project))
        .route(
            "/settings/teams/{slug}/projects/{pid}/remove",
            post(remove_project),
        )
}

#[derive(Template)]
#[template(path = "teams.html")]
struct List {
    me: Me,
    teams: Vec<teams::Team>,
    error: Option<String>,
}

async fn list_page(st: &AppState, me: Me, error: Option<String>) -> R {
    let u = me.user.clone();
    let teams = st.db.read(move |c| teams::list(c, &u)).await?;
    render(List { me, teams, error })
}
async fn list(me: Me, State(st): State<AppState>) -> R {
    list_page(&st, me, None).await
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}
async fn create(me: Me, State(st): State<AppState>, Form(f): Form<NameForm>) -> R {
    super::admin(&me)?;
    let name = f.name.trim().to_string();
    match st.db.write(move |c| teams::create(c, &name)).await? {
        Some(slug) => flash(&format!("/settings/teams/{slug}"), "Team created"),
        None => {
            list_page(
                &st,
                me,
                Some("A team with that name already exists.".into()),
            )
            .await
        }
    }
}

/// Visible team, and with `manage` the viewer must be org admin or team admin.
async fn team(st: &AppState, me: &Me, slug: String, manage: bool) -> Result<teams::Team, E> {
    let u = me.user.clone();
    let t = st
        .db
        .read(move |c| teams::find(c, &u, &slug))
        .await?
        .ok_or_else(not_found)?;
    if manage && !t.manage {
        return Err(forbidden());
    }
    Ok(t)
}

#[derive(Template)]
#[template(path = "team.html")]
struct Page {
    me: Me,
    team: teams::Team,
    members: Vec<teams::Person>,
    projects: Vec<(i64, String, String)>,
    /// org users not yet in the team: (id, label)
    others: Vec<(i64, String)>,
    /// projects the viewer administers that the team doesn't have: (id, name)
    addable: Vec<(i64, String)>,
}

async fn page(me: Me, State(st): State<AppState>, Path(slug): Path<String>) -> R {
    let team = team(&st, &me, slug, false).await?;
    let (tid, uid, manage) = (team.id, me.id, team.manage);
    let (members, projects, others, addable) = st
        .db
        .read(move |c| {
            let others = c
                .prepare("SELECT id, iif(name = '', email, name || ' · ' || email) FROM users
                          WHERE ?2 AND id NOT IN (SELECT user_id FROM team_members WHERE team_id = ?1) ORDER BY name, email")?
                .query_map(params![tid, manage], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let addable = c
                .prepare("SELECT id, name FROM projects p WHERE id NOT IN (SELECT project_id FROM team_projects WHERE team_id = ?1)
                          ORDER BY name")?
                .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<(i64, String)>>>()?
                .into_iter()
                .filter(|(pid, _)| teams::is_project_admin(c, uid, *pid).unwrap_or(false))
                .collect();
            Ok((teams::members(c, tid)?, teams::projects(c, tid)?, others, addable))
        })
        .await?;
    render(Page {
        me,
        team,
        members,
        projects,
        others,
        addable,
    })
}

async fn delete(me: Me, State(st): State<AppState>, Path(slug): Path<String>) -> R {
    super::admin(&me)?;
    let t = team(&st, &me, slug, true).await?;
    st.db
        .write(move |c| c.execute("DELETE FROM teams WHERE id = ?1", [t.id]))
        .await?;
    crate::live::changed(&st.db, 0);
    flash("/settings/teams", "Team deleted")
}

#[derive(Deserialize)]
struct MemberForm {
    user_id: i64,
    role: String,
}
/// Add an org member, or change their team role.
async fn set_member(
    me: Me,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Form(f): Form<MemberForm>,
) -> R {
    let t = team(&st, &me, slug.clone(), true).await?;
    st.db
        .write(move |c| teams::set_member(c, t.id, f.user_id, &f.role))
        .await?;
    crate::live::changed(&st.db, 0);
    flash(&format!("/settings/teams/{slug}"), "Member saved")
}

async fn remove_member(
    me: Me,
    State(st): State<AppState>,
    Path((slug, uid)): Path<(String, i64)>,
) -> R {
    let t = team(&st, &me, slug.clone(), true).await?;
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM team_members WHERE team_id = ?1 AND user_id = ?2",
                [t.id, uid],
            )
        })
        .await?;
    crate::live::changed(&st.db, 0);
    flash(&format!("/settings/teams/{slug}"), "Member removed")
}

#[derive(Deserialize)]
struct ProjectForm {
    project_id: i64,
}
async fn add_project(
    me: Me,
    State(st): State<AppState>,
    Path(slug): Path<String>,
    Form(f): Form<ProjectForm>,
) -> R {
    let t = team(&st, &me, slug.clone(), true).await?;
    // team admins can only bring in projects they already administer.
    if !teams::project_admin(&st.db, &me, f.project_id).await? {
        return Err(forbidden());
    }
    st.db
        .write(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO team_projects (team_id, project_id) SELECT ?1, id FROM projects WHERE id = ?2",
                params![t.id, f.project_id],
            )
        })
        .await?;
    crate::live::changed(&st.db, 0);
    flash(&format!("/settings/teams/{slug}"), "Project added")
}

async fn remove_project(
    me: Me,
    State(st): State<AppState>,
    Path((slug, pid)): Path<(String, i64)>,
) -> R {
    let t = team(&st, &me, slug.clone(), true).await?;
    st.db
        .write(move |c| {
            c.execute(
                "DELETE FROM team_projects WHERE team_id = ?1 AND project_id = ?2",
                [t.id, pid],
            )
        })
        .await?;
    crate::live::changed(&st.db, 0);
    flash(&format!("/settings/teams/{slug}"), "Project removed")
}
