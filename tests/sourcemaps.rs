//! sentry-cli contract: chunk-upload + artifactbundle/assemble, then a minified JS event resolves to src/app.js.
//! Fixture: esbuild --minify --sourcemap + `sentry-cli sourcemaps inject` (debug id below).
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use openfault::{
    AppState, app,
    db::{Db, seed_dev},
    process,
};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::io::Write;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";
const TOKEN: &str = "of_0000000000000000000000000000000000000000000000000000000000000000";
const MEMBER_TOKEN: &str = "of_1111111111111111111111111111111111111111111111111111111111111111";
const DEBUG_ID: &str = "63d5bc96-4ecf-5ab3-ad9c-2fa6c05077c2";
const ORG: &str = "/api/0/organizations/openfault";

fn fixture(n: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/sourcemaps/{n}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn hex(b: &[u8]) -> String {
    Sha1::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    ct: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    call_as(app, TOKEN, method, uri, ct, body).await
}

async fn call_as(
    app: &Router,
    token: &str,
    method: &str,
    uri: &str,
    ct: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let s = res.status();
    let b = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

/// Layout captured from a real sentry-cli 3.8 upload (header case varied on purpose).
fn bundle() -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
    let opt = zip::write::SimpleFileOptions::default();
    let manifest = json!({
        "files": {
            "files/_/_/app.js": {"url": "~/app.js", "type": "minified_source",
                "headers": {"debug-id": DEBUG_ID, "sourcemap": "app.js.map"}},
            "files/_/_/app.js.map": {"url": "~/app.js.map", "type": "source_map",
                "headers": {"Debug-Id": DEBUG_ID}},
        },
        "debug_id": "ec9b9954-ae61-5bfa-beca-50a688bb5465", "org": "openfault", "project": "dev",
    });
    for (name, data) in [
        ("manifest.json", serde_json::to_vec(&manifest).unwrap()),
        ("files/_/_/app.js", fixture("app.js")),
        ("files/_/_/app.js.map", fixture("app.js.map")),
    ] {
        z.start_file(name, opt).unwrap();
        z.write_all(&data).unwrap();
    }
    z.finish().unwrap().into_inner()
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_then_symbolicate() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    db.write(|c| {
        seed_dev(c, KEY)?;
        c.execute(
            "INSERT INTO users (id, email, pw_hash, role, created_at) VALUES (1, 'a@b.c', 'x', 'admin', 0), (2, 'm@b.c', 'x', 'member', 0)",
            [],
        )?;
        for (t, uid) in [(TOKEN, 1), (MEMBER_TOKEN, 2)] {
            c.execute(
                "INSERT INTO api_tokens (token_hash, user_id, scopes) VALUES (?1, ?2, 'write')",
                rusqlite::params![blake3::hash(t.as_bytes()).to_hex().to_string(), uid],
            )?;
        }
        Ok(())
    })
    .await
    .unwrap();
    let st = AppState::new(db.clone());
    tokio::spawn(process::run(db.clone(), st.notify.clone()));
    let app = app(st);

    // dSYM upload (legacy sentry-cli DIF path / curl -F); symbolication itself is unit-tested in process::symbolicate.
    let mut mp = b"--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"crash.dSYM.zip\"\r\n\r\n".to_vec();
    mp.extend(
        std::fs::read(format!(
            "{}/tests/fixtures/crash.dSYM.zip",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    );
    mp.extend(b"\r\n--XX--\r\n");
    let dsyms = "/api/0/projects/openfault/dev/files/dsyms/";
    let ct = "multipart/form-data; boundary=XX";
    let (s, j) = call(&app, "POST", dsyms, ct, mp.clone()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(j[0]["debugId"], "353abed5-8469-3171-a13c-b81f142cf274");
    // a member outside the project cannot write to it.
    let (s, _) = call_as(&app, MEMBER_TOKEN, "POST", dsyms, ct, mp).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, j) = call(&app, "GET", &format!("{ORG}/chunk-upload/"), "", vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(j["hashAlgorithm"], "sha1");
    assert_eq!(j["url"], "/organizations/openfault/chunk-upload/");

    let zip = bundle();
    let sum = hex(&zip);
    let req = serde_json::to_vec(&json!({"checksum": sum, "chunks": [sum], "projects": ["dev"]}))
        .unwrap();
    let assemble = format!("{ORG}/artifactbundle/assemble/");
    let (_, j) = call(&app, "POST", &assemble, "application/json", req.clone()).await;
    assert_eq!(j, json!({"state": "not_found", "missingChunks": [sum]}));

    let mut mp = format!(
        "--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{sum}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    mp.extend(&zip);
    mp.extend(b"\r\n--XX--\r\n");
    let (s, _) = call(
        &app,
        "POST",
        &format!("{ORG}/chunk-upload/"),
        "multipart/form-data; boundary=XX",
        mp,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // unknown slugs are rejected, not silently written to the first project.
    let bad = serde_json::to_vec(&json!({"checksum": sum, "chunks": [sum], "projects": ["nope"]}))
        .unwrap();
    let (s, j) = call(&app, "POST", &assemble, "application/json", bad).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(j, json!({"detail": "unknown project(s): nope"}));
    // a member with no project memberships cannot write bundles into "dev".
    let (s, _) = call_as(
        &app,
        MEMBER_TOKEN,
        "POST",
        &assemble,
        "application/json",
        req.clone(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    for _ in 0..2 {
        let (_, j) = call(&app, "POST", &assemble, "application/json", req.clone()).await;
        assert_eq!(j["state"], "ok", "{j}");
    }

    // frames exactly as node printed them for the minified bundle (outermost first)
    let f = |func: &str, col: u64| json!({"abs_path": "file:///srv/dist/app.js", "filename": "/srv/dist/app.js", "function": func, "lineno": 3, "colno": col});
    let ev = json!({
        "event_id": "5b0f8c7e2a8e4d9fa0c1b2d3e4f50607", "platform": "node", "level": "error",
        "exception": {"values": [{"type": "TypeError", "value": "nope",
            "stacktrace": {"frames": [f("o", 116), f("t", 87), f("r", 35)]}}]},
        "debug_meta": {"images": [{"type": "sourcemap", "code_file": "file:///srv/dist/app.js", "debug_id": DEBUG_ID}]},
    });
    let envelope = format!(
        "{{\"event_id\":\"5b0f8c7e2a8e4d9fa0c1b2d3e4f50607\",\"dsn\":\"http://{KEY}@localhost/1\"}}\n{{\"type\":\"event\"}}\n{ev}\n"
    );
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/1/envelope/")
                .body(Body::from(envelope))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let mut payload = None;
    for _ in 0..40 {
        payload = db
            .read(|c| {
                use rusqlite::OptionalExtension;
                c.query_row("SELECT payload FROM events", [], |r| r.get::<_, Vec<u8>>(0))
                    .optional()
            })
            .await
            .unwrap();
        if payload.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let ev: Value =
        serde_json::from_slice(&zstd::decode_all(&payload.unwrap()[..]).unwrap()).unwrap();
    assert_eq!(ev["symbolicated"], true);
    let frames = ev["exception"]["values"][0]["stacktrace"]["frames"]
        .as_array()
        .unwrap();
    let top = &frames[2];
    assert!(
        top["filename"].as_str().unwrap().ends_with("src/app.js"),
        "{top}"
    );
    assert_eq!(top["function"], "parseConfig");
    assert_eq!(top["lineno"], 3);
    assert_eq!(
        top["context_line"].as_str().unwrap().trim(),
        "throw new TypeError(\"nope\");"
    );
    assert_eq!(top["raw"]["colno"], 35);
    assert_eq!(frames[1]["function"], "loadSettings");
    assert_eq!(frames[0]["function"], "startApp");
}
