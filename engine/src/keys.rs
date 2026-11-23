//! Keys: values written so that comparing the bytes compares the values.
//!
//! A tree orders its entries by their key bytes. A row's primary key, and
//! each index entry, is the row's key values written one after another in
//! this form, so that byte order is the order of the tuples. Each value
//! says where it ends, so one never runs into the next.
//!
//! ```text
//! bool       10, then 0 or 1
//! int        20, then 8 bytes big-endian with the sign bit flipped
//! float      21, then the IEEE bits arranged to sort numerically
//! decimal    22, then sign, exponent and digits (see `decimal`)
//! text       30, then the bytes with 00 written as 00 FF, then 00 00
//! bytes      40, the same
//! uuid       50, then its 16 bytes
//! timestamp  60, then 8 bytes like an int
//! date       70, then 4 bytes like an int
//! json       80, then its text like text
//! list       90, then each item, then 00
//! NULL       F0
//! ```
//!
//! NULL sorts after everything, as Postgres orders it. A descending key is
//! the same bytes inverted.

use database_protocol::decimal::Decimal;
use database_protocol::value::Value;

const BOOL: u8 = 0x10;
const INT: u8 = 0x20;
const FLOAT: u8 = 0x21;
const DECIMAL: u8 = 0x22;
const TEXT: u8 = 0x30;
const BYTES: u8 = 0x40;
const UUID: u8 = 0x50;
const TIMESTAMP: u8 = 0x60;
const DATE: u8 = 0x70;
const JSON: u8 = 0x80;
const LIST: u8 = 0x90;
const NULL: u8 = 0xF0;

fn escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
}

fn decimal(out: &mut Vec<u8>, d: &Decimal) {
    // As 0.d1d2d3… × 10^e with d1 not zero: the sign, then e, then the
    // digits, each plus one so that a shorter number ends below a longer.
    let d = d.trimmed();
    if d.is_zero() {
        out.push(0x02);
        return;
    }
    let digits = d.mantissa().unsigned_abs().to_string();
    let exponent = digits.len() as i32 - i32::from(d.scale());
    let start = out.len();
    out.push(0x03);
    out.extend_from_slice(&((exponent + 0x8000) as u16).to_be_bytes());
    out.extend(digits.bytes().map(|c| c - b'0' + 1));
    out.push(0);
    if d.is_negative() {
        // Larger magnitudes must sort first: invert everything after the
        // sign, which itself goes below zero's.
        out[start] = 0x01;
        for b in &mut out[start + 1..] {
            *b = !*b;
        }
    }
}

fn ascending(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(NULL),
        Value::Bool(b) => out.extend_from_slice(&[BOOL, *b as u8]),
        Value::Int(n) => {
            out.push(INT);
            out.extend_from_slice(&((*n as u64) ^ (1 << 63)).to_be_bytes());
        }
        Value::Float(f) => {
            out.push(FLOAT);
            // Positive numbers get the sign bit set; negative ones are
            // inverted whole, so that more negative sorts lower. -0 is 0.
            let bits = if *f == 0.0 { 0f64.to_bits() } else { f.to_bits() };
            let ordered = if bits >> 63 == 0 { bits | (1 << 63) } else { !bits };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Decimal(d) => {
            out.push(DECIMAL);
            decimal(out, d);
        }
        Value::Text(s) => {
            out.push(TEXT);
            escaped(out, s.as_bytes());
            out.extend_from_slice(&[0, 0]);
        }
        Value::Bytes(b) => {
            out.push(BYTES);
            escaped(out, b);
            out.extend_from_slice(&[0, 0]);
        }
        Value::Uuid(u) => {
            out.push(UUID);
            out.extend_from_slice(u);
        }
        Value::Timestamp(t) => {
            out.push(TIMESTAMP);
            out.extend_from_slice(&((*t as u64) ^ (1 << 63)).to_be_bytes());
        }
        Value::Date(d) => {
            out.push(DATE);
            out.extend_from_slice(&((*d as u32) ^ (1 << 31)).to_be_bytes());
        }
        Value::Json(j) => {
            out.push(JSON);
            escaped(out, j.to_string().as_bytes());
            out.extend_from_slice(&[0, 0]);
        }
        Value::List(items) => {
            out.push(LIST);
            for item in items {
                ascending(out, item);
            }
            out.push(0);
        }
    }
}

