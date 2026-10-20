use serde_json::Value;

pub struct Item<'a> {
    pub header: Value,
    pub payload: &'a [u8],
}

pub struct Envelope<'a> {
    pub header: Value,
    pub items: Vec<Item<'a>>,
}

fn line(buf: &[u8], pos: usize) -> (&[u8], usize) {
    match buf[pos..].iter().position(|&b| b == b'\n') {
        Some(i) => (&buf[pos..pos + i], pos + i + 1),
        None => (&buf[pos..], buf.len()),
    }
}

fn json(b: &[u8]) -> Result<Value, String> {
    let b = b.strip_suffix(b"\r").unwrap_or(b);
    if b.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_slice(b).map_err(|e| format!("invalid envelope header: {e}"))
}

/// Byte-level Sentry envelope parser: `header\n(item_header\npayload\n)*`, trailing LF optional.
pub fn parse(buf: &[u8]) -> Result<Envelope<'_>, String> {
    let (h, mut pos) = line(buf, 0);
    let header = json(h)?;
    let mut items = Vec::new();
    while pos < buf.len() {
        let (ih, next) = line(buf, pos);
        pos = next;
        if ih.iter().all(u8::is_ascii_whitespace) {
            continue; // tolerate blank lines between items
        }
        let header = json(ih)?;
        let payload = match header.get("length").and_then(Value::as_u64) {
            Some(len) => {
                let n = items.len() + 1;
                let rem = buf.len() - pos;
                let len = usize::try_from(len).unwrap_or(usize::MAX);
                if len > rem {
                    return Err(format!(
                        "item {n}: declared length {len} exceeds remaining {rem} bytes"
                    ));
                }
                let end = pos + len;
                pos = match buf.get(end) {
                    Some(b'\n') => end + 1,
                    None => end,
                    _ => {
                        return Err(format!(
                            "item {n}: expected newline after {len}-byte payload"
                        ));
                    }
                };
                &buf[end - len..end]
            }
            None => {
                let (p, next) = line(buf, pos);
                pos = next;
                p
            }
        };
        items.push(Item { header, payload });
    }
    Ok(Envelope { header, items })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_with_embedded_newline() {
        let b = b"{\"event_id\":\"a\"}\n{\"type\":\"event\",\"length\":8}\n{\"a\"\n:1}\n{\"type\":\"session\"}\n{}\n";
        let e = parse(b).unwrap();
        assert_eq!(e.items.len(), 2);
        assert_eq!(e.items[0].payload, b"{\"a\"\n:1}");
        assert_eq!(e.items[1].header["type"], "session");
    }

    #[test]
    fn length_absent() {
        let e = parse(b"{}\n{\"type\":\"event\"}\n{\"x\":1}\n{\"type\":\"attachment\"}\nabc\n")
            .unwrap();
        assert_eq!(e.items.len(), 2);
        assert_eq!(e.items[0].payload, b"{\"x\":1}");
        assert_eq!(e.items[1].payload, b"abc");
    }

    #[test]
    fn trailing_lf_absent() {
        let e = parse(b"{}\n{\"type\":\"event\",\"length\":2}\n{}").unwrap();
        assert_eq!(e.items[0].payload, b"{}");
        let e = parse(b"{}\n{\"type\":\"event\"}\n{\"y\":2}").unwrap();
        assert_eq!(e.items[0].payload, b"{\"y\":2}");
    }

    #[test]
    fn headers_only() {
        let e = parse(b"{\"event_id\":\"abc\",\"dsn\":\"https://k@h/1\"}").unwrap();
        assert!(e.items.is_empty());
        assert_eq!(e.header["dsn"], "https://k@h/1");
        let e = parse(b"{}\n").unwrap();
        assert!(e.items.is_empty());
    }

    #[test]
    fn bad_length() {
        let e = |b: &[u8]| parse(b).err().unwrap();
        assert_eq!(
            e(b"{}\n{\"type\":\"event\",\"length\":99}\n{}"),
            "item 1: declared length 99 exceeds remaining 2 bytes"
        );
        assert_eq!(
            e(b"{}\n{\"type\":\"event\",\"length\":1}\n{}\n"),
            "item 1: expected newline after 1-byte payload"
        );
    }
}
