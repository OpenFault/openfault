//! Message parameterization, frame normalization and in-app rules (PLAN §5).
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static PARAMS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    [
        (r"[\w.+-]+@[\w-]+\.[\w.-]+", "<email>"),
        (r"\b[a-zA-Z][a-zA-Z0-9+.-]*://[^\s'\x22<>]+", "<url>"),
        // ponytail: TLD allow-list so `obj.method` in JS messages is not mistaken for a hostname.
        (r"(?i)\b(?:[a-z0-9-]+\.)+(?:com|net|org|io|dev|app|co|ai|cloud|local|internal|localhost)\b", "<hostname>"),
        (r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b", "<uuid>"),
        (r"(?i)\b(?:0x)?[0-9a-f]*[a-f][0-9a-f]*\b", "<hex>"), // filtered to len ≥ 8 below
        (r"\b\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?)?", "<date>"),
        (r"\b\d+(?:\.\d+)?\s?(?:ms|s|sec|seconds|min|minutes|h|hours)\b", "<duration>"),
        (r"\b\d{1,3}(?:\.\d{1,3}){3}\b", "<ip>"),
        (r"-?\b\d+\.\d+\b", "<float>"),
        (r"-?\b\d+\b", "<int>"),
        (r#"'[^'\n]*'|"[^"\n]*""#, "<quoted>"),
        (r"(?i)\b(?:true|false)\b", "<bool>"),
    ]
    .into_iter()
    .map(|(r, n)| (Regex::new(r).unwrap(), n))
    .collect()
});

pub fn parameterize(s: &str) -> String {
    let mut out = s.to_string();
    for (re, name) in PARAMS.iter() {
        if *name == "<hex>" {
            out = re
                .replace_all(&out, |c: &regex::Captures| {
                    let m = c[0].trim_start_matches("0x");
                    if m.len() >= 8 || c[0].starts_with("0x") {
                        "<hex>".to_string()
                    } else {
                        c[0].to_string()
                    }
                })
                .into_owned();
        } else {
            out = re.replace_all(&out, *name).into_owned();
        }
    }
    out
}

pub fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

static HEX_ADDR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"0x[0-9a-fA-F]+").unwrap());
static RUBY_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^block (?:\(\d+ levels\) )?in ").unwrap());
static RUBY_RESCUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:rescue|ensure) in ").unwrap());
static ERB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__\d+_\d+").unwrap());
static JAVA_AUTO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\$Lambda\$[\w/$.]*|\$\$(?:EnhancerBy|FastClassBy)\w*\$\$\w+|lambda\$\w+\$\d+")
        .unwrap()
});

pub fn is_js(platform: &str) -> bool {
    matches!(platform, "javascript" | "node" | "nodejs")
}

/// Normalized `module|filename|function` key for one frame. Never lineno/colno.
pub fn frame_key(platform: &str, f: &Value) -> String {
    let s = |k| f.get(k).and_then(Value::as_str).unwrap_or("");
    let mut module = s("module").to_string();
    let mut func = HEX_ADDR.replace_all(s("function"), "").into_owned();
    let mut file = s("filename").to_string();
    if file.is_empty() {
        file = s("abs_path").to_string();
    }
    if is_js(platform) {
        if func == "<anonymous>" || func == "?" {
            func.clear();
        }
        if let Some(last) = func.rsplit('.').next() {
            func = last.to_string();
        }
        // minified names are unstable across builds; only safe to drop once symbolicated context exists
        if func.len() <= 2 && f.get("context_line").is_some() {
            func.clear();
        }
        if file == "<anonymous>" || file == "[native code]" || file.contains("://") {
            file.clear();
        }
    }
    match platform {
        "ruby" => {
            func = RUBY_BLOCK.replace(&func, "block in ").into_owned();
            func = RUBY_RESCUE.replace(&func, "").into_owned();
            func = ERB.replace_all(&func, "").into_owned();
        }
        "java" | "kotlin" => {
            func = JAVA_AUTO.replace_all(&func, "<auto>").into_owned();
            module = JAVA_AUTO.replace_all(&module, "<auto>").into_owned();
        }
        _ => {} // python: `<module>` is kept as-is as the function name
    }
    let base = file.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();
    let base = ERB.replace_all(&base, "").into_owned();
    format!("{module}|{base}|{func}")
}

