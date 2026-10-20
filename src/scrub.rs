use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const F: &str = "[Filtered]";

static KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)password|passwd|secret|token|api[_-]?key|authorization|cookie|set-cookie|session|credit|card|ssn|private").unwrap()
});
// ponytail: a generic `[A-Za-z0-9_-]{32,}` secret rule is deliberately absent: it eats hashes, ids and chunk names.
static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"Bearer [A-Za-z0-9._-]{20,}",
        r"|\bAKIA[0-9A-Z]{16}\b",
        r"|\b(?:gh[po]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})",
        r"|\bsk_(?:live|test)_[A-Za-z0-9]{10,}",
    ))
    .unwrap()
});
static CARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d(?:[ -]?\d){12,18}\b").unwrap());

fn luhn(s: &str) -> bool {
    let d: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&d.len()) {
        return false;
    }
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| {
            if i % 2 == 1 {
                let y = x * 2;
                if y > 9 { y - 9 } else { y }
            } else {
                x
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn scrub_str(s: &mut String) {
    if SECRET.is_match(s) {
        *s = SECRET.replace_all(s, F).into_owned();
    }
    if CARD.is_match(s) {
        *s = CARD
            .replace_all(s, |c: &regex::Captures| {
                if luhn(&c[0]) {
                    F.to_string()
                } else {
                    c[0].to_string()
                }
            })
            .into_owned();
    }
}

/// Every string leaf in the event: value-shaped secrets.
fn values(v: &mut Value) {
    match v {
        Value::String(s) => scrub_str(s),
        Value::Array(a) => a.iter_mut().for_each(values),
        Value::Object(o) => o.values_mut().for_each(values),
        _ => {}
    }
}

/// Recursive key-based scrub: matching keys get "[Filtered]". Handles `[[k, v], ...]` pair lists (headers).
fn keys(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                if KEY.is_match(k) && !x.is_null() {
                    *x = F.into();
                } else {
                    keys(x);
                }
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                if let Value::Array(pair) = x
                    && pair.len() == 2
                    && pair[0].as_str().is_some_and(|k| KEY.is_match(k))
                {
                    pair[1] = F.into();
                } else {
                    keys(x);
                }
            }
        }
        _ => {}
    }
}

fn query_string(v: &mut Value) {
    if let Value::String(s) = v {
        *s = s
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, _)) if KEY.is_match(k) => format!("{k}={F}"),
                _ => kv.to_string(),
            })
            .collect::<Vec<_>>()
            .join("&");
    } else {
        keys(v);
    }
}

fn each<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Vec<Value>> {
    let mut cur = v;
    for p in path {
        cur = cur.get_mut(*p)?;
    }
    cur.as_array_mut()
}

/// Scrub an event in place before anything is stored (PLAN §4).
pub fn scrub(ev: &mut Value, store_ip: bool) {
    if let Some(req) = ev.get_mut("request").and_then(Value::as_object_mut) {
        if let Some(c) = req.get_mut("cookies")
            && !c.is_null()
        {
            *c = F.into();
        }
        for k in ["headers", "data", "env"] {
            if let Some(x) = req.get_mut(k) {
                keys(x);
            }
        }
        if let Some(q) = req.get_mut("query_string") {
            query_string(q);
        }
    }
    for k in ["extra", "contexts"] {
        if let Some(x) = ev.get_mut(k) {
            keys(x);
        }
    }
    if let Some(excs) = each(ev, &["exception", "values"]) {
        for e in excs {
            if let Some(frames) = each(e, &["stacktrace", "frames"]) {
                for f in frames {
                    if let Some(vars) = f.get_mut("vars") {
                        keys(vars);
                    }
                }
            }
        }
    }
    if let Some(crumbs) = each(ev, &["breadcrumbs", "values"]) {
        for b in crumbs {
            if let Some(d) = b.get_mut("data") {
                keys(d);
            }
        }
    }
    if let Some(user) = ev.get_mut("user").and_then(Value::as_object_mut) {
        if !store_ip {
            user.remove("ip_address");
        }
        for (k, x) in user.iter_mut() {
            if matches!(k.as_str(), "id" | "email" | "username" | "ip_address") {
                continue;
            }
            if KEY.is_match(k) {
                *x = F.into();
            } else {
                keys(x);
            }
        }
    }
    values(ev);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrubs_keys_values_and_ip() {
        let mut ev = json!({
            "request": {
                "headers": {"Authorization": "Basic abc", "Accept": "text/html"},
                "cookies": "_session=xyz",
                "data": {"user": {"password": "hunter2", "name": "bob"}},
                "query_string": "page=2&api_key=abc"
            },
            "extra": {"Session-Id": "s1", "ok": "fine"},
            "exception": {"values": [{"stacktrace": {"frames": [{"vars": {"token": "t", "x": 1}}]}}]},
            "breadcrumbs": {"values": [{"data": {"secret_thing": 1, "url": "/x"}}]},
            "user": {"id": "42", "email": "a@b.co", "ip_address": "1.2.3.4", "private_note": "x"},
            "message": "card 4111 1111 1111 1111 not 1234567890123 key AKIAABCDEFGHIJKLMNOP auth Bearer abcdefghijklmnopqrstuvwxyz sk_live_abcdefghij12 ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        });
        scrub(&mut ev, false);
        assert_eq!(ev["request"]["headers"]["Authorization"], F);
        assert_eq!(ev["request"]["headers"]["Accept"], "text/html");
        assert_eq!(ev["request"]["cookies"], F);
        assert_eq!(ev["request"]["data"]["user"]["password"], F);
        assert_eq!(ev["request"]["data"]["user"]["name"], "bob");
        assert_eq!(ev["request"]["query_string"], "page=2&api_key=[Filtered]");
        assert_eq!(ev["extra"]["Session-Id"], F);
        assert_eq!(ev["extra"]["ok"], "fine");
        assert_eq!(
            ev["exception"]["values"][0]["stacktrace"]["frames"][0]["vars"]["token"],
            F
        );
        assert_eq!(ev["breadcrumbs"]["values"][0]["data"]["secret_thing"], F);
        assert_eq!(ev["user"]["id"], "42");
        assert_eq!(ev["user"]["email"], "a@b.co");
        assert!(ev["user"].get("ip_address").is_none());
        assert_eq!(ev["user"]["private_note"], F);
        assert_eq!(
            ev["message"],
            "card [Filtered] not 1234567890123 key [Filtered] auth [Filtered] [Filtered] [Filtered]"
        );
    }

    #[test]
    fn header_pairs_and_store_ip() {
        let mut ev = json!({"request": {"headers": [["Cookie", "a=b"], ["Host", "x"]]}, "user": {"ip_address": "1.2.3.4"}});
        scrub(&mut ev, true);
        assert_eq!(ev["request"]["headers"][0][1], F);
        assert_eq!(ev["request"]["headers"][1][1], "x");
        assert_eq!(ev["user"]["ip_address"], "1.2.3.4");
    }
}
