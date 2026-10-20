//! Hash components in priority order (PLAN §5). An event joins an issue if any hash matches.
use super::normalize::{Rule, classify_with, first_line, frame_key, parameterize};
use serde_json::Value;

pub struct Grouping {
    pub hashes: Vec<String>,
    pub title: String,
    pub culprit: String,
}

fn h(kind: &str, parts: &[String]) -> String {
    let mut x = blake3::Hasher::new();
    x.update(kind.as_bytes());
    for p in parts {
        x.update(b"\x1f");
        x.update(p.as_bytes());
    }
    x.finalize().to_hex()[..32].to_string()
}

fn str_at<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn exceptions(ev: &Value) -> &[Value] {
    ev.pointer("/exception/values")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn frames(e: &Value) -> &[Value] {
    e.pointer("/stacktrace/frames")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn synthetic(e: &Value) -> bool {
    e.pointer("/mechanism/synthetic")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn message(ev: &Value) -> &str {
    let m = ev
        .pointer("/logentry/formatted")
        .or(ev.pointer("/logentry/message"))
        .and_then(Value::as_str);
    m.or_else(|| match ev.get("message") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(o) => o
            .get("formatted")
            .or(o.get("message"))
            .and_then(Value::as_str),
        None => None,
    })
    .unwrap_or("")
}

fn stack_hash(platform: &str, excs: &[Value], rules: &[Rule]) -> Option<String> {
    let usable: Vec<(&Value, Vec<(&Value, bool)>)> = excs
        .iter()
        .map(|e| {
            (
                e,
                frames(e)
                    .iter()
                    .map(|f| (f, classify_with(f, rules)))
                    .filter(|(_, (_, g))| *g)
                    .map(|(f, (a, _))| (f, a))
                    .collect(),
            )
        })
        .collect();
    let any_app = usable.iter().any(|(_, fs)| fs.iter().any(|(_, a)| *a));
    let mut parts = Vec::new();
    let mut n_frames = 0;
    for (e, fs) in &usable {
        if !synthetic(e) {
            parts.push(format!("type:{}", str_at(e, "type")));
        }
        let mut last: Option<String> = None;
        for (f, app) in fs {
            if any_app && !app {
                continue;
            }
            let k = frame_key(platform, f);
            if k == "||" || last.as_ref() == Some(&k) {
                continue; // empty frame, or recursion
            }
            n_frames += 1;
            parts.push(k.clone());
            last = Some(k);
        }
    }
    (n_frames > 0).then(|| h("stack", &parts))
}

pub fn group(ev: &Value) -> Grouping {
    group_with(ev, &[])
}

/// `rules`: the project's in-app rules, applied after the built-ins.
pub fn group_with(ev: &Value, rules: &[Rule]) -> Grouping {
    let platform = str_at(ev, "platform");
    let excs = exceptions(ev);
    let last = excs.last();
    let msg = message(ev);

    let mut default = Vec::new();
    default.extend(stack_hash(platform, excs, rules));
    if let Some(e) = last {
        let ty = if synthetic(e) { "" } else { str_at(e, "type") };
        let val = parameterize(first_line(str_at(e, "value")));
        if !ty.is_empty() || !val.is_empty() {
            default.push(h("type_value", &[ty.to_string(), val]));
        }
    }
    if !msg.is_empty() {
        default.push(h("message", &[parameterize(first_line(msg))]));
    }
    if default.is_empty() {
        default.push(h("empty", &[]));
    }

    let fp: Vec<String> = ev
        .get("fingerprint")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|p| {
                    p.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| p.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    let is_default = |p: &str| p.replace(' ', "") == "{{default}}";
    let hashes = if fp.is_empty() || (fp.len() == 1 && is_default(&fp[0])) {
        default
    } else {
        // custom fingerprint replaces default grouping entirely (that is the point of setting one)
        let parts: Vec<String> = fp
            .iter()
            .map(|p| {
                if is_default(p) {
                    default[0].clone()
                } else {
                    p.clone()
                }
            })
            .collect();
        vec![h("fingerprint", &parts)]
    };

    let title = match last {
        Some(e) => {
            let (ty, val) = (str_at(e, "type"), first_line(str_at(e, "value")));
            match (ty.is_empty(), val.is_empty()) {
                (false, false) => format!("{ty}: {val}"),
                (false, true) => ty.to_string(),
                (true, false) => val.to_string(),
                _ => first_line(msg).to_string(),
            }
        }
        None => first_line(msg).to_string(),
    };
    let title = if title.is_empty() {
        "<unlabeled event>".to_string()
    } else {
        title.chars().take(200).collect()
    };

    let culprit = last
        .and_then(|e| frames(e).iter().rev().find(|f| classify_with(f, rules).0))
        .map(|f| {
            let loc = [str_at(f, "module"), str_at(f, "filename")]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("");
            format!("{} ({loc})", str_at(f, "function"))
        })
        .unwrap_or_else(|| str_at(ev, "transaction").to_string());

    Grouping {
        hashes,
        title,
        culprit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ruby(lineno: u32, value: &str) -> Value {
        json!({"platform": "ruby", "exception": {"values": [{"type": "NoMethodError", "value": value,
        "stacktrace": {"frames": [
            {"filename": "/gems/actionpack/lib/x.rb", "abs_path": "/usr/local/bundle/gems/actionpack-7.1/lib/x.rb", "function": "process", "lineno": 10, "in_app": false},
            {"filename": "app/controllers/users_controller.rb", "abs_path": "/app/app/controllers/users_controller.rb", "function": "block (2 levels) in show", "lineno": lineno, "in_app": true}
        ]}}]}})
    }

    #[test]
    fn line_numbers_do_not_split() {
        let a = group(&ruby(10, "undefined method `name' for nil"));
        let b = group(&ruby(99, "undefined method `name' for nil"));
        assert_eq!(a.hashes[0], b.hashes[0]);
        assert_eq!(a.title, "NoMethodError: undefined method `name' for nil");
        assert_eq!(
            a.culprit,
            "block (2 levels) in show (app/controllers/users_controller.rb)"
        );
    }

    #[test]
    fn ruby_block_levels_normalized() {
        let mut a = ruby(1, "x");
        a["exception"]["values"][0]["stacktrace"]["frames"][1]["function"] = json!("block in show");
        assert_eq!(group(&a).hashes[0], group(&ruby(1, "x")).hashes[0]);
    }

    #[test]
    fn messages_with_different_ints_group() {
        let a = group(&json!({"message": "Order 1234 failed after 3 retries"}));
        let b = group(&json!({"message": {"formatted": "Order 98 failed after 5 retries"}}));
        assert_eq!(a.hashes, b.hashes);
        assert_eq!(a.title, "Order 1234 failed after 3 retries");
    }

    #[test]
    fn recursion_collapsed() {
        let f = json!({"filename": "a.js", "function": "walk", "in_app": true});
        let mk = |n| {
            json!({"platform": "node", "exception": {"values": [{"type": "RangeError", "value": "x",
            "stacktrace": {"frames": vec![f.clone(); n]}}]}})
        };
        assert_eq!(group(&mk(3)).hashes[0], group(&mk(300)).hashes[0]);
    }

    #[test]
    fn synthetic_ignores_type() {
        let mk = |ty: &str| {
            json!({"platform": "node", "exception": {"values": [{"type": ty, "value": "boom",
            "mechanism": {"synthetic": true}, "stacktrace": {"frames": [{"filename": "a.js", "function": "f", "in_app": true}]}}]}})
        };
        assert_eq!(group(&mk("Error")).hashes, group(&mk("Object")).hashes);
    }

    #[test]
    fn fingerprint_override() {
        let mut a = ruby(1, "x");
        let base = group(&a).hashes[0].clone();
        a["fingerprint"] = json!(["{{ default }}"]);
        assert_eq!(group(&a).hashes[0], base);
        a["fingerprint"] = json!(["{{ default }}", "tenant-1"]);
        let t1 = group(&a).hashes;
        a["fingerprint"] = json!(["{{ default }}", "tenant-2"]);
        let t2 = group(&a).hashes;
        assert_eq!(t1.len(), 1);
        assert_ne!(t1, t2);
        assert_ne!(t1[0], base);
    }

    #[test]
    fn node_modules_excluded() {
        let mk = |lib: &str| {
            json!({"platform": "node", "exception": {"values": [{"type": "TypeError", "value": "x is undefined",
            "stacktrace": {"frames": [
                {"abs_path": format!("/app/node_modules/{lib}/index.js"), "filename": format!("node_modules/{lib}/index.js"), "function": "run", "in_app": true},
                {"abs_path": "/app/src/handler.js", "filename": "src/handler.js", "function": "handle", "in_app": true}
            ]}}]}})
        };
        let (a, b) = (group(&mk("express")), group(&mk("koa")));
        assert_eq!(a.hashes[0], b.hashes[0]);
        assert_eq!(a.culprit, "handle (src/handler.js)");
    }

    #[test]
    fn type_value_is_secondary_hash() {
        let a = group(&ruby(1, "undefined method `name' for nil"));
        assert_eq!(a.hashes.len(), 2);
        let mut b = ruby(1, "undefined method `name' for nil");
        b["exception"]["values"][0]["stacktrace"]["frames"][1]["function"] = json!("index");
        assert_eq!(a.hashes[1], group(&b).hashes[1]);
        assert_ne!(a.hashes[0], group(&b).hashes[0]);
    }
}
