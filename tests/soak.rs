//! Compressed soak (PLAN §13b): 72 simulated hours in ~13 min against the release binary.
//! `OPENFAULT_TIME_OFFSET_FILE` fast-forwards `db::now()` by 1h every ~10s real; housekeeping keys off now().
//! Ignored by default: `cargo test --release --test soak -- --ignored --nocapture`.
mod common;
use common::*;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering::SeqCst},
};
use std::time::{Duration, Instant};

const HOURS: i64 = 72;
const PER_HOUR: usize = 200;
const PORT: u16 = 18201;
const TOKEN: &str = "of_soak_admin";

fn rss_kb(pid: u32) -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn fsize(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn real_now() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn exec(dir: &Path, sql: &str) {
    rusqlite::Connection::open(dir.join("openfault.db"))
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

/// Small event on a recurring issue (`fp`), or a real 7 KB node fixture.
fn body(fixture: &str, fp: &str) -> String {
    let id = uuid::Uuid::now_v7().simple().to_string();
    if fp.is_empty() {
        let old = serde_json::from_str::<serde_json::Value>(fixture.lines().next().unwrap())
            .unwrap()["event_id"]
            .as_str()
            .unwrap()
            .to_string();
        return fixture.replace(&old, &id);
    }
    format!(
        "{{\"event_id\":\"{id}\"}}\n{{\"type\":\"event\"}}\n{{\"event_id\":\"{id}\",\"platform\":\"node\",\"level\":\"error\",\"message\":\"boom {fp}\",\"fingerprint\":[\"{fp}\"]}}\n"
    )
}

/// One SSE client: reconnects on drop; `cycle` forces a drop every 10s to exercise reconnects.
fn sse(
    base: String,
    cycle: bool,
    done: Arc<AtomicBool>,
    connects: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        while !done.load(SeqCst) {
            let r = http
                .get(format!("{base}/live?scope=all"))
                .bearer_auth(TOKEN)
                .send()
                .await;
            if let Ok(mut r) = r
                && r.status() == 200
            {
                connects.fetch_add(1, SeqCst);
                let read = async {
                    while let Ok(Some(c)) = r.chunk().await {
                        bytes.fetch_add(c.len() as u64, SeqCst);
                        if done.load(SeqCst) {
                            return;
                        }
                    }
                };
                let limit = if cycle { 10 } else { 100_000 };
                let _ = tokio::time::timeout(Duration::from_secs(limit), read).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
}

fn issue_id(dir: &Path, fp: &str) -> i64 {
    sqlite_count(
        dir,
        &format!("SELECT id FROM issues WHERE title LIKE '%boom {fp}%' LIMIT 1"),
    )
}

async fn act(http: &reqwest::Client, base: &str, id: i64, body: serde_json::Value) {
    let r = http
        .post(format!("{base}/api/v1/issues/{id}"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "act {id}: {}", r.status());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn soak_72h() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (db, wal, offset, log) = (
        dir.join("openfault.db"),
        dir.join("openfault.db-wal"),
        dir.join("offset"),
        dir.join("server.log"),
    );
    std::fs::write(&offset, "0").unwrap();
    seed_dir(dir, &[]).await;
    let t0s = real_now();
    exec(
        dir,
        &format!(
            "UPDATE projects SET retention_days = 1;
             INSERT INTO users (id, email, name, pw_hash, role, created_at) VALUES (1, 'a@x.io', 'Ada', 'x', 'admin', 0);
             INSERT INTO api_tokens (token_hash, user_id, scopes) VALUES ('{}', 1, 'write');
             INSERT INTO sessions (id_hash, user_id, expires_at) VALUES ('short', 1, {}), ('long', 1, {});
             INSERT INTO password_resets (token_hash, user_id, expires_at) VALUES ('r', 1, {});
             INSERT INTO chunks (sha1, data, created_at) VALUES ('c', x'00', {t0s});",
            openfault::auth::hash_token(TOKEN),
            t0s + 12 * 3600,
            t0s + 100 * 86400,
            t0s + 3600,
        ),
    );
    let size0 = fsize(&db) + fsize(&wal);

    let logf = std::fs::File::create(&log).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_openfault"))
        .env("OPENFAULT_DATA", dir)
        .env("OPENFAULT_PORT", PORT.to_string())
        .env("OPENFAULT_TIME_OFFSET_FILE", &offset)
        .env("RUST_LOG", "info")
        .stdout(logf.try_clone().unwrap())
        .stderr(logf)
        .spawn()
        .unwrap();
    let pid = child.id();
    let base = format!("http://127.0.0.1:{PORT}");
    let mut up = false;
    for _ in 0..200 {
        if reqwest::get(format!("{base}/healthz")).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(up, "binary did not start");

    let done = Arc::new(AtomicBool::new(false));
    let (peak_rss, peak_wal) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let sampler = {
        let (done, peak_rss, peak_wal, wal) = (
            done.clone(),
            peak_rss.clone(),
            peak_wal.clone(),
            wal.clone(),
        );
        tokio::spawn(async move {
            while !done.load(SeqCst) {
                peak_rss.fetch_max(rss_kb(pid), SeqCst);
                peak_wal.fetch_max(fsize(&wal), SeqCst);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
    };
    let stat = || Arc::new(AtomicU64::new(0));
    let (ca, ba, cb, bb) = (stat(), stat(), stat(), stat());
    let sse_a = sse(base.clone(), false, done.clone(), ca.clone(), ba.clone());
    let sse_b = sse(base.clone(), true, done.clone(), cb.clone(), bb.clone());

    let fixture = std::fs::read_to_string(format!(
        "{}/tests/fixtures/real/node_exception.envelope",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let http = reqwest::Client::new();
    let (mut ok, mut limited) = (0u64, 0u64);
    let mut sizes = vec![];
    let mut muted = 0;
    let start = Instant::now();
    for h in 0..HOURS {
        let hour_start = Instant::now();
        std::fs::write(&offset, (h * 3600).to_string()).unwrap();
        for i in 0..PER_HOUR {
            // 10% real fixture, 10% fresh issues, 80% across 10 recurring issues.
            let fp = match i % 10 {
                0 => String::new(),
                1 => format!("u{h}-{i}"),
                _ => format!("r{}", i % 10),
            };
            let r = http
                .post(format!("{base}/api/1/envelope/?sentry_key={KEY}"))
                .body(body(&fixture, &fp))
                .send()
                .await
                .unwrap();
            match r.status().as_u16() {
                200 => ok += 1,
                429 => limited += 1,
                s => panic!("hour {h}: unexpected {s}"),
            }
        }
        wait_inbox_empty(dir, Duration::from_secs(30)).await;
        match h {
            // resolve recurring issues → regressions on the next hour's events.
            10 | 40 => {
                for fp in ["r2", "r3"] {
                    act(
                        &http,
                        &base,
                        issue_id(dir, fp),
                        serde_json::json!({"action": "resolve"}),
                    )
                    .await;
                }
            }
            // mute an issue that never recurs, for 2 simulated hours → housekeeping must unmute it.
            20 => {
                muted = issue_id(dir, "u19-1");
                let until = real_now() + h * 3600 + 2 * 3600;
                act(
                    &http,
                    &base,
                    muted,
                    serde_json::json!({"action": "mute", "until": until}),
                )
                .await;
            }
            _ => {}
        }
        sizes.push(fsize(&db) + fsize(&wal));
        if let Some(rest) = Duration::from_secs(10).checked_sub(hour_start.elapsed()) {
            tokio::time::sleep(rest).await;
        }
    }
    // jump 2 more hours and give housekeeping (60s tick) + notify purge (60s) one pass at the final time.
    let end_off = (HOURS + 1) * 3600;
    std::fs::write(&offset, end_off.to_string()).unwrap();
    let passes = |l: &Path| {
        std::fs::read_to_string(l)
            .unwrap()
            .matches("housekeeping done")
            .count()
    };
    let before = passes(&log);
    tokio::time::sleep(Duration::from_secs(65)).await;
    let after = passes(&log);
    done.store(true, SeqCst);
    sampler.await.unwrap();
    sse_a.abort();
    sse_b.abort();
    let wall = start.elapsed();

    let now = real_now() + end_off;
    let cutoff = now - 86400 - 120;
    let n = |sql: &str| sqlite_count(dir, sql);
    let events = n("SELECT count(*) FROM events");
    let old_events = n(&format!(
        "SELECT count(*) FROM events WHERE received_at < {cutoff}"
    ));
    let size_pre_ckpt = (fsize(&db), fsize(&wal));
    exec(dir, "PRAGMA wal_checkpoint(TRUNCATE);");
    let wal_after = fsize(&wal);
    let pages = (n("PRAGMA page_count"), n("PRAGMA freelist_count"));
    let logtxt = std::fs::read_to_string(&log).unwrap();
    let errors: Vec<&str> = logtxt
        .lines()
        .filter(|l| l.contains("ERROR") || l.contains("panicked"))
        .collect();
    let rss = peak_rss.load(SeqCst) / 1024;
    let mb = |b: u64| b as f64 / 1_048_576.0;
    eprintln!(
        "soak: {HOURS} sim hours in {wall:?}; posts 200={ok} 429={limited}; events retained={events} (old={old_events}); \
         housekeeping passes={} (+{} at end); rss_peak={rss} MB; wal_peak={:.1} MB, wal_end={:.1} MB, wal_after_ckpt={wal_after} B; \
         db start={:.2} MB h24={:.2} h48={:.2} h72={:.2} end db={:.2} MB (pages={} free={}); bytes/retained event={}; \
         sse A connects={} bytes={}, B connects={} bytes={}; errors={}",
        after,
        after - before,
        mb(peak_wal.load(SeqCst)),
        mb(size_pre_ckpt.1),
        mb(size0),
        mb(sizes[23]),
        mb(sizes[47]),
        mb(sizes[71]),
        mb(size_pre_ckpt.0),
        pages.0,
        pages.1,
        size_pre_ckpt.0 / events.max(1) as u64,
        ca.load(SeqCst),
        ba.load(SeqCst),
        cb.load(SeqCst),
        bb.load(SeqCst),
        errors.len(),
    );
    for e in errors.iter().take(10) {
        eprintln!("  {e}");
    }
    let dump = |sql: &str| n(sql);
    let checks = [
        ("no events older than retention", old_events == 0),
        (
            // final clock is sim hour 73, last posts at hour 71 → hours 50..71 survive the 24h cutoff.
            "last ~22-24 sim hours of events retained",
            events >= 22 * PER_HOUR as i64 && events <= 24 * PER_HOUR as i64,
        ),
        ("housekeeping ran at the final time", after > before),
        ("WAL < 10 MB after checkpoint", wal_after < 10 << 20),
        ("WAL peak < 64 MB", peak_wal.load(SeqCst) < 64 << 20),
        (
            "DB stays bounded once retention kicks in",
            size_pre_ckpt.0 < sizes[23].max(1) * 2,
        ),
        ("RSS < 100 MB", rss < 100),
        ("no ERROR/panicked log lines", errors.is_empty()),
        ("inbox empty", dump("SELECT count(*) FROM inbox") == 0),
        (
            "notif_queue drained",
            dump("SELECT count(*) FROM notif_queue WHERE done_at IS NULL") == 0,
        ),
        (
            "notif_queue purged >24h",
            dump(&format!(
                "SELECT count(*) FROM notif_queue WHERE created_at < {cutoff}"
            )) == 0,
        ),
        (
            "notif_sent purged >24h",
            dump(&format!(
                "SELECT count(*) FROM notif_sent WHERE sent_at < {cutoff}"
            )) == 0,
        ),
        (
            "project_5min only last 24h",
            dump(&format!(
                "SELECT count(*) FROM project_5min WHERE bucket_ts < {cutoff}"
            )) == 0,
        ),
        (
            "seen_events purge ran (≤ total)",
            dump("SELECT count(*) FROM seen_events") <= (ok as i64),
        ),
        (
            "expired session gone",
            dump("SELECT count(*) FROM sessions WHERE id_hash = 'short'") == 0,
        ),
        (
            "live session kept",
            dump("SELECT count(*) FROM sessions WHERE id_hash = 'long'") == 1,
        ),
        (
            "expired reset gone",
            dump("SELECT count(*) FROM password_resets") == 0,
        ),
        ("stale chunk gone", dump("SELECT count(*) FROM chunks") == 0),
        (
            "mute expired without events",
            dump(&format!(
                "SELECT count(*) FROM issues WHERE id = {muted} AND status = 'unresolved'"
            )) == 1,
        ),
        (
            "unmuted_expired activity",
            dump(&format!(
                "SELECT count(*) FROM issue_activity WHERE issue_id = {muted} AND kind = 'unmuted_expired'"
            )) == 1,
        ),
        (
            "SSE A stayed up (≤ 2 connects)",
            ca.load(SeqCst) >= 1 && ca.load(SeqCst) <= 2,
        ),
        ("SSE B reconnected every cycle", cb.load(SeqCst) >= 60),
        (
            "SSE both received data",
            ba.load(SeqCst) > 0 && bb.load(SeqCst) > 0,
        ),
    ];
    child.kill().unwrap();
    child.wait().unwrap();
    let failed: Vec<_> = checks.iter().filter(|c| !c.1).map(|c| c.0).collect();
    assert!(
        failed.is_empty(),
        "soak checks failed: {failed:?} (log: {})",
        tmp.keep().join("server.log").display()
    );
}
