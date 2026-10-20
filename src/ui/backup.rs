//! Settings → Backups and wizard step 3 (/setup/backup). Admin only; secrets never rendered.
use super::{Me, R, admin, flash, render, slugify, to};
use crate::{AppState, backup, config};
use askama::Template;
use axum::{
    Form, Router,
    extract::{Query, State},
    routing::get,
};
use serde::Deserialize;
use std::time::Duration;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings/backups", get(page).post(save))
        .route("/setup/backup", get(setup))
}

pub struct Run {
    pub started_at: i64,
    pub key: Option<String>,
    pub bytes: Option<i64>,
    pub ok: bool,
    pub error: Option<String>,
    pub done: bool,
}

pub struct View {
    /// effective settings (stored + env), secret cleared
    pub b: config::Backup,
    pub secret_set: bool,
    pub env: bool,
    /// wizard: project slug to continue to; empty on the Settings page
    pub project: String,
    /// (ok, message) of the last Test / Back up now
    pub result: Option<(bool, String)>,
    pub runs: Vec<Run>,
    /// failed runs since the last good one
    pub failing: usize,
    pub restore: Option<String>,
}

async fn view(
    st: &AppState,
    project: String,
    result: Option<(bool, String)>,
) -> Result<View, super::E> {
    let cfg = config::get(&st.db);
    let mut b = config::backup_from_env(cfg.stored.backup.clone().unwrap_or_default());
    let secret_set = !b.secret_key.is_empty();
    b.secret_key.clear();
    let runs: Vec<Run> = st
        .db
        .read(|c| {
            c.prepare("SELECT started_at, key, bytes, ok, error, finished_at IS NOT NULL FROM backups_log ORDER BY id DESC LIMIT 20")?
                .query_map([], |r| {
                    Ok(Run {
                        started_at: r.get(0)?,
                        key: r.get(1)?,
                        bytes: r.get(2)?,
                        ok: r.get(3)?,
                        error: r.get(4)?,
                        done: r.get(5)?,
                    })
                })?
                .collect()
        })
        .await?;
    let failing = runs.iter().take_while(|r| r.done && !r.ok).count();
    let restore = runs.iter().find_map(|r| r.key.clone()).map(|k| {
        let ep = if b.endpoint.is_empty() {
            String::new()
        } else {
            format!("OPENFAULT_BACKUP_S3_ENDPOINT={} ", b.endpoint)
        };
        format!(
            "{ep}OPENFAULT_BACKUP_S3_REGION={} \\\n  OPENFAULT_BACKUP_S3_ACCESS_KEY=<access key> OPENFAULT_BACKUP_S3_SECRET_KEY=<secret key> \\\n  openfault restore s3://{}/{k}",
            b.region, b.bucket
        )
    });
    Ok(View {
        b,
        secret_set,
        env: cfg.backup_env,
        project,
        result,
        runs,
        failing,
        restore,
    })
}

#[derive(Template)]
#[template(path = "backup.html")]
struct Page {
    me: Me,
    v: View,
}

#[derive(Template)]
#[template(path = "setup_backup.html")]
struct Setup {
    v: View,
}

async fn page(me: Me, State(st): State<AppState>) -> R {
    admin(&me)?;
    let v = view(&st, String::new(), None).await?;
    render(Page { me, v })
}

#[derive(Deserialize)]
struct ProjectQ {
    #[serde(default)]
    project: String,
}

async fn setup(me: Me, State(st): State<AppState>, Query(q): Query<ProjectQ>) -> R {
    admin(&me)?;
    render(Setup {
        v: view(&st, slugify(&q.project), None).await?,
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct F {
    enabled: String,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    access_key: String,
    secret_key: String,
    hour_utc: String,
    keep: String,
    path_style: String,
    /// save | test | now
    action: String,
    project: String,
}

/// Saves (unless set by env; blank secret = keep), then optionally tests or runs a backup.
async fn save(me: Me, State(st): State<AppState>, Form(f): Form<F>) -> R {
    admin(&me)?;
    let project = slugify(&f.project);
    let action = f.action.clone();
    if !config::get(&st.db).backup_env {
        config::update(&st.db, move |s| {
            let old = s.backup.take().map(|b| b.secret_key).unwrap_or_default();
            let t = |x: &str| x.trim().to_string();
            s.backup = Some(config::Backup {
                enabled: !f.enabled.is_empty(),
                endpoint: t(&f.endpoint),
                region: Some(t(&f.region))
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "us-east-1".into()),
                bucket: t(&f.bucket),
                prefix: t(&f.prefix),
                access_key: t(&f.access_key),
                secret_key: if f.secret_key.trim().is_empty() {
                    old
                } else {
                    t(&f.secret_key)
                },
                hour_utc: f.hour_utc.trim().parse().unwrap_or(3).min(23),
                keep: f.keep.trim().parse().unwrap_or(14).max(1),
                path_style: !f.path_style.is_empty(),
            });
        })
        .await?;
    }
    let b = config::backup_from_env(
        config::get(&st.db)
            .stored
            .backup
            .clone()
            .unwrap_or_default(),
    );
    let result = match action.as_str() {
        "test" => Some(match backup::test_connection(&b).await {
            Ok(()) => (
                true,
                "Connection works: wrote and deleted a test object.".to_string(),
            ),
            Err(e) => (false, format!("Connection failed: {e:#}")),
        }),
        "now" => {
            let (db, started) = (st.db.clone(), crate::db::now());
            let job = tokio::spawn(async move { backup::run_with(&db, &b, started).await });
            Some(
                match tokio::time::timeout(Duration::from_secs(30), job).await {
                    Ok(Ok(Ok(k))) => (true, format!("Backed up to {k}.")),
                    Ok(Ok(Err(e))) => (false, format!("Backup failed: {e:#}")),
                    Ok(Err(e)) => (false, format!("Backup failed: {e}")),
                    Err(_) => (
                        true,
                        "Backup still running; check the log below.".to_string(),
                    ),
                },
            )
        }
        _ if !project.is_empty() => return to(&format!("/projects/{project}/setup-sdk")),
        _ => return flash("/settings/backups", "Backup settings saved"),
    };
    let v = view(&st, project.clone(), result).await?;
    if project.is_empty() {
        render(Page { me, v })
    } else {
        render(Setup { v })
    }
}
