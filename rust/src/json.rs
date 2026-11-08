//! Bounded JSON input. Mirrors `src/json.hpp`: byte limits alone do not bound
//! recursion, so parsing is guarded by the same 64-level nesting cap before
//! the data ever reaches the deserializer.

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
/// above i64::MAX never wrap during range comparisons.
pub fn as_integer(value: &serde_json::Value) -> Option<i128> {
    let Value::Number(number) = value else {
        return None;
    };
    if let Some(unsigned) = number.as_u64() {
        return Some(unsigned as i128);
    }
    number.as_i64().map(|signed| signed as i128)
}

#[cfg(test)]
mod tests {
    use super::{ParseError, as_integer, check_depth, parse_line};

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
        assert_eq!(as_integer(&json!(u64::MAX)), Some(u64::MAX as i128));
        assert_eq!(as_integer(&json!(1.5)), None);
        assert_eq!(as_integer(&json!(true)), None);
        // 2^64 overflows integer parsing and lands in float storage — the same
        // path a raw wire line takes through serde_json.
        let overflow: serde_json::Value = serde_json::from_str("18446744073709551616").unwrap();
        assert_eq!(as_integer(&overflow), None);
        assert!(parse_line(b"{").is_err());
        assert!(parse_line(b"").is_err());
    }
}
