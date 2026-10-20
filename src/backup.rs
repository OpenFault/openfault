//! S3-compatible backups: online SQLite snapshot → zstd → single SigV4 PUT, pruned to `keep`.
//! Daily at `hour_utc` (scheduler), from Settings → Backups, or `openfault backup now`.
//! Every run is a `backups_log` row; failures are logged and shown on the Backups page.
use crate::config::{self, Backup};
use crate::db::{Db, now};
use anyhow::{Context, Result, bail, ensure};
use ring::{digest, hmac};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// S3 single-PUT limit; multipart isn't implemented.
const MAX: u64 = 5 << 30;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .build()
        .expect("http client")
});

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn sha(b: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, b).as_ref())
}
fn mac(k: &[u8], m: &str) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, k), m.as_bytes())
        .as_ref()
        .to_vec()
}

/// SigV4 URI encoding; `/` kept when `slash` (paths), encoded otherwise (query values).
fn enc(s: &str, slash: bool) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b'/' if slash => "/".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// SigV4 `Authorization` header. `path` and `query` are already canonical (encoded, sorted);
/// `headers` are lowercase name + trimmed value and all get signed.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    payload_sha: &str,
    amz_date: &str,
    region: &str,
    service: &str,
    access_key: &str,
    secret_key: &str,
) -> String {
    let mut h = headers.to_vec();
    h.sort();
    let canon: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = h.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let creq = format!("{method}\n{path}\n{query}\n{canon}\n{signed}\n{payload_sha}");
    let day = &amz_date[..8];
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let sts = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha(creq.as_bytes())
    );
    let mut k = mac(format!("AWS4{secret_key}").as_bytes(), day);
    for p in [region, service, "aws4_request"] {
        k = mac(&k, p);
    }
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={}",
        hex(&mac(&k, &sts))
    )
}

/// One signed S3 request; Err on transport errors and non-2xx.
async fn req(
    b: &Backup,
    method: reqwest::Method,
    key: &str,
    query: &[(&str, &str)],
    body: Vec<u8>,
) -> Result<Vec<u8>> {
    let ep = match b.endpoint.trim().trim_end_matches('/') {
        "" => format!("https://s3.{}.amazonaws.com", b.region),
        e => e.to_string(),
    };
    let mut url = reqwest::Url::parse(&ep).context("bad endpoint URL")?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && local),
        "endpoint must be https:// (http only for localhost)"
    );
    let mut path = String::new();
    if b.path_style {
        path = format!("/{}", b.bucket);
    } else {
        let host = format!("{}.{}", b.bucket, url.host_str().unwrap_or_default());
        url.set_host(Some(&host)).context("bad bucket name")?;
    }
    if !key.is_empty() || path.is_empty() {
        path = format!("{path}/{key}");
    }
    let path = enc(&path, true);
    let mut q: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k, false), enc(v, false)))
        .collect();
    q.sort();
    let q = q.join("&");
    url.set_path(&path);
    url.set_query(Some(&q).filter(|q| !q.is_empty()).map(|q| q.as_str()));
    let host = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    let amz = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    let psha = sha(&body);
    let auth = sign(
        method.as_str(),
        &path,
        &q,
        &[
            ("host", &host),
            ("x-amz-content-sha256", &psha),
            ("x-amz-date", &amz),
        ],
        &psha,
        &amz,
        &b.region,
        "s3",
        &b.access_key,
        &b.secret_key,
    );
    let res = HTTP
        .request(method.clone(), url)
        .header("x-amz-date", &amz)
        .header("x-amz-content-sha256", &psha)
        .header(reqwest::header::AUTHORIZATION, auth)
        .body(body)
        .send()
        .await
        .with_context(|| format!("{method} {key}"))?;
    let status = res.status();
    let bytes = res.bytes().await?.to_vec();
    if !status.is_success() {
        let x = String::from_utf8_lossy(&bytes);
        let tag = |t: &str| {
            x.split_once(&format!("<{t}>"))
                .and_then(|(_, r)| r.split_once(&format!("</{t}>")))
                .map(|(v, _)| v.to_string())
        };
        let why = [tag("Code"), tag("Message")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(": ");
        bail!("{method} /{key}: HTTP {} {why}", status.as_u16());
    }
    Ok(bytes)
}

fn prefix(b: &Backup) -> String {
    match b.prefix.trim().trim_matches('/') {
        "" => String::new(),
        p => format!("{p}/"),
    }
}

fn stamp() -> String {
    jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string()
}

