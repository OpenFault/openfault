//! sentry-cli / bundler-plugin compatible upload API (PLAN §6). Debug-ID artifact bundles only.
use crate::{
    AppState,
    auth::{ApiToken, auth_token},
    db::now,
};
use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Request, State},
    http::{StatusCode, header, request::Parts},
    routing::{get, post},
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::sync::{LazyLock, Mutex};
use symbolic_sourcemapcache::SourceMapCacheWriter;

pub fn routes() -> Router<AppState> {
    let o = "/api/0/organizations/{org}";
    let p = "/api/0/projects/{org}/{project}";
    Router::new()
        .route("/api/0/", get(empty))
        .route("/api/0/auth/", get(empty))
        .route(&format!("{o}/"), get(org))
        .route(
            &format!("{o}/chunk-upload/"),
            get(chunk_opts).post(chunk_upload),
        )
        .route(&format!("{o}/artifactbundle/assemble/"), post(assemble))
        .route(&format!("{o}/releases/"), post(release_new))
        .route(&format!("{o}/releases/{{v}}/"), get(release).put(release))
        .route(&format!("{o}/releases/{{v}}/deploys/"), post(created))
        .route(&format!("{p}/releases/"), post(release_new))
        .route(&format!("{p}/releases/{{v}}/"), get(release).put(release))
        .route(&format!("{p}/releases/{{v}}/deploys/"), post(created))
        .route(&format!("{p}/files/dsyms/"), post(dsyms))
}

/// Bearer `of_…` token with `write` scope.
struct W(ApiToken);
impl FromRequestParts<AppState> for W {
    type Rejection = StatusCode;
    async fn from_request_parts(p: &mut Parts, st: &AppState) -> Result<Self, StatusCode> {
        let bearer = p
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let t = auth_token(st, bearer.trim())
            .await
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if !t.scopes.contains("write") {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(W(t))
    }
}

type R = Result<Json<Value>, (StatusCode, String)>;
fn e500(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn empty(_: W) -> Json<Value> {
    Json(json!({}))
}

async fn created(_: W) -> (StatusCode, Json<Value>) {
    (StatusCode::CREATED, Json(json!({})))
}

// ponytail: org slug ignored; one install = one org.
async fn org(_: W, Path(org): Path<String>) -> Json<Value> {
    Json(json!({"id": "1", "slug": org, "name": org}))
}

async fn release_new(_: W, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::CREATED,
        Json(release_json(b["version"].as_str().unwrap_or_default())),
    )
}

async fn release(_: W, Path(p): Path<HashMap<String, String>>) -> Json<Value> {
    Json(release_json(&p["v"]))
}

fn release_json(v: &str) -> Value {
    json!({"version": v, "ref": null, "url": null, "dateCreated": "1970-01-01T00:00:00Z",
           "dateReleased": null, "lastEvent": null, "newGroups": 0, "projects": []})
}

async fn dsyms(_: W) -> (StatusCode, &'static str) {
    (StatusCode::BAD_REQUEST, "not supported")
}

async fn chunk_opts(_: W, Path(org): Path<String>) -> Json<Value> {
    Json(json!({
        // relative: sentry-cli joins it onto SENTRY_URL/api/0, which survives proxies.
        "url": format!("/organizations/{org}/chunk-upload/"),
        "chunkSize": CHUNK_SIZE, "chunksPerRequest": 64,
        // deviation: app() caps request bodies at 20 MiB on the wire; bundles are assembled in memory.
        "maxRequestSize": 16 << 20, "maxFileSize": 256 << 20,
        "concurrency": 8, "hashAlgorithm": "sha1",
        // ponytail: no gzip chunks, so no gzip decoder; sentry-cli then sends plain `file` parts.
        "compression": [],
        "accept": ["artifact_bundles", "artifact_bundles_v2", "release_files"],
    }))
}

const CHUNK_SIZE: usize = 8 << 20;
/// Caps assembled zip size (chunks) and total decompressed bytes read from it (zip bomb).
const MAX_FILE: usize = 256 << 20;
const MAX_UNZIPPED: u64 = if cfg!(test) { 1 << 20 } else { 256 << 20 };

fn sha1_hex(b: &[u8]) -> String {
    Sha1::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

async fn chunk_upload(_: W, State(st): State<AppState>, req: Request) -> R {
    let bad = |e: multer::Error| (StatusCode::BAD_REQUEST, e.to_string());
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(ct).map_err(bad)?;
    let mut mp = multer::Multipart::new(req.into_body().into_data_stream(), boundary);
    let mut chunks = vec![];
    while let Some(f) = mp.next_field().await.map_err(bad)? {
        let name = f.file_name().unwrap_or_default().to_lowercase();
        let data = f.bytes().await.map_err(bad)?;
        if data.len() > CHUNK_SIZE {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("chunk {name}: too large"),
            ));
        }
        if sha1_hex(&data) != name {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("chunk {name}: checksum mismatch"),
            ));
        }
        chunks.push((name, data.to_vec()));
    }
    st.db
        .write(move |c| {
            let t = now();
            for (h, d) in chunks {
                c.execute(
                    "INSERT OR IGNORE INTO chunks (sha1, data, created_at) VALUES (?1, ?2, ?3)",
                    params![h, d, t],
                )?;
            }
            Ok(())
        })
        .await
        .map_err(e500)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Assemble {
    checksum: String,
    chunks: Vec<String>,
    #[serde(default)]
    projects: Vec<String>,
}

/// ponytail: in-memory, so a poll after a restart re-uploads (chunks are gone); harmless.
static ASSEMBLED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

