//! JavaScript semantics the 0.x app's output depends on: `Number.prototype.toString`, `toFixed`, `JSON.stringify`
//! (number formatting and the key order of objects: integer-like keys first, ascending, then the rest in insertion
//! order), `Math.round`, and string comparison (UTF-16 code units, as `<` and `localeCompare`-free sorts do).
//! The parity harness compares reports made by both apps, so these must match V8 exactly.

use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::fmt::Write as _;

/// `String(x)` for a JS number.
pub fn num_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x < 0.0 {
        return format!("-{}", num_to_string(-x));
    }
    // Shortest round-trip digits (ryu-equivalent: Rust's `{:e}` prints the shortest representation).
    let sci = format!("{:e}", x); // e.g. "1.2345e-7", "5e0"
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32; // number of significant digits
    let n = exp + 1; // position of the decimal point relative to the digits
    let mut out = String::new();
    if k <= n && n <= 21 {
        out.push_str(&digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e >= 0 { '+' } else { '-' });
        let _ = write!(out, "{}", e.abs());
    }
    out
}

/// `x.toFixed(digits)` (0 ≤ digits ≤ 100): the exact binary value rounded, ties away from zero... as V8 does
/// (the spec picks the larger n on a tie, which for negative numbers is applied to the magnitude).
pub fn to_fixed(x: f64, digits: usize) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.abs() >= 1e21 {
        return num_to_string(x);
    }
    let neg = x < 0.0;
    let a = x.abs();
    // The exact decimal expansion of `a` (every f64 has a finite one; 1100 places covers the smallest subnormal).
    let exact = format!("{:.1100}", a);
    let (int_part, frac_part) = exact.split_once('.').unwrap();
    let frac = frac_part.as_bytes();
    let mut kept: Vec<u8> = int_part.bytes().chain(frac[..digits].iter().copied()).collect();
    // Round half up on the exact tail.
    let tail = &frac[digits..];
    let round_up = match tail.first() {
        Some(b) if *b > b'5' => true,
        Some(b'5') => true, // exactly half or more than half: both round up (ties to the larger magnitude)
        _ => false,
    };
    if round_up {
        let mut i = kept.len();
        loop {
            if i == 0 {
                kept.insert(0, b'1');
                break;
            }
            i -= 1;
            if kept[i] == b'9' {
                kept[i] = b'0';
            } else {
                kept[i] += 1;
                break;
            }
        }
    }
    let int_len = kept.len() - digits;
    let mut s = String::from_utf8(kept[..int_len].to_vec()).unwrap();
    // Strip leading zeros of the integer part (keep one).
    let trimmed = s.trim_start_matches('0');
    s = if trimmed.is_empty() { "0".into() } else { trimmed.into() };
    if digits > 0 {
        s.push('.');
        s.push_str(std::str::from_utf8(&kept[int_len..]).unwrap());
    }
    let is_zero = s.bytes().all(|b| b == b'0' || b == b'.');
    if neg && !is_zero {
        format!("-{s}")
    } else if neg && x != 0.0 {
        // (-0.001).toFixed(2) is "-0.00" in JS.
        format!("-{s}")
    } else {
        s
    }
}

/// `Math.round(x)`: rounds half up (towards +∞), -0.5 → -0.
pub fn round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// Whether `k` is an array index as JS orders object keys (canonical uint32 below 2^32 - 1).
pub fn is_index_key(k: &str) -> bool {
    if k.is_empty() || k.len() > 10 {
        return false;
    }
    if k == "0" {
        return true;
    }
    if k.starts_with('0') || !k.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    k.parse::<u64>().map(|n| n < 4_294_967_295).unwrap_or(false)
}

/// An object's keys in JS order.
pub fn ordered_keys(m: &Map<String, Value>) -> Vec<&String> {
    let mut idx: Vec<&String> = m.keys().filter(|k| is_index_key(k)).collect();
    idx.sort_by_key(|k| k.parse::<u64>().unwrap());
    idx.extend(m.keys().filter(|k| !is_index_key(k)));
    idx
}

/// An object's entries in JS order (`Object.entries`).
pub fn entries(m: &Map<String, Value>) -> Vec<(&String, &Value)> {
    ordered_keys(m).into_iter().map(|k| (k, &m[k])).collect()
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_num(out: &mut String, n: &serde_json::Number) {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
    } else if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
    } else {
        let f = n.as_f64().unwrap();
        if f.is_finite() {
            out.push_str(&num_to_string(f));
        } else {
            out.push_str("null");
        }
    }
}

