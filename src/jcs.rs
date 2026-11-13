//! The platform's canonical JSON form (the "Kilimanjaro JCS code-point profile").
//!
//! [`canonicalize`] turns a `serde_json::Value` into the canonical text:
//!
//! - object keys sorted by UTF-8 byte order (the same as Unicode code-point order);
//! - no whitespace;
//! - strings escaped minimally: `"` and `\` with a backslash, the short escapes `\b \t \n \f \r`,
//!   every other control character below U+0020 as lowercase `\u00xx`, `/` left as it is, and every
//!   other code point written literally as UTF-8;
//! - numbers are integers only, written in base 10 with no exponent (`-0` is `0`). A fractional
//!   number, or one whose absolute value is above 9007199254740991 (2^53 - 1), is an error.

use serde_json::Value;
use std::fmt;

/// The largest integer magnitude the profile accepts: 2^53 - 1.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Why a value could not be canonicalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JcsError {
    /// A number with a fractional part (or NaN or an infinity).
    NonIntegerNumber,
    /// An integer whose absolute value is above 2^53 - 1.
    NumericCeilingExceeded,
}

impl fmt::Display for JcsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JcsError::NonIntegerNumber => {
                f.write_str("only integers can be canonicalised (found a fractional number)")
            }
            JcsError::NumericCeilingExceeded => f.write_str(
                "an integer above 9007199254740991 in magnitude cannot be canonicalised",
            ),
        }
    }
}

impl std::error::Error for JcsError {}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_number(n: &serde_json::Number, out: &mut String) -> Result<(), JcsError> {
    if let Some(i) = n.as_i64() {
        if i.unsigned_abs() > MAX_SAFE_INTEGER {
            return Err(JcsError::NumericCeilingExceeded);
        }
        out.push_str(&i.to_string());
        return Ok(());
    }
    if let Some(u) = n.as_u64() {
        if u > MAX_SAFE_INTEGER {
            return Err(JcsError::NumericCeilingExceeded);
        }
        out.push_str(&u.to_string());
        return Ok(());
    }
    // A float: only a whole value inside the ceiling is an integer (so `100.0` is `100`, `-0.0` is `0`).
    let f = n.as_f64().ok_or(JcsError::NonIntegerNumber)?;
    if !f.is_finite() || f.fract() != 0.0 {
        return Err(JcsError::NonIntegerNumber);
    }
    if f.abs() > MAX_SAFE_INTEGER as f64 {
        return Err(JcsError::NumericCeilingExceeded);
    }
    out.push_str(&(f as i64).to_string());
    Ok(())
}

fn write_value(v: &Value, out: &mut String) -> Result<(), JcsError> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(n, out)?,
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // `str` ordering is byte-wise, which for UTF-8 is code-point order.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(&map[*k], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// The canonical text of `value` under the platform profile.
pub fn canonicalize(value: &Value) -> Result<String, JcsError> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whole_floats_inside_the_ceiling_are_integers() {
        let v: Value = serde_json::from_str(r#"{"b":[100.0,-0.0,-0,1e2],"a":null}"#).unwrap();
        assert_eq!(canonicalize(&v).unwrap(), r#"{"a":null,"b":[100,0,0,100]}"#);
    }

    #[test]
    fn fractions_and_big_numbers_are_errors() {
        assert_eq!(canonicalize(&json!(1.5)), Err(JcsError::NonIntegerNumber));
        assert_eq!(
            canonicalize(&json!(9007199254740992u64)),
            Err(JcsError::NumericCeilingExceeded)
        );
        assert_eq!(
            canonicalize(&serde_json::from_str::<Value>("1e300").unwrap()),
            Err(JcsError::NumericCeilingExceeded)
        );
        assert_eq!(
            canonicalize(&json!({"a": [1, {"b": 0.25}]})),
            Err(JcsError::NonIntegerNumber)
        );
    }

    #[test]
    fn keys_sort_by_utf8_bytes_not_utf16_units() {
        // U+FB00 (EF AC 80) sorts before U+1F600 (F0 9F 98 80); in UTF-16 the emoji would come first.
        let v = json!({"\u{1F600}": 1, "\u{FB00}": 2});
        assert_eq!(
            canonicalize(&v).unwrap(),
            "{\"\u{FB00}\":2,\"\u{1F600}\":1}"
        );
    }
}
