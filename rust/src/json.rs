//! Bounded JSON input. Byte limits alone do not bound recursion, so parsing is
//! guarded by the same 64-level nesting cap before the data ever reaches the
//! deserializer.

pub const MAX_LINE_BYTES: usize = 1024 * 1024;

pub enum ParseError {
    /// Depth cap exceeded; maps to JSON-RPC -32600 with this message.
    Nesting(String),
    /// Syntax / UTF-8 failure; maps to JSON-RPC -32700.
    Syntax,
}

use serde_json::Value;

/// Reject JSON nested deeper than 64 levels. Scans bytes outside string
/// literals only, so bracket characters inside strings do not count.
pub fn check_depth(input: &[u8]) -> Result<(), ParseError> {
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in input {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > 64 {
                    return Err(ParseError::Nesting(
                        "JSON nesting exceeds 64 levels".to_string(),
                    ));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

pub fn parse_line(input: &[u8]) -> Result<serde_json::Value, ParseError> {
    check_depth(input)?;
    serde_json::from_slice(input).map_err(|_| ParseError::Syntax)
}

/// Exact JSON integer (i64 or u64 storage), as `nlohmann::is_number_integer`
/// sees it: floats and booleans do not qualify. Returned as i128 so values
/// above `i64::MAX` never wrap during range comparisons.
pub fn as_integer(value: &serde_json::Value) -> Option<i128> {
    let Value::Number(number) = value else {
        return None;
    };
    if let Some(unsigned) = number.as_u64() {
        return Some(i128::from(unsigned));
    }
    number.as_i64().map(i128::from)
}

/// Serialized size of `value` when it fits in `limit` bytes, else `None`.
/// Counts serializer output directly, so measuring a page never allocates a
/// page-sized string and stops as soon as the limit is exceeded. Uses the same
/// compact serializer as the final tool result, so the crawl's 4 MiB budget
/// matches what the client would receive. Rust's `Value` cannot hold invalid
/// UTF-8, so the serializer itself cannot fail; a write error only means the
/// budget was reached.
pub fn serialized_size_within(value: &Value, limit: usize) -> Option<usize> {
    let mut writer = CountingWriter { written: 0, limit };
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.written)
}

/// Build a JSON object from already-built values. `json!` serializes borrowed
/// expressions, so `json!({"text": big_string})` copies the string; this moves
/// it instead. Use it on hot paths that carry page-sized payloads.
pub fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    let mut map = serde_json::Map::with_capacity(N);
    for (key, value) in entries {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

struct CountingWriter {
    written: usize,
    limit: usize,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.add(bytes.len())?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // Serializer payloads arrive as whole strings/numbers; counting the
        // slice once avoids the default per-write loop.
        self.add(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CountingWriter {
    fn add(&mut self, length: usize) -> std::io::Result<()> {
        self.written = self.written.saturating_add(length);
        if self.written > self.limit {
            return Err(std::io::Error::other("serialized size limit exceeded"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ParseError, as_integer, check_depth, parse_line, serialized_size_within};

    #[test]
    fn deep_nesting_rejected() {
        let mut line = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{"deep":"#.to_vec();
        line.extend(std::iter::repeat_n(b'[', 5000));
        line.push(b'0');
        line.extend(std::iter::repeat_n(b']', 5000));
        line.extend(b"}}");
        assert!(matches!(check_depth(&line), Err(ParseError::Nesting(_))));
    }

    #[test]
    fn brackets_inside_strings_do_not_count() {
        assert!(check_depth(br#""[[[[[[[["""#).is_ok());
        assert!(check_depth(br#"{"a":[1,2,{"b":[]}]} "#).is_ok());
    }

    #[test]
    fn integer_kind() {
        use serde_json::json;
        assert_eq!(as_integer(&json!(-1)), Some(-1));
        assert_eq!(as_integer(&json!(0)), Some(0));
        assert_eq!(as_integer(&json!(2_u64.pow(63))), Some(1 << 63));
        assert_eq!(as_integer(&json!(u64::MAX)), Some(i128::from(u64::MAX)));
        assert_eq!(as_integer(&json!(1.5)), None);
        assert_eq!(as_integer(&json!(true)), None);
        // 2^64 overflows integer parsing and lands in float storage — the same
        // path a raw wire line takes through serde_json.
        let overflow: serde_json::Value = serde_json::from_str("18446744073709551616").unwrap();
        assert_eq!(as_integer(&overflow), None);
        assert!(parse_line(b"{").is_err());
        assert!(parse_line(b"").is_err());
    }

    /// Port of the historical JSON size-boundary suite: every limit boundary
    /// around the exact serialized size, for the same value zoo the C++ suite walks.
    fn check(value: &serde_json::Value) {
        let expected = serde_json::to_string(value).unwrap().len();
        assert_eq!(serialized_size_within(value, expected), Some(expected));
        assert_eq!(serialized_size_within(value, expected + 1), Some(expected));
        assert_eq!(
            serialized_size_within(value, expected.saturating_sub(1)),
            None
        );
        assert_eq!(serialized_size_within(value, 0), None);
        assert_eq!(serialized_size_within(value, usize::MAX), Some(expected));
    }

    #[test]
    fn serialized_size_boundaries_match_dump() {
        use serde_json::{Value, json};
        // nlohmann dumps non-finite doubles as null; serde_json maps
        // non-finite floats to null on serialization, so both land on the
        // same 4-byte payload.
        let infinity = serde_json::to_value(f64::INFINITY).unwrap();
        assert_eq!(infinity, Value::Null);
        for value in [
            Value::Null,
            json!(true),
            json!(false),
            json!(-42),
            json!(u64::MAX),
            json!(1.234e-50),
            infinity,
            json!(""),
            json!("quotes\" slash\\ newline\n\t\u{1} UTF-8: é🐼"),
            json!([]),
            json!({}),
            json!({"nested": [1, "two", Value::Null, [true, false]]}),
        ] {
            check(&value);
        }
        check(&json!({"text": "\n".repeat(256 * 1024)}));
        check(&json!({"text": "x".repeat(4 * 1024 * 1024)}));
        // The C++ suite's invalid-UTF-8 `dump()` error has no Rust equivalent:
        // Value::String is UTF-8 by construction, and serialization of a
        // Value cannot fail.
        assert_eq!(
            serialized_size_within(&json!("\u{fffd}"), 100),
            Some(serde_json::to_string(&json!("\u{fffd}")).unwrap().len())
        );
    }
}
