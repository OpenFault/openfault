//! PLAN §13b load check: 10k-event burst across 3 projects stays under 100 MB RSS and drains.
//! Ignored by default: `cargo test --release -- --ignored load`.
mod common;
use common::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst},
};
use std::time::{Duration, Instant};

const TOTAL: usize = 10_000;
const KEY3: &str = "33333333333333333333333333333333";

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn load_10k_burst() {
    let dir = tempfile::tempdir().unwrap();
    seed_dir(dir.path(), &[(3, KEY3)]).await;
    let (mut child, base) = spawn_bin(dir.path()).await;
    let pid = child.id();
    let peak = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (peak.clone(), done.clone());
        tokio::spawn(async move {
            while !done.load(SeqCst) {
                peak.fetch_max(rss_kb(pid), SeqCst);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
    };
    let next = Arc::new(AtomicUsize::new(0));
    let (ok, limited) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let http = reqwest::Client::new();
    let t0 = Instant::now();
    let workers: Vec<_> = (0..16)
        .map(|_| {
            let (next, ok, limited, http, base) = (
                next.clone(),
                ok.clone(),
                limited.clone(),
                http.clone(),
                base.clone(),
            );
            tokio::spawn(async move {
                loop {
                    let i = next.fetch_add(1, SeqCst);
                    if i >= TOTAL {
                        return;
                    }
                    let (pid, key) = [(1, KEY), (2, KEY2), (3, KEY3)][i % 3];
                    let id = uuid::Uuid::now_v7().simple().to_string();
                    let r = http
                        .post(format!("{base}/api/{pid}/envelope/?sentry_key={key}"))
                        .body(envelope_body(&id))
                        .send()
                        .await
                        .unwrap();
                    match r.status().as_u16() {
                        200 => ok.fetch_add(1, SeqCst),
                        429 => limited.fetch_add(1, SeqCst),
                        s => panic!("unexpected {s}"),
                    };
                }
            })
        })
        .collect();
    for w in workers {
        w.await.unwrap();
    }
    let post_time = t0.elapsed();
    let drain = wait_inbox_empty(dir.path(), Duration::from_secs(60)).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    done.store(true, SeqCst);
    sampler.await.unwrap();
    let stored = sqlite_count(dir.path(), "SELECT count(*) FROM events");
    child.kill().unwrap();
    child.wait().unwrap();
    let (ok, limited, peak_mb) = (
        ok.load(SeqCst),
        limited.load(SeqCst),
        peak.load(SeqCst) / 1024,
    );
    eprintln!(
        "load: {TOTAL} posts in {post_time:?} ({:.0} req/s), 200={ok} 429={limited}, stored={stored}, drain={drain:?}, rss_peak={peak_mb} MB",
        TOTAL as f64 / post_time.as_secs_f64()
    );
    assert_eq!(ok + limited, TOTAL);
    assert_eq!(stored as usize, ok);
    assert!(peak_mb < 100, "RSS peak {peak_mb} MB");
}
