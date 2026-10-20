//! S3 backups against an in-test S3 stub: snapshot → upload → list → prune → restore, scheduler, SigV4.
use axum::{
    Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use openfault::{backup, config::Backup, db::Db};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Store = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

async fn s3(
    State(s): State<Store>,
    m: Method,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
    h: HeaderMap,
    body: Bytes,
) -> Response {
    let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    let sha: String = ring::digest::digest(&ring::digest::SHA256, &body)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !hv("authorization").starts_with("AWS4-HMAC-SHA256 Credential=AK/")
        || hv("x-amz-content-sha256") != sha
        || hv("x-amz-date").len() != 16
    {
        return (StatusCode::BAD_REQUEST, "unsigned").into_response();
    }
    let path = uri.path().trim_start_matches('/');
    let (bucket, key) = path.split_once('/').unwrap_or((path, ""));
    if bucket == "denied" {
        let x = "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
        return (StatusCode::FORBIDDEN, x).into_response();
    }
    let id = format!("{bucket}/{key}");
    let mut st = s.lock().unwrap();
    match m {
        Method::PUT => {
            st.insert(id, body.to_vec());
            StatusCode::OK.into_response()
        }
        Method::DELETE => {
            st.remove(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        Method::GET if key.is_empty() => {
            assert_eq!(q.get("list-type").map(String::as_str), Some("2"));
            let p = format!("{bucket}/{}", q.get("prefix").cloned().unwrap_or_default());
            let items: String = st
                .iter()
                .filter(|(k, _)| k.starts_with(&p))
                .map(|(k, v)| {
                    format!(
                        "<Contents><Key>{}</Key><Size>{}</Size></Contents>",
                        &k[bucket.len() + 1..],
                        v.len()
                    )
                })
                .collect();
            format!("<ListBucketResult>{items}</ListBucketResult>").into_response()
        }
        Method::GET => match st.get(&id) {
            Some(v) => v.clone().into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn stub() -> (Store, String) {
    let s: Store = Default::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = Router::new().fallback(s3).with_state(s.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (s, format!("http://{addr}"))
}

fn cfg(endpoint: &str) -> Backup {
    Backup {
        enabled: true,
        endpoint: endpoint.into(),
        region: "auto".into(),
        bucket: "b".into(),
        prefix: "of".into(),
        access_key: "AK".into(),
        secret_key: "SK".into(),
        hour_utc: 3,
        keep: 2,
        path_style: true,
    }
}

async fn seeded() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("openfault.db")).unwrap();
    db.write(|c| {
        openfault::db::seed_dev(c, "abcdef0123456789abcdef0123456789")?;
        for i in 0..50 {
            c.execute(
                "INSERT INTO users (email, name, pw_hash, role, created_at) VALUES (?1, 'u', 'x', 'member', 0)",
                [format!("u{i}@x.io")],
            )?;
        }
        Ok(())
    })
    .await
    .unwrap();
    (db, dir)
}

#[tokio::test]
async fn snapshot_upload_list_prune_restore() {
    let (store, ep) = stub().await;
    let b = cfg(&ep);
    let (db, dir) = seeded().await;
    let snap = backup::snapshot(&db).unwrap();
    let name = snap.file_name().unwrap().to_str().unwrap().to_string();
    assert!(name.starts_with("openfault-") && name.ends_with(".db.zst"));
    // two older copies so prune has something to drop
    for old in [
        "openfault-20200101T000000Z.db.zst",
        "openfault-20210101T000000Z.db.zst",
    ] {
        let p = dir.path().join(old);
        std::fs::copy(&snap, &p).unwrap();
        assert_eq!(backup::upload(&b, &p).await.unwrap(), format!("of/{old}"));
    }
    let key = backup::upload(&b, &snap).await.unwrap();
    assert_eq!(key, format!("of/{name}"));
    let l = backup::list(&b).await.unwrap();
    assert_eq!(l.len(), 3);
    assert_eq!(l[2].0, key, "newest last");
    assert_eq!(backup::prune(&b, 2).await.unwrap(), 1);
    let l = backup::list(&b).await.unwrap();
    assert_eq!(l.len(), 2);
    assert!(
        !store
            .lock()
            .unwrap()
            .contains_key("b/of/openfault-20200101T000000Z.db.zst")
    );

    // restore over an existing file: old one kept aside, rows match
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("openfault.db");
    std::fs::write(&dest, b"old").unwrap();
    backup::restore(&b, &format!("s3://b/{key}"), &dest)
        .await
        .unwrap();
    let n = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))
            .unwrap()
    };
    let got = n(&rusqlite::Connection::open(&dest).unwrap());
    let want = db.read(move |c| Ok(n(c))).await.unwrap();
    assert_eq!((got, want), (50, 50));
    assert!(std::fs::read_dir(out.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("openfault.db.pre-restore-")
    }));

    // full run: logged, prunes back to keep
    let k = backup::run_with(&db, &b, openfault::db::now())
        .await
        .unwrap();
    assert!(k.starts_with("of/openfault-"));
    let (ok, bytes): (bool, i64) = db
        .read(|c| {
            c.query_row("SELECT ok, bytes FROM backups_log", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
        })
        .await
        .unwrap();
    assert!(ok && bytes > 0);
    assert_eq!(backup::list(&b).await.unwrap().len(), 2);
}

#[tokio::test]
async fn scheduler_runs_once_at_the_hour() {
    let (_store, ep) = stub().await;
    let (db, _dir) = seeded().await;
    let b = cfg(&ep);
    openfault::config::update(&db, move |s| s.backup = Some(b))
        .await
        .unwrap();
    let now = openfault::db::now();
    let day = now - now.rem_euclid(86400);
    assert!(
        backup::tick(&db, day + 2 * 3600).await.is_none(),
        "wrong hour"
    );
    let r = backup::tick(&db, day + 3 * 3600 + 60).await.expect("due");
    assert!(r.is_ok(), "{r:?}");
    assert!(
        backup::tick(&db, day + 3 * 3600 + 120).await.is_none(),
        "once a day"
    );
    assert!(
        backup::tick(&db, day + 86400 + 3 * 3600).await.is_some(),
        "next day"
    );
    let n: i64 = db
        .read(|c| {
            c.query_row("SELECT count(*) FROM backups_log WHERE ok = 1", [], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn failed_runs_are_logged() {
    let (_store, ep) = stub().await;
    let (db, _dir) = seeded().await;
    let mut b = cfg(&ep);
    b.bucket = "denied".into();
    let e = backup::run_with(&db, &b, 0).await.unwrap_err();
    assert!(format!("{e:#}").contains("AccessDenied"), "{e:#}");
    let err: String = db
        .read(|c| {
            c.query_row("SELECT error FROM backups_log WHERE ok = 0", [], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert!(err.contains("HTTP 403"));
}

#[tokio::test]
async fn test_connection_ok_and_err() {
    let (store, ep) = stub().await;
    let mut b = cfg(&ep);
    backup::test_connection(&b).await.unwrap();
    assert!(store.lock().unwrap().is_empty(), "marker deleted");
    b.bucket = "denied".into();
    let e = backup::test_connection(&b).await.unwrap_err().to_string();
    assert!(e.contains("403") && e.contains("AccessDenied"), "{e}");
    b.endpoint = "http://127.0.0.1:1".into();
    assert!(backup::test_connection(&b).await.is_err());
}

/// AWS SigV4 test suite `get-vanilla`.
#[test]
fn sigv4_get_vanilla() {
    let a = backup::sign(
        "GET",
        "/",
        "",
        &[
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ],
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "20150830T123600Z",
        "us-east-1",
        "service",
        "AKIDEXAMPLE",
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
    );
    assert_eq!(
        a,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
    );
}

#[tokio::test]
async fn prune_stays_in_prefix_and_endpoint_must_be_https() {
    let (store, ep) = stub().await;
    let b = cfg(&ep);
    for k in [
        "b/of/openfault-20200101T000000Z.db.zst",
        "b/of/openfault-20200102T000000Z.db.zst",
        "b/of/openfault-20200103T000000Z.db.zst",
        "b/of/nested/openfault-20190101T000000Z.db.zst",
        "b/other/openfault-20190101T000000Z.db.zst",
    ] {
        store.lock().unwrap().insert(k.into(), b"x".to_vec());
    }
    assert_eq!(backup::prune(&b, 1).await.unwrap(), 2);
    let left: Vec<String> = store.lock().unwrap().keys().cloned().collect();
    assert!(left.contains(&"b/of/nested/openfault-20190101T000000Z.db.zst".to_string()));
    assert!(left.contains(&"b/other/openfault-20190101T000000Z.db.zst".to_string()));
    let mut bad = cfg("http://example.com");
    let e = backup::test_connection(&bad).await.unwrap_err().to_string();
    assert!(e.contains("https"), "{e}");
    bad.endpoint = "ftp://localhost".into();
    assert!(backup::test_connection(&bad).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (db, _dir) = seeded().await;
    let p = backup::snapshot(&db).unwrap();
    assert_eq!(
        std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
