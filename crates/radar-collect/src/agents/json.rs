//! Just enough JSON scanning to pull a few fields out of agent logs without an allocation.
//! Lookups find the first occurrence of a quoted key anywhere in the slice, so callers
//! narrow to the enclosing object first when a key name repeats at several depths.

pub fn value<'a>(buf: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let mut pos = 0;
    while let Some(start) = find_key(&buf[pos..], key) {
        let after_key = pos + start + key.len() + 2;
        let rest = skip_ws(&buf[after_key..]);
        if let Some((b':', rest)) = rest.split_first() {
            let rest = skip_ws(rest);
            return Some(&rest[..value_len(rest)]);
        }
        pos = after_key;
    }
    None
}

/// The contents of a string value, without quotes. Escapes are left as written.
pub fn string<'a>(buf: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let v = value(buf, key)?;
    match v {
        [b'"', inner @ .., b'"'] => Some(inner),
        _ => None,
    }
}

pub fn number(buf: &[u8], key: &str) -> Option<f64> {
    std::str::from_utf8(value(buf, key)?).ok()?.parse().ok()
}

pub fn integer(buf: &[u8], key: &str) -> Option<i64> {
    let v = value(buf, key)?;
    match std::str::from_utf8(v).ok()?.parse::<f64>().ok()? {
        f if f.is_finite() => Some(f as i64),
        _ => None,
    }
}

fn find_key(buf: &[u8], key: &str) -> Option<usize> {
    let k = key.as_bytes();
    buf.windows(k.len() + 2)
        .position(|w| w[0] == b'"' && w[w.len() - 1] == b'"' && &w[1..w.len() - 1] == k)
}

fn skip_ws(buf: &[u8]) -> &[u8] {
    let n = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
    &buf[n..]
}

fn string_len(buf: &[u8]) -> usize {
    let mut i = 1;
    while i < buf.len() {
        match buf[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    buf.len()
}

fn value_len(buf: &[u8]) -> usize {
    match buf.first() {
        None => 0,
        Some(b'"') => string_len(buf),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut i = 0;
            while i < buf.len() {
                match buf[i] {
                    b'"' => i += string_len(&buf[i..]),
                    b'{' | b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        i += 1;
                        if depth == 0 {
                            return i;
                        }
                    }
                    _ => i += 1,
                }
            }
            buf.len()
        }
        Some(_) => buf
            .iter()
            .position(|b| matches!(b, b',' | b'}' | b']') || b.is_ascii_whitespace())
            .unwrap_or(buf.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &[u8] = br#"{"type": "event_msg", "payload":{"type":"token_count","info":{"total":{"input_tokens":15,"usage":null}},"rate_limits":{"primary":{"used_percent":10.5,"resets_at":1790887470},"secondary":null},"note":"a \"quoted\" }"}}"#;

    #[test]
    fn finds_values_at_any_depth() {
        assert_eq!(string(DOC, "type"), Some(&b"event_msg"[..]));
        let payload = value(DOC, "payload").unwrap();
        assert_eq!(string(payload, "type"), Some(&b"token_count"[..]));
        assert_eq!(integer(payload, "input_tokens"), Some(15));
        let limits = value(payload, "rate_limits").unwrap();
        let primary = value(limits, "primary").unwrap();
        assert_eq!(number(primary, "used_percent"), Some(10.5));
        assert_eq!(integer(primary, "resets_at"), Some(1790887470));
        assert_eq!(value(limits, "secondary"), Some(&b"null"[..]));
        assert_eq!(value(DOC, "missing"), None);
    }

    #[test]
    fn keys_match_whole_names_and_objects_close_past_strings() {
        assert_eq!(value(DOC, "usage"), Some(&b"null"[..]));
        assert_eq!(
            string(DOC, "note"),
            Some(&br#"a \"quoted\" }"#[..]),
            "escaped quotes stay inside the string"
        );
        let payload = value(DOC, "payload").unwrap();
        assert_eq!(payload.last(), Some(&b'}'));
        assert_eq!(
            payload.len(),
            DOC.len() - b"{\"type\": \"event_msg\", \"payload\":".len() - 1
        );
    }

    #[test]
    fn tolerates_truncated_input() {
        let cut = &DOC[..DOC.len() / 2];
        let _ = value(cut, "payload");
        let _ = value(cut, "rate_limits");
        assert_eq!(value(b"{\"a\":", "a"), Some(&b""[..]));
    }
}