fn write_value(out: &mut String, v: &Value, indent: Option<&str>, depth: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_num(out, n),
        Value::String(s) => write_str(out, s),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, x, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, k) in ordered_keys(m).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_str(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, &m[k], indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, indent: Option<&str>, depth: usize) {
    if let Some(ind) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(ind);
        }
    }
}

/// `JSON.stringify(v)`.
pub fn stringify(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, None, 0);
    out
}

/// `JSON.stringify(v, null, 2)`.
pub fn stringify_pretty(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, Some("  "), 0);
    out
}

/// A JSON number for an f64 the way JS keeps it: integral values as integers (so they print as JS prints them).
pub fn num(x: f64) -> Value {
    if x.is_finite() && x == x.trunc() && x.abs() < 9.007_199_254_740_992e15 {
        Value::from(x as i64)
    } else if x.is_finite() {
        Value::from(x)
    } else {
        Value::Null
    }
}

/// JS `a < b` on strings: UTF-16 code unit order.
pub fn cmp_str(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `Number(v)` for a JSON value, NaN when it isn't a number (or a numeric string).
pub fn to_number(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Null => 0.0,
        _ => f64::NAN,
    }
}

/// `x.toLocaleString('en-US')` for integers (the reports' thousands separators).
pub fn thousands(x: f64) -> String {
    let r = round(x);
    let neg = r < 0.0;
    let s = format!("{}", r.abs() as u64);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_as_v8() {
        assert_eq!(num_to_string(1.0), "1");
        assert_eq!(num_to_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(num_to_string(1e21), "1e+21");
        assert_eq!(num_to_string(1e20), "100000000000000000000");
        assert_eq!(num_to_string(1e-7), "1e-7");
        assert_eq!(num_to_string(0.000001), "0.000001");
        assert_eq!(num_to_string(-2.5), "-2.5");
        assert_eq!(num_to_string(123.456), "123.456");
        assert_eq!(num_to_string(1.5e-10), "1.5e-10");
    }

    #[test]
    fn to_fixed_rounds_as_v8() {
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(1.005, 2), "1.00"); // 1.005 is 1.00499999…
        assert_eq!(to_fixed(1.0, 1), "1.0");
        assert_eq!(to_fixed(-1.25, 1), "-1.3");
        assert_eq!(to_fixed(-0.001, 2), "-0.00");
        assert_eq!(to_fixed(0.0, 2), "0.00");
        assert_eq!(to_fixed(99.99, 1), "100.0");
        assert_eq!(to_fixed(12345.678, 0), "12346");
    }

    #[test]
    fn objects_order_keys_as_v8() {
        let v: Value = serde_json::from_str(r#"{"b":1,"10":2,"a":3,"2":4,"01":5}"#).unwrap();
        assert_eq!(stringify(&v), r#"{"2":4,"10":2,"b":1,"a":3,"01":5}"#);
        assert_eq!(stringify(&serde_json::json!({"x": 1.5, "y": [1, 2.0]})), r#"{"x":1.5,"y":[1,2]}"#);
    }

    #[test]
    fn math_round() {
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(-2.6), -3.0);
    }
}

/// `Date.parse(s)` for the forms the app meets (ISO dates and date-times; a date-time without an offset is local
/// time, a date alone is UTC). None for NaN.
pub fn date_parse(s: &str) -> Option<f64> {
    use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone};
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis() as f64);
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z", "%Y-%m-%dT%H:%M:%S%.fZ", "%Y-%m-%dT%H:%MZ"] {
        if let Ok(d) = DateTime::parse_from_str(&s.replace('Z', "+00:00"), &f.replace('Z', "%:z")) {
            return Some(d.timestamp_millis() as f64);
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis() as f64);
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return Local.from_local_datetime(&n).earliest().map(|d| d.timestamp_millis() as f64);
        }
    }
    None
}

/// JS truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// serde: a number, or null read as NaN (JS writes NaN and ±Infinity as null).
pub fn de_num<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v: Option<f64> = serde::Deserialize::deserialize(d)?;
    Ok(v.unwrap_or(f64::NAN))
}

/// serde: an optional number where null means None.
pub fn de_opt_num<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    serde::Deserialize::deserialize(d)
}

/// `Math.floor` as an index.
pub fn floor_usize(x: f64) -> usize {
    if x.is_finite() && x > 0.0 {
        x.floor() as usize
    } else {
        0
    }
}
