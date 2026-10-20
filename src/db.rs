use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;
const READERS: usize = 3;

/// One writer thread (owns the only write connection) + a small pool of read-only connections.
#[derive(Clone)]
pub struct Db {
    tx: mpsc::Sender<Job>,
    readers: Arc<Mutex<Vec<Connection>>>,
    path: Arc<std::path::PathBuf>,
    /// deviation: live-update channel lives on Db (not just AppState) so `process::run` and `apply_action`, which only get a Db, can publish.
    pub live: tokio::sync::broadcast::Sender<crate::live::LiveEvent>,
}

#[derive(Debug)]
pub struct DbError(pub String);
impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError(e.to_string())
    }
}
impl std::error::Error for DbError {}
pub type Result<T> = std::result::Result<T, DbError>;

fn pragmas(c: &Connection, cache_kb: i64) -> rusqlite::Result<()> {
    c.execute_batch(&format!(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;
         PRAGMA foreign_keys=ON; PRAGMA cache_size=-{cache_kb}; PRAGMA mmap_size=0;"
    ))
}

fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    pragmas(&c, 8000)?;
    Ok(c)
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let mut w = Connection::open(path)?;
        // auto_vacuum must be set before the first table exists; no-op afterwards.
        w.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        pragmas(&w, 16000)?;
        let check: String = w.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(DbError(format!(
                "integrity check failed ({check}); restore from backup"
            )));
        }
        w.execute_batch(include_str!("schema.sql"))?;
        let readers = (0..READERS)
            .map(|_| open_reader(path))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // deviation: tokio bounded mpsc instead of std::sync_channel so a full queue awaits instead of blocking a runtime worker.
        let (tx, mut rx) = mpsc::channel::<Job>(256);
        std::thread::Builder::new()
            .name("sqlite-writer".into())
            .spawn(move || {
                while let Some(job) = rx.blocking_recv() {
                    job(&mut w);
                }
            })
            .map_err(|e| DbError(e.to_string()))?;
        Ok(Db {
            tx,
            readers: Arc::new(Mutex::new(readers)),
            path: Arc::new(path.to_owned()),
            live: tokio::sync::broadcast::channel(1024).0,
        })
    }

    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (otx, orx) = oneshot::channel();
        let job: Job = Box::new(move |c| {
            let _ = otx.send(f(c));
        });
        self.tx
            .send(job)
            .await
            .map_err(|_| DbError("writer gone".into()))?;
        Ok(orx
            .await
            .map_err(|_| DbError("writer dropped job".into()))??)
    }

    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let pool = self.readers.clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let popped = pool.lock().unwrap().pop();
            // ponytail: pool exhausted → open a throwaway reader; a Condvar wait is the upgrade if this shows up in profiles.
            let c = match popped {
                Some(c) => c,
                None => open_reader(&path)?,
            };
            let r = f(&c);
            let mut p = pool.lock().unwrap();
            if p.len() < READERS {
                p.push(c);
            }
            Ok(r?)
        })
        .await
        .map_err(|e| DbError(e.to_string()))?
    }
}

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// ponytail: dev/test seed until the M2 setup wizard exists. Creates org 1 + project 1 "dev" if no projects exist.
pub fn seed_dev(c: &mut Connection, key: &str) -> rusqlite::Result<()> {
    let n: i64 = c.query_row("SELECT count(*) FROM projects", [], |r| r.get(0))?;
    if n > 0 {
        return Ok(());
    }
    let t = now();
    let tx = c.transaction()?;
    tx.execute(
        "INSERT INTO orgs (id, name, slug, created_at) VALUES (1, 'Dev', 'dev', ?1)",
        [t],
    )?;
    tx.execute(
        "INSERT INTO projects (id, org_id, slug, name, created_at) VALUES (1, 1, 'dev', 'dev', ?1)",
        [t],
    )?;
    tx.execute(
        "INSERT INTO dsn_keys (project_id, public_key, created_at) VALUES (1, ?1, ?2)",
        rusqlite::params![key, t],
    )?;
    tx.commit()
}