/// Consistent copy of the live database, zstd-compressed next to it as `openfault-<utc>.db.zst`.
/// ponytail: deviation from "step the writer connection": a fresh read-only connection copies
/// everything in one step. Under WAL that's one read transaction, so the writer is never blocked
/// and the copy never restarts; stepping on the writer thread would stall ingest per step.
pub fn snapshot(db: &Db) -> Result<PathBuf> {
    let src = Connection::open_with_flags(db.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let dir = db.path().parent().unwrap_or(Path::new("."));
    let name = format!("openfault-{}.db", stamp());
    let raw = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&raw);
    {
        let mut dst = Connection::open(&raw)?;
        private(&raw)?;
        rusqlite::backup::Backup::new(&src, &mut dst)?.run_to_completion(
            i32::MAX,
            Duration::ZERO,
            None,
        )?;
    }
    let out = dir.join(format!("{name}.zst"));
    let r = (|| -> std::io::Result<()> {
        let f = std::fs::File::create(&out)?;
        private(&out)?;
        zstd::stream::copy_encode(std::fs::File::open(&raw)?, f, 3)
    })();
    let _ = std::fs::remove_file(&raw);
    if r.is_err() {
        let _ = std::fs::remove_file(&out);
    }
    r?;
    Ok(out)
}

