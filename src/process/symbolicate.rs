//! JS frames → original source via debug-ID SourceMapCaches (PLAN §6). No URL fetching.
//! Native (cocoa) frames → function/file/line via dSYM SymCaches keyed by image debug_id.
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use symbolic_demangle::{Demangle, DemangleOptions};
use symbolic_sourcemapcache::{ScopeLookupResult, SourceMapCache, SourcePosition};
use symbolic_symcache::SymCache;

thread_local! {
    // ponytail: raw blobs keyed by (project, debug_id), cleared at 32; parse() is zero-copy so re-parsing per event is cheap.
    static BLOBS: RefCell<HashMap<String, Rc<Vec<u8>>>> = RefCell::default();
}

/// `sql` selects the blob for (?1 project, ?2 debug_id).
fn blob(c: &Connection, sql: &str, pid: i64, id: &str) -> Option<Rc<Vec<u8>>> {
    let key = format!("{sql}:{pid}:{id}");
    if let Some(b) = BLOBS.with_borrow(|m| m.get(&key).cloned()) {
        return Some(b);
    }
    let b: Vec<u8> = c
        .query_row(sql, params![pid, id],
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
    if ev["platform"] == "cocoa" {
        return native(c, pid, ev);
    }
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
    let b = blob(
        c,
        "SELECT map_cache FROM artifact_bundles WHERE project_id = ?1 AND debug_id = ?2",
        pid,
        id,
    )?;
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

fn hex(v: &Value) -> Option<u64> {
    let s = v.as_str()?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

/// macho images as (start, end, lowercase debug_id).
fn macho_images(ev: &Value) -> Vec<(u64, u64, String)> {
    ev.pointer("/debug_meta/images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|i| i["type"] == "macho")
        .filter_map(|i| {
            let a = hex(&i["image_addr"])?;
            Some((a, a + i["image_size"].as_u64()?, i["debug_id"].as_str()?.to_lowercase()))
        })
        .collect()
}

/// Image-relative lookup address. Caller frames hold return addresses, so step back into the call
/// (Sentry's heuristic); the innermost frame (last in the array) is the faulting pc itself.
fn rel_addr(images: &[(u64, u64, String)], addr: u64, innermost: bool) -> Option<(&str, u64)> {
    let (start, _, id) = images.iter().find(|(s, e, _)| (*s..*e).contains(&addr))?;
    Some((id, addr - start - u64::from(!innermost)))
}

fn native(c: &Connection, pid: i64, ev: &mut Value) {
    let images = macho_images(ev);
    if images.is_empty() {
        return;
    }
    let mut changed = false;
    for path in ["/exception/values", "/threads/values"] {
        for v in ev.pointer_mut(path).and_then(Value::as_array_mut).into_iter().flatten() {
            let frames = v.pointer_mut("/stacktrace/frames").and_then(Value::as_array_mut);
            let Some(frames) = frames else { continue };
            let n = frames.len();
            for (i, f) in frames.iter_mut().enumerate() {
                changed |= native_frame(c, pid, &images, f, i + 1 == n).is_some();
            }
        }
    }
    if changed {
        ev["symbolicated"] = true.into();
    }
}

fn native_frame(
    c: &Connection,
    pid: i64,
    images: &[(u64, u64, String)],
    f: &mut Value,
    innermost: bool,
) -> Option<()> {
    let (id, addr) = rel_addr(images, hex(&f["instruction_addr"])?, innermost)?;
    let b = blob(c, "SELECT symcache FROM debug_files WHERE project_id = ?1 AND debug_id = ?2", pid, id)?;
    let cache = SymCache::parse(&b).ok()?;
    // shortcut: inlined frames collapse to the outermost caller; expand them if inline detail is wanted.
    let loc = cache.lookup(addr).last()?;
    let func = loc.function();
    f["raw"] = json!({"function": f.get("function"), "symbol": f.get("symbol")});
    f["symbol"] = func.name().into();
    f["function"] = func.name_for_demangling().try_demangle(DemangleOptions::name_only()).into();
    if let Some(file) = loc.file() {
        let path = file.full_path();
        f["filename"] = path.rsplit('/').next().unwrap_or(&path).into();
        f["abs_path"] = path.into();
    }
    if loc.line() > 0 {
        f["lineno"] = loc.line().into();
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn native_addr_math() {
        let ev = json!({"debug_meta": {"images": [
            {"type": "macho", "image_addr": "0x100000000", "image_size": 4096, "debug_id": "ABCD-EF"},
            {"type": "sourcemap", "image_addr": "0x0", "image_size": 1, "debug_id": "x"}]}});
        let imgs = super::macho_images(&ev);
        assert_eq!(imgs, vec![(0x100000000, 0x100001000, "abcd-ef".into())]);
        assert_eq!(super::rel_addr(&imgs, 0x100000010, true), Some(("abcd-ef", 0x10)));
        assert_eq!(super::rel_addr(&imgs, 0x100000010, false), Some(("abcd-ef", 0xf)));
        assert_eq!(super::rel_addr(&imgs, 0x100001000, true), None);
    }

    /// tests/fixtures/crash.dSYM.zip: `int crash_here(int x) {\n return x * 2; }` (crash.c), clang -g arm64.
    #[test]
    fn symbolicates_dsym_fixture() {
        let zip = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/crash.dSYM.zip")).unwrap();
        let files = crate::sourcemaps::debug_files(&zip).unwrap();
        assert_eq!(files.len(), 1);
        let (id, _, _, blob) = &files[0];
        assert_eq!(id, "353abed5-8469-3171-a13c-b81f142cf274");
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../schema.sql")).unwrap();
        c.execute_batch(
            "INSERT INTO orgs (id, name, slug, created_at) VALUES (1, 'o', 'o', 0);
             INSERT INTO projects (id, org_id, slug, name, created_at) VALUES (1, 1, 'p', 'p', 0);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO debug_files (project_id, debug_id, symcache, created_at) VALUES (1, ?1, ?2, 0)",
            rusqlite::params![id, blob],
        )
        .unwrap();
        // crash_here line 2 sits at vmaddr 0x100000330 (__TEXT vmaddr 0x100000000), loaded at 0x104000000.
        let mut ev = json!({"platform": "cocoa",
            "debug_meta": {"images": [{"type": "macho", "image_addr": "0x104000000", "image_size": 16384,
                "debug_id": id.to_uppercase()}]},
            "exception": {"values": [{"stacktrace": {"frames": [
                {"instruction_addr": "0x200000000"}, {"instruction_addr": "0x104000330"}]}}]}});
        super::symbolicate(&c, 1, &mut ev);
        assert_eq!(ev["symbolicated"], true);
        let f = &ev["exception"]["values"][0]["stacktrace"]["frames"];
        assert_eq!(f[0].get("function"), None);
        assert_eq!((f[1]["function"].as_str(), f[1]["filename"].as_str(), f[1]["lineno"].as_u64()),
            (Some("crash_here"), Some("crash.c"), Some(2)));
    }
}
