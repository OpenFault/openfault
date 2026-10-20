//! JS frames → original source via debug-ID SourceMapCaches (PLAN §6). No URL fetching.
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use symbolic_sourcemapcache::{ScopeLookupResult, SourceMapCache, SourcePosition};

thread_local! {
    // ponytail: raw blobs keyed by (project, debug_id), cleared at 32; parse() is zero-copy so re-parsing per event is cheap.
    static BLOBS: RefCell<HashMap<String, Rc<Vec<u8>>>> = RefCell::default();
}

fn blob(c: &Connection, pid: i64, id: &str) -> Option<Rc<Vec<u8>>> {
    let key = format!("{pid}:{id}");
    if let Some(b) = BLOBS.with_borrow(|m| m.get(&key).cloned()) {
        return Some(b);
    }
    let b: Vec<u8> = c
        .query_row(
            "SELECT map_cache FROM artifact_bundles WHERE project_id = ?1 AND debug_id = ?2",
            params![pid, id],
            |r| r.get(0),
        )
        .optional()
        .ok()??;
    let b = Rc::new(b);
    BLOBS.with_borrow_mut(|m| {
        // ponytail: also evict all past 64 MiB total.
        if m.len() >= 32 || m.values().map(|v| v.len()).sum::<usize>() + b.len() > 64 << 20 {
            m.clear();
        }
        m.insert(key, b.clone());
    });
    Some(b)
}

pub fn symbolicate(c: &Connection, pid: i64, ev: &mut Value) {
    if !matches!(ev["platform"].as_str(), Some("javascript" | "node")) {
        return;
    }
    let images: Vec<(String, String)> = ev
        .pointer("/debug_meta/images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|i| i["type"] == "sourcemap")
        .filter_map(|i| {
            Some((
                i["code_file"].as_str()?.to_string(),
                i["debug_id"].as_str()?.to_lowercase(),
            ))
        })
        .collect();
    if images.is_empty() {
        return;
    }
    let mut changed = false;
    let excs = ev
        .pointer_mut("/exception/values")
        .and_then(Value::as_array_mut);
    for exc in excs.into_iter().flatten() {
        let frames = exc
            .pointer_mut("/stacktrace/frames")
            .and_then(Value::as_array_mut);
        for f in frames.into_iter().flatten() {
            changed |= frame(c, pid, &images, f).is_some();
        }
    }
    if changed {
        ev["symbolicated"] = true.into();
    }
}

fn base(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn frame(c: &Connection, pid: i64, images: &[(String, String)], f: &mut Value) -> Option<()> {
    let abs = f["abs_path"].as_str()?;
    let (line, col) = (f["lineno"].as_u64()?, f["colno"].as_u64()?);
    let (_, id) = images
        .iter()
        .find(|(cf, _)| cf == abs)
        .or_else(|| images.iter().find(|(cf, _)| base(cf) == base(abs)))?;
    let b = blob(c, pid, id)?;
    let cache = SourceMapCache::parse(&b).ok()?;
    let loc = cache.lookup(SourcePosition::new(
        (line as u32).checked_sub(1)?,
        (col as u32).checked_sub(1)?,
    ))?;
    let file = loc.file_name()?.to_string();
    f["raw"] =
        json!({"abs_path": abs, "lineno": line, "colno": col, "function": f.get("function")});
    if let ScopeLookupResult::NamedScope(n) = loc.scope() {
        f["function"] = n.into();
    }
    if let Some(src) = loc.file_source() {
        let lines: Vec<&str> = src.lines().collect();
        let i = loc.line() as usize;
        if let Some(l) = lines.get(i) {
            f["context_line"] = (*l).into();
            f["pre_context"] = json!(lines[i.saturating_sub(3)..i]);
            f["post_context"] = json!(lines[i + 1..(i + 4).min(lines.len())]);
        }
    }
    f["in_app"] = (!file.contains("node_modules")).into();
    f["abs_path"] = file.clone().into();
    f["filename"] = file.into();
    f["lineno"] = (loc.line() + 1).into();
    f["colno"] = (loc.column() + 1).into();
    Some(())
}
