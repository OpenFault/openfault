//! PLAN §13b crash safety: SIGKILL mid-ingest, restart, zero accepted events lost or duplicated.
mod common;
use common::*;
use std::collections::HashSet;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering::SeqCst},
};

const TOTAL: usize = 300;
const KILL_AT: usize = 100;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigkill_mid_ingest_loses_nothing() {
    for round in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        seed_dir(dir.path(), &[]).await;
        let (mut child, base) = spawn_bin(dir.path()).await;
        let ids: Arc<Vec<String>> = Arc::new(
            (0..TOTAL)
                .map(|_| uuid::Uuid::now_v7().simple().to_string())
                .collect(),
        );
        let next = Arc::new(AtomicUsize::new(0));
        // ok: got 200. maybe: sent but no answer (the kill raced it), so it may or may not be stored.
        let ok = Arc::new(Mutex::new(HashSet::new()));
        let maybe = Arc::new(Mutex::new(HashSet::new()));
        let http = reqwest::Client::new();
        let url = format!("{base}/api/1/envelope/?sentry_key={KEY}");
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (ids, next, ok, maybe, http, url) = (
                    ids.clone(),
                    next.clone(),
                    ok.clone(),
                    maybe.clone(),
                    http.clone(),
                    url.clone(),
                );
                tokio::spawn(async move {
                    loop {
                        let i = next.fetch_add(1, SeqCst);
                        let Some(id) = ids.get(i) else { return };
                        match http.post(&url).body(envelope_body(id)).send().await {
                            Ok(r) if r.status() == 200 => {
                                ok.lock().unwrap().insert(id.clone());
                            }
                            Ok(r) => panic!("unexpected {}", r.status()),
                            Err(_) => {
                                maybe.lock().unwrap().insert(id.clone());
                                return; // server is gone
                            }
                        }
                    }
                })
            })
            .collect();
        while ok.lock().unwrap().len() < KILL_AT {
            tokio::task::yield_now().await;
        }
        child.kill().unwrap(); // SIGKILL on unix
        child.wait().unwrap();
        for w in workers {
            w.await.unwrap();
        }
        let ok = ok.lock().unwrap().clone();
        let maybe = maybe.lock().unwrap().clone();

        let (mut child, _) = spawn_bin(dir.path()).await;
        wait_inbox_empty(dir.path(), std::time::Duration::from_secs(30)).await;
        let stored: HashSet<String> = rusqlite::Connection::open(dir.path().join("openfault.db"))
            .unwrap()
            .prepare("SELECT id FROM events")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let issues = sqlite_count(dir.path(), "SELECT count(*) FROM issues");
        child.kill().unwrap();
        child.wait().unwrap();

        let lost: Vec<_> = ok.difference(&stored).collect();
        assert!(
            lost.is_empty(),
            "round {round}: lost {} accepted events",
            lost.len()
        );
        assert!(
            stored
                .iter()
                .all(|id| ok.contains(id) || maybe.contains(id))
        );
        assert_eq!(issues as usize, stored.len(), "one issue per unique event");
        eprintln!(
            "round {round}: 200s={} stored={} (unacked-but-stored {})",
            ok.len(),
            stored.len(),
            stored.len() - ok.len()
        );
    }
}
