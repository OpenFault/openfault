//! Property tests on the trust boundary (PLAN §13b): envelope parser, scrubber, grouping, untrusted().
use openfault::{
    ingest::envelope::parse,
    issues::untrusted,
    process::{group::group, normalize::parameterize},
    scrub::scrub,
};
use proptest::prelude::*;
use serde_json::{Value, json};

// mirror of scrub::KEY (private there).
const SENSITIVE: &str = r"(?i)password|passwd|secret|token|api[_-]?key|authorization|cookie|set-cookie|session|credit|card|ssn|private";

fn key() -> impl Strategy<Value = String> {
    prop_oneof![
        "\\PC{0,10}",
        (
            "[a-z_]{0,4}",
            prop::sample::select(vec![
                "Password",
                "token",
                "API-KEY",
                "Cookie",
                "ssn",
                "x_private"
            ]),
            "[a-z_]{0,4}"
        )
            .prop_map(|(a, b, c)| format!("{a}{b}{c}")),
    ]
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        "\\PC{0,20}".prop_map(Value::from),
        Just(json!(
            "4111 1111 1111 1111 Bearer abcdefghijklmnopqrstuvwxyz"
        )),
    ];
    leaf.prop_recursive(4, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            (key(), inner.clone()).prop_map(|(k, v)| json!([k, v])),
            prop::collection::btree_map(key(), inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// Every object key (or pair-list key) matching SENSITIVE holds "[Filtered]" (or null, which the scrubber leaves).
fn check_filtered(v: &Value, re: &regex::Regex) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                if re.is_match(k) {
                    assert!(x.is_null() || x == "[Filtered]", "{k} = {x}");
                } else {
                    check_filtered(x, re);
                }
            }
        }
        // `[[k, v], ...]` pair lists (headers): a sensitive k means a filtered v.
        Value::Array(a) => {
            for x in a {
                match x.as_array() {
                    Some(p) if p.len() == 2 && p[0].as_str().is_some_and(|k| re.is_match(k)) => {
                        assert_eq!(p[1], "[Filtered]")
                    }
                    _ => check_filtered(x, re),
                }
            }
        }
        _ => {}
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn envelope_never_panics(b in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = parse(&b);
    }

    #[test]
    fn envelope_round_trips(
        items in prop::collection::vec(
            ("[a-z]{1,10}", "\\PC{0,10}", prop::collection::vec(any::<u8>(), 0..64), any::<bool>()),
            0..6,
        ),
    ) {
        let mut buf = br#"{"event_id":"0123456789abcdef0123456789abcdef"}"#.to_vec();
        let mut want = vec![];
        for (ty, extra, mut payload, with_len) in items {
            let mut h = json!({"type": ty, "x": extra});
            if with_len {
                h["length"] = payload.len().into();
            } else {
                payload.retain(|&b| b != b'\n');
            }
            buf.push(b'\n');
            buf.extend(serde_json::to_vec(&h).unwrap());
            buf.push(b'\n');
            buf.extend(&payload);
            want.push((h, payload));
        }
        let env = parse(&buf).unwrap();
        prop_assert_eq!(env.items.len(), want.len());
        for (got, (h, p)) in env.items.iter().zip(&want) {
            prop_assert_eq!(&got.header, h);
            prop_assert_eq!(got.payload, &p[..]);
        }
    }

    #[test]
    fn scrubber_filters_sensitive_keys(extra in json_value(), data in json_value(), ctx in json_value()) {
        let re = regex::Regex::new(SENSITIVE).unwrap();
        let mut ev = json!({"extra": extra, "contexts": ctx, "request": {"data": data}});
        scrub(&mut ev, false);
        check_filtered(&ev["extra"], &re);
        check_filtered(&ev["contexts"], &re);
        check_filtered(&ev["request"]["data"], &re);
    }

    #[test]
    fn scrubber_never_panics(ev in json_value()) {
        let mut ev = ev;
        scrub(&mut ev, true);
    }

    #[test]
    fn grouping_never_panics(s in "\\PC{0,80}", raw in ".{0,40}", ev in json_value()) {
        let _ = parameterize(&s);
        let _ = parameterize(&raw);
        let _ = group(&ev);
        let _ = group(&json!({
            "platform": s, "message": raw,
            "exception": {"values": [{"type": s, "value": raw,
                "stacktrace": {"frames": [{"function": s, "filename": raw, "module": s, "lineno": 1}]}}]},
        }));
    }

    #[test]
    fn untrusted_is_clean(s in any::<String>(), fences in "[`a\\n]{0,30}") {
        for input in [s.as_str(), fences.as_str(), &format!("{s}```{fences}")] {
            let out = untrusted(input);
            prop_assert!(!out.contains("```"), "{:?}", out);
            prop_assert!(out.chars().all(|c| !c.is_control() || c == '\n' || c == '\t'));
        }
    }
}