/// Snapshots hold every secret in the DB: owner-only (0600) before any data is written.
fn private(p: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// PUT `path` as `<prefix><file name>`; returns the key.
pub async fn upload(b: &Backup, path: &Path) -> Result<String> {
    let len = std::fs::metadata(path)?.len();
    ensure!(
        len < MAX,
        "backup is {len} bytes; single PUT uploads are limited to 5 GB"
    );
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("bad file name")?;
    let key = format!("{}{name}", prefix(b));
    req(b, reqwest::Method::PUT, &key, &[], std::fs::read(path)?).await?;
    Ok(key)
}

/// Backup objects under the prefix, oldest first: (key, bytes).
/// ponytail: first page only (1000 keys), far above any sane `keep`.
pub async fn list(b: &Backup) -> Result<Vec<(String, u64)>> {
    let p = prefix(b);
    let body = req(
        b,
        reqwest::Method::GET,
        "",
        &[("list-type", "2"), ("prefix", &p)],
        vec![],
    )
    .await?;
    let x = String::from_utf8_lossy(&body);
    let inner = |s: &str, t: &str| {
        s.split_once(&format!("<{t}>"))
            .and_then(|(_, r)| r.split_once(&format!("</{t}>")))
            .map(|(v, _)| v.to_string())
    };
    let mut out: Vec<(String, u64)> = x
        .split("<Contents>")
        .skip(1)
        .filter_map(|c| {
            let k = inner(c, "Key")?;
            let size = inner(c, "Size").and_then(|s| s.parse().ok()).unwrap_or(0);
            // only our own files directly under the prefix, so prune never touches anything else.
            let name = k.strip_prefix(&p)?;
            (!name.contains('/') && name.starts_with("openfault-") && name.ends_with(".db.zst"))
                .then_some((k, size))
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Deletes all but the newest `keep` (at least 1); returns how many were deleted.
pub async fn prune(b: &Backup, keep: u32) -> Result<usize> {
    let all = list(b).await?;
    let n = all.len().saturating_sub(keep.max(1) as usize);
    for (k, _) in &all[..n] {
        req(b, reqwest::Method::DELETE, k, &[], vec![]).await?;
    }
    Ok(n)
}

/// PUT + DELETE a tiny marker object.
pub async fn test_connection(b: &Backup) -> Result<()> {
    ensure!(!b.bucket.trim().is_empty(), "bucket is not set");
    let key = format!("{}.openfault-test", prefix(b));
    req(b, reqwest::Method::PUT, &key, &[], b"ok".to_vec()).await?;
    req(b, reqwest::Method::DELETE, &key, &[], vec![]).await?;
    Ok(())
}

/// GET `target` (a key, or `s3://bucket/key`) → decompress → integrity check → `dest`.
/// An existing `dest` (and its -wal/-shm) is kept as `<dest>.pre-restore-<utc>`. Server must be stopped.
pub async fn restore(b: &Backup, target: &str, dest: &Path) -> Result<u64> {
    let mut b = b.clone();
    let key = match target.strip_prefix("s3://") {
        Some(r) => {
            let (bucket, k) = r.split_once('/').context("expected s3://bucket/key")?;
            b.bucket = bucket.into();
            k.to_string()
        }
        None => target.to_string(),
    };
    ensure!(!b.bucket.is_empty(), "bucket is not set");
    let body = req(&b, reqwest::Method::GET, &key, &[], vec![]).await?;
    let dest = dest.to_owned();
    tokio::task::spawn_blocking(move || -> Result<u64> {
        let with = |p: &Path, s: &str| PathBuf::from(format!("{}{s}", p.display()));
        let tmp = with(&dest, ".restore");
        zstd::stream::copy_decode(&body[..], std::fs::File::create(&tmp)?)
            .context("not a zstd backup")?;
        let check: String = Connection::open(&tmp)?
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .context("not a SQLite database")?;
        ensure!(check == "ok", "backup failed integrity check: {check}");
        let old = with(&dest, &format!(".pre-restore-{}", stamp()));
        for s in ["", "-wal", "-shm"] {
            if with(&dest, s).exists() {
                std::fs::rename(with(&dest, s), with(&old, s))?;
            }
        }
        std::fs::rename(&tmp, &dest)?;
        Ok(std::fs::metadata(&dest)?.len())
    })
    .await?
}

/// Databases with a run in progress (one per DB file).
static RUNNING: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);
struct Running(PathBuf);
impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.lock().unwrap().remove(&self.0);
    }
}

/// One full run with the effective settings.
pub async fn run(db: &Db) -> Result<String> {
    let b = config::get(db)
        .backup
        .clone()
        .context("backups are not configured")?;
    run_with(db, &b, now()).await
}

/// snapshot → upload → prune, recorded in backups_log (started_at = `started`). Returns the key.
pub async fn run_with(db: &Db, b: &Backup, started: i64) -> Result<String> {
    let p = db.path().to_owned();
    if !RUNNING.lock().unwrap().insert(p.clone()) {
        bail!("a backup is already running");
    }
    let _g = Running(p);
    let id = db
        .write(move |c| {
            c.execute(
                "INSERT INTO backups_log (started_at) VALUES (?1)",
                [started],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await?;
    let r = async {
        let d = db.clone();
        let path = tokio::task::spawn_blocking(move || snapshot(&d)).await??;
        let bytes = std::fs::metadata(&path)?.len() as i64;
        let up = upload(b, &path).await;
        let _ = std::fs::remove_file(&path);
        let key = up?;
        prune(b, b.keep)
            .await
            .with_context(|| format!("uploaded {key}, but pruning failed"))?;
        anyhow::Ok((key, bytes))
    }
    .await;
    let (key, bytes, err) = match &r {
        Ok((k, n)) => (Some(k.clone()), Some(*n), None),
        Err(e) => {
            tracing::error!("backup failed: {e:#}");
            (None, None, Some(format!("{e:#}")))
        }
    };
    db.write(move |c| {
        c.execute(
            "UPDATE backups_log SET finished_at = ?2, key = ?3, bytes = ?4, ok = ?5, error = ?6 WHERE id = ?1",
            rusqlite::params![id, now(), key, bytes, err.is_none(), err],
        )
    })
    .await?;
    r.map(|(k, _)| k)
}

/// Runs a backup when enabled, `t` is inside hour_utc and nothing ran since that day's 00:00 UTC.
/// None = not due.
pub async fn tick(db: &Db, t: i64) -> Option<Result<String>> {
    let b = config::get(db).backup.clone()?;
    if t.rem_euclid(86400) / 3600 != i64::from(b.hour_utc) {
        return None;
    }
    let day = t - t.rem_euclid(86400);
    let ran = db
        .read(move |c| {
            c.query_row(
                "SELECT EXISTS (SELECT 1 FROM backups_log WHERE started_at >= ?1)",
                [day],
                |r| r.get(0),
            )
        })
        .await
        .unwrap_or(true);
    if ran {
        return None;
    }
    Some(run_with(db, &b, t).await)
}

/// Checks once a minute; failures are in backups_log and the log (run_with).
pub async fn scheduler(db: Db) {
    let mut i = tokio::time::interval(Duration::from_secs(60));
    loop {
        i.tick().await;
        tick(&db, now()).await;
    }
}

/// `openfault backup now|list`, `openfault restore <key|s3://bucket/key>`. Returns the exit code.
pub async fn cli(args: &[String], db_path: &Path, server_running: bool) -> i32 {
    let r = async {
        match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            ["restore", target] => {
                ensure!(
                    !server_running,
                    "the server is running; stop it before restoring"
                );
                // env only: the database being replaced may be missing or broken
                let b = config::backup_from_env(Backup::default());
                let n = restore(&b, target, db_path).await?;
                println!("restored {target} to {} ({n} bytes)", db_path.display());
            }
            ["backup", sub @ ("now" | "list")] => {
                let db = Db::open(db_path).map_err(|e| anyhow::anyhow!("{e}"))?;
                let b = config::get(&db).backup.clone().context(
                    "backups are not configured (Settings → Backups or OPENFAULT_BACKUP_S3_*)",
                )?;
                if sub == "now" {
                    println!("{}", run_with(&db, &b, now()).await?);
                } else {
                    for (k, n) in list(&b).await? {
                        println!("{k}\t{n}");
                    }
                }
            }
            _ => bail!("usage: openfault backup now | backup list | restore <key|s3://bucket/key>"),
        }
        Ok(())
    }
    .await;
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    }
}