/// Server-side in-app rules, Sentry/Bugsink `matcher:glob [+-]app [+-]group` syntax. Later lines win.
pub const RULES: &str = "\
stack.abs_path:**/node_modules/** -app
stack.abs_path:node:* -app
stack.abs_path:internal/** -app
stack.abs_path:**/webpack/runtime/** -app
stack.abs_path:**/gems/** -app
stack.abs_path:/usr/lib/ruby/** -app
stack.abs_path:**/vendor/bundle/** -app
stack.module:sentry* -app -group
stack.module:@sentry/** -app -group
stack.abs_path:**/@sentry/** -app -group
stack.abs_path:**/gems/sentry-*/** -app -group
stack.function:sentry* -app -group
";

struct Rule {
    field: &'static str,
    glob: &'static str,
    app: Option<bool>,
    group: Option<bool>,
}

static PARSED: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    RULES
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let (field, glob) = parts.next()?.strip_prefix("stack.")?.split_once(':')?;
            let mut r = Rule {
                field,
                glob,
                app: None,
                group: None,
            };
            for a in parts {
                match a {
                    "+app" => r.app = Some(true),
                    "-app" => r.app = Some(false),
                    "+group" => r.group = Some(true),
                    "-group" => r.group = Some(false),
                    _ => {}
                }
            }
            Some(r)
        })
        .collect()
});

/// `**` matches across `/`, `*` within a segment, `?` one char.
pub fn glob(p: &[u8], s: &[u8]) -> bool {
    match p {
        [] => s.is_empty(),
        [b'*', b'*', rest @ ..] => (0..=s.len()).any(|i| glob(rest, &s[i..])),
        [b'*', rest @ ..] => (0..=s.len())
            .take_while(|&i| i == 0 || s[i - 1] != b'/')
            .any(|i| glob(rest, &s[i..])),
        [b'?', rest @ ..] => !s.is_empty() && glob(rest, &s[1..]),
        [c, rest @ ..] => s.first() == Some(c) && glob(rest, &s[1..]),
    }
}

/// (in_app, contributes_to_grouping). The SDK's `in_app` is the starting point; rules override it.
pub fn classify(f: &Value) -> (bool, bool) {
    let s = |k| f.get(k).and_then(Value::as_str);
    let mut app = f.get("in_app").and_then(Value::as_bool).unwrap_or(false);
    let mut group = true;
    for r in PARSED.iter() {
        let v = match r.field {
            "abs_path" => s("abs_path").or(s("filename")),
            k => s(k),
        };
        if v.is_some_and(|v| glob(r.glob.as_bytes(), v.as_bytes())) {
            app = r.app.unwrap_or(app);
            group = r.group.unwrap_or(group);
        }
    }
    (app, group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn params() {
        assert_eq!(
            parameterize("user 42 not found in 3.5s"),
            "user <int> not found in <duration>"
        );
        assert_eq!(
            parameterize(
                "x@y.com hit https://a.b/c from 10.0.0.1 id 550e8400-e29b-41d4-a716-446655440000 sha deadbeefcafe"
            ),
            "<email> hit <url> from <ip> id <uuid> sha <hex>"
        );
        assert_eq!(
            parameterize("at 2026-10-20T10:00:00Z took 1.25 'abc' true"),
            "at <date> took <float> <quoted> <bool>"
        );
        assert_eq!(
            parameterize("obj.method is not a function"),
            "obj.method is not a function"
        );
        assert_eq!(parameterize("db.example.com refused"), "<hostname> refused");
    }

    #[test]
    fn globs() {
        assert!(glob(
            b"**/node_modules/**",
            b"/app/node_modules/next/dist/x.js"
        ));
        assert!(!glob(b"**/node_modules/**", b"/app/src/x.js"));
        assert!(glob(b"node:*", b"node:internal"));
        assert!(!glob(b"node:*", b"node:internal/process/task_queues"));
        assert!(glob(b"sentry*", b"sentry_sdk.integrations"));
    }

    #[test]
    fn ruby_and_java_frames() {
        let f = json!({"function": "block (2 levels) in index", "filename": "app/controllers/Users_Controller.rb"});
        assert_eq!(frame_key("ruby", &f), "|users_controller.rb|block in index");
        let f = json!({"function": "rescue in call"});
        assert_eq!(frame_key("ruby", &f), "||call");
        let f = json!({"function": "_app_views_users_show_html_erb__123_456", "filename": "show.html.erb"});
        assert_eq!(
            frame_key("ruby", &f),
            "|show.html.erb|_app_views_users_show_html_erb"
        );
        let f = json!({"module": "com.x.Foo$$Lambda$12/0x1234", "function": "lambda$run$0"});
        assert_eq!(frame_key("java", &f), "com.x.Foo<auto>||<auto>");
    }
}