async fn assemble(W(t): W, State(st): State<AppState>, Json(a): Json<Assemble>) -> R {
    let ok = || Ok(Json(json!({"state": "ok", "missingChunks": []})));
    // keyed by target too: the same bundle uploaded to another project must still be written.
    let key = format!(
        "{}:{}:{:?}:{:?}",
        a.checksum, t.user_id, t.project_id, a.projects
    );
    if ASSEMBLED.lock().unwrap().contains(&key) {
        return ok();
    }
    if a.chunks.len() > MAX_FILE / CHUNK_SIZE {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "bundle too large".into()));
    }
    let ids = a.chunks.clone();
    let found: Vec<Option<Vec<u8>>> = st
        .db
        .read(move |c| {
            let mut q = c.prepare("SELECT data FROM chunks WHERE sha1 = ?1")?;
            ids.iter()
                .map(|h| q.query_row([h], |r| r.get(0)).optional())
                .collect()
        })
        .await
        .map_err(e500)?;
    let missing: Vec<&String> = a
        .chunks
        .iter()
        .zip(&found)
        .filter(|(_, d)| d.is_none())
        .map(|(h, _)| h)
        .collect();
    if !missing.is_empty() {
        return Ok(Json(
            json!({"state": "not_found", "missingChunks": missing}),
        ));
    }
    let zip: Vec<u8> = found.into_iter().flatten().flatten().collect();
    let err = |d: String| {
        Ok(Json(
            json!({"state": "error", "missingChunks": [], "detail": d}),
        ))
    };
    if sha1_hex(&zip) != a.checksum.to_lowercase() {
        return err("checksum mismatch".into());
    }
    let caches = match tokio::task::spawn_blocking(move || bundle(&zip))
        .await
        .map_err(e500)?
    {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let (slugs, scoped, chunks, uid) = (a.projects, t.project_id, a.chunks, t.user_id);
    let wrote = st
        .db
        .write(move |c| {
            let tx = c.transaction()?;
            // only projects the token's user can see right now (role/membership may have changed).
            let admin: bool =
                tx.query_row("SELECT role != 'member' FROM users WHERE id = ?1", [uid], |r| r.get(0))?;
            let visible: Vec<(i64, String)> = tx
                .prepare(&format!("SELECT id, slug FROM projects WHERE {} ORDER BY id", crate::ui::VISIBLE))?
                .query_map(params![admin, uid], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            // a project-scoped token only ever writes to its own project.
            let pids: Vec<i64> = visible
                .iter()
                .filter(|(id, s)| scoped.map_or(slugs.contains(s), |p| p == *id))
                .map(|v| v.0)
                .collect();
            if pids.is_empty() {
                return Ok(Err(match scoped {
                    Some(_) => (StatusCode::FORBIDDEN, "no accessible project".to_string()),
                    None => (
                        StatusCode::BAD_REQUEST,
                        json!({"detail": format!("unknown project(s): {}", slugs.join(", "))}).to_string(),
                    ),
                }));
            }
            for pid in pids {
                for (debug_id, blob) in &caches {
                    tx.execute(
                        "INSERT INTO artifact_bundles (project_id, debug_id, map_cache, created_at) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT (project_id, debug_id) DO UPDATE SET map_cache = excluded.map_cache, created_at = excluded.created_at",
                        params![pid, debug_id, blob, now()],
                    )?;
                }
            }
            for h in &chunks {
                tx.execute("DELETE FROM chunks WHERE sha1 = ?1", [h])?;
            }
            tx.commit()?;
            Ok(Ok(()))
        })
        .await
        .map_err(e500)?;
    wrote?;
    let mut done = ASSEMBLED.lock().unwrap();
    // ponytail: cleared when full instead of aged out.
    if done.len() >= 256 {
        done.clear();
    }
    done.insert(key);
    ok()
}

/// Artifact bundle zip → [(debug_id, SourceMapCache blob)].
fn bundle(zip: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut z = zip::ZipArchive::new(Cursor::new(zip)).map_err(|e| e.to_string())?;
    let mut budget = MAX_UNZIPPED;
    let mut read = |name: &str| -> Result<String, String> {
        let mut s = String::new();
        z.by_name(name)
            .map_err(|e| format!("{name}: {e}"))?
            .take(budget + 1)
            .read_to_string(&mut s)
            .map_err(|e| format!("{name}: {e}"))?;
        budget = budget
            .checked_sub(s.len() as u64)
            .ok_or("bundle too large when decompressed")?;
        Ok(s)
    };
    let manifest: Value =
        serde_json::from_str(&read("manifest.json")?).map_err(|e| e.to_string())?;
    let mut pairs: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for (path, f) in manifest["files"].as_object().into_iter().flatten() {
        let debug_id = f["headers"].as_object().and_then(|h| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("debug-id"))
                .and_then(|(_, v)| v.as_str())
        });
        let (Some(id), Some(ty)) = (debug_id, f["type"].as_str()) else {
            continue;
        };
        let slot = pairs.entry(id.to_lowercase()).or_default();
        match ty {
            "minified_source" => slot.0 = Some(read(path)?),
            "source_map" => slot.1 = Some(read(path)?),
            _ => {}
        }
    }
    let mut out = vec![];
    for (id, pair) in pairs {
        let (Some(src), Some(map)) = pair else {
            continue;
        };
        let mut blob = vec![];
        SourceMapCacheWriter::new(&src, &map)
            .map_err(|e| format!("{id}: {e}"))?
            .serialize(&mut blob)
            .map_err(|e| e.to_string())?;
        out.push((id, blob));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    #[test]
    fn zip_bomb_is_capped() {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        z.start_file("manifest.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        z.write_all(&vec![b' '; (super::MAX_UNZIPPED + 1) as usize])
            .unwrap();
        let zip = z.finish().unwrap().into_inner();
        assert_eq!(
            super::bundle(&zip).unwrap_err(),
            "bundle too large when decompressed"
        );
    }
}