/// Appends one key value.
pub fn encode(out: &mut Vec<u8>, value: &Value, desc: bool) {
    let start = out.len();
    ascending(out, value);
    if desc {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

/// The start of every text key that begins with `prefix`: the text's
/// bytes without the end mark.
pub fn encode_text_prefix(out: &mut Vec<u8>, prefix: &str) {
    out.push(TEXT);
    escaped(out, prefix.as_bytes());
}

/// The smallest byte string greater than every string that starts with
/// `prefix`, or `None` if there is none (the prefix is all FF).
pub fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if last < 0xFF {
            out.push(last + 1);
            return Some(out);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn key(v: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        encode(&mut out, v, false);
        out
    }

    fn d(s: &str) -> Value {
        Value::Decimal(s.parse().unwrap())
    }

    /// Each list is in ascending order; the keys must be too.
    #[test]
    fn byte_order_is_value_order() {
        let groups: Vec<Vec<Value>> = vec![
            vec![Value::Bool(false), Value::Bool(true)],
            [i64::MIN, -1_000_000, -1, 0, 1, 255, 256, 1 << 40, i64::MAX].iter().map(|n| Value::Int(*n)).collect(),
            [f64::NEG_INFINITY, -1e300, -2.5, -1e-300, 0.0, 1e-300, 1.0, 2.5, 1e300, f64::INFINITY].iter().map(|f| Value::Float(*f)).collect(),
            ["-1000000", "-12.5", "-12.49", "-0.001", "0", "0.0001", "0.001", "0.12", "0.123", "1", "1.5", "9.99", "10", "10.01", "123456789.123456"].iter().map(|s| d(s)).collect(),
            ["", "\0", "\0\0", "\0a", "a", "a\0", "a\0b", "aa", "ab", "b", "é", "日本"].iter().map(|s| Value::Text(s.to_string())).collect(),
            vec![Value::Bytes(vec![]), Value::Bytes(vec![0]), Value::Bytes(vec![0, 255]), Value::Bytes(vec![1]), Value::Bytes(vec![255]), Value::Bytes(vec![255, 0])],
            vec![Value::Uuid([0; 16]), Value::Uuid([1; 16]), Value::Uuid([255; 16])],
            [i64::MIN, -1, 0, 1_700_000_000_000_000, i64::MAX].iter().map(|n| Value::Timestamp(*n)).collect(),
            [-10_000, -1, 0, 20_000].iter().map(|n| Value::Date(*n)).collect(),
            vec![
                Value::List(vec![]),
                Value::List(vec![Value::Int(1)]),
                Value::List(vec![Value::Int(1), Value::Int(0)]),
                Value::List(vec![Value::Int(2)]),
            ],
        ];
        for group in &groups {
            for pair in group.windows(2) {
                assert_eq!(key(&pair[0]).cmp(&key(&pair[1])), Ordering::Less, "{:?} < {:?}", pair[0], pair[1]);
            }
            // NULL after every value, and before it when descending.
            let last = group.last().unwrap();
            assert!(key(last) < key(&Value::Null));
            let mut desc_null = Vec::new();
            let mut desc_last = Vec::new();
            encode(&mut desc_null, &Value::Null, true);
            encode(&mut desc_last, last, true);
            assert!(desc_null < desc_last);
        }
        assert_eq!(key(&d("1.50")), key(&d("1.5")));
        assert_eq!(key(&Value::Float(-0.0)), key(&Value::Float(0.0)));
    }

    #[test]
    fn tuples_compare_part_by_part() {
        let tuple = |parts: &[Value]| {
            let mut out = Vec::new();
            for p in parts {
                encode(&mut out, p, false);
            }
            out
        };
        let t = |s: &str| Value::Text(s.to_string());
        // A shorter first part never runs into the second.
        assert!(tuple(&[t("a"), t("z")]) < tuple(&[t("ab"), t("a")]));
        assert!(tuple(&[t("a\0"), t("a")]) > tuple(&[t("a"), t("zzz")]));
        assert!(tuple(&[Value::Int(1), t("b")]) < tuple(&[Value::Int(2), t("a")]));
        // A prefix sorts before everything that extends it.
        assert!(tuple(&[t("a")]) < tuple(&[t("a"), Value::Int(i64::MIN)]));
    }

    #[test]
    fn a_text_prefix_spans_exactly_the_texts_that_start_with_it() {
        let mut prefix = Vec::new();
        encode_text_prefix(&mut prefix, "inv");
        let end = successor(&prefix).unwrap();
        for (word, inside) in [("inv", true), ("invoice", true), ("inw", false), ("in", false), ("i", false), ("invz\0", true)] {
            let k = key(&Value::Text(word.into()));
            assert_eq!(k >= prefix && k < end, inside, "{word}");
        }
        assert_eq!(successor(&[1, 255, 255]), Some(vec![2]));
        assert_eq!(successor(&[255, 255]), None);
    }

    #[test]
    fn descending_reverses_the_order() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        encode(&mut a, &Value::Text("apple".into()), true);
        encode(&mut b, &Value::Text("apples".into()), true);
        assert!(a > b);
    }
}
