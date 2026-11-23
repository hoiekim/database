//! The values a column can hold, and how they compare.

use std::cmp::Ordering;
use std::fmt;

use crate::decimal::Decimal;
use crate::error::{DbError, DbResult, ErrorCode};
use crate::time;

pub type Json = serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// An exact decimal number, for money.
    Decimal(Decimal),
    Text(String),
    Bytes(Vec<u8>),
    Uuid([u8; 16]),
    /// Microseconds since 1970-01-01T00:00:00Z.
    Timestamp(i64),
    /// Days since 1970-01-01.
    Date(i32),
    Json(Json),
    List(Vec<Value>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    Bool,
    Int,
    Float,
    /// An exact decimal. With `(precision, scale)`, as SQL's
    /// `NUMERIC(precision, scale)`: values are rounded to `scale` digits
    /// after the point and refused if they then have more than
    /// `precision` digits in all.
    Decimal(Option<(u8, u8)>),
    Text,
    Bytes,
    Uuid,
    Timestamp,
    Date,
    Json,
    List(Box<Type>),
}

impl Type {
    pub fn list(of: Type) -> Type {
        Type::List(Box::new(of))
    }

    /// `NUMERIC(precision, scale)`.
    pub fn decimal(precision: u8, scale: u8) -> Type {
        Type::Decimal(Some((precision, scale)))
    }

    pub fn name(&self) -> String {
        match self {
            Type::Bool => "bool".into(),
            Type::Int => "int".into(),
            Type::Float => "float".into(),
            Type::Decimal(None) => "decimal".into(),
            Type::Decimal(Some((p, s))) => format!("decimal({p}, {s})"),
            Type::Text => "text".into(),
            Type::Bytes => "bytes".into(),
            Type::Uuid => "uuid".into(),
            Type::Timestamp => "timestamp".into(),
            Type::Date => "date".into(),
            Type::Json => "json".into(),
            Type::List(of) => format!("list of {}", of.name()),
        }
    }
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn type_name(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Bool(_) => "bool".into(),
            Value::Int(_) => "int".into(),
            Value::Float(_) => "float".into(),
            Value::Decimal(_) => "decimal".into(),
            Value::Text(_) => "text".into(),
            Value::Bytes(_) => "bytes".into(),
            Value::Uuid(_) => "uuid".into(),
            Value::Timestamp(_) => "timestamp".into(),
            Value::Date(_) => "date".into(),
            Value::Json(_) => "json".into(),
            Value::List(_) => "list".into(),
        }
    }

    /// Where a value's kind falls when values of different kinds are
    /// sorted together. The three kinds of number share a rank and are
    /// compared by value.
    fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Float(_) | Value::Decimal(_) => 2,
            Value::Text(_) => 3,
            Value::Bytes(_) => 4,
            Value::Uuid(_) => 5,
            Value::Timestamp(_) => 6,
            Value::Date(_) => 7,
            Value::Json(_) => 8,
            Value::List(_) => 9,
        }
    }

    /// Compares two values the way a filter does: `None` when either is
    /// NULL, an error when the kinds cannot be compared. Numbers of
    /// different kinds compare by value, text by its bytes, lists item by
    /// item.
    pub fn compare(&self, other: &Value) -> DbResult<Option<Ordering>> {
        use Value::*;
        Ok(Some(match (self, other) {
            (Null, _) | (_, Null) => return Ok(None),
            (Bool(a), Bool(b)) => a.cmp(b),
            (Int(a), Int(b)) => a.cmp(b),
            (Text(a), Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Bytes(a), Bytes(b)) => a.cmp(b),
            (Uuid(a), Uuid(b)) => a.cmp(b),
            (Timestamp(a), Timestamp(b)) => a.cmp(b),
            (Date(a), Date(b)) => a.cmp(b),
            (Date(a), Timestamp(b)) => (i64::from(*a) * time::DAY_MICROS).cmp(b),
            (Timestamp(a), Date(b)) => a.cmp(&(i64::from(*b) * time::DAY_MICROS)),
            (Json(a), Json(b)) => compare_json(a, b),
            (List(a), List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    match x.compare(y)? {
                        None => return Ok(None),
                        Some(Ordering::Equal) => {}
                        Some(o) => return Ok(Some(o)),
                    }
                }
                a.len().cmp(&b.len())
            }
            (a, b) if a.rank() == 2 && b.rank() == 2 => compare_numbers(a, b),
            (a, b) => {
                return Err(DbError::type_mismatch(format!("cannot compare {} with {}", a.type_name(), b.type_name())));
            }
        }))
    }

    /// A total order over all values, for sorting: never fails, and puts
    /// NULL after everything else. Values of different kinds sort by kind.
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        match (self.is_null(), other.is_null()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        if let (Value::List(a), Value::List(b)) = (self, other) {
            for (x, y) in a.iter().zip(b) {
                let o = x.sort_cmp(y);
                if o != Ordering::Equal {
                    return o;
                }
            }
            return a.len().cmp(&b.len());
        }
        match self.compare(other) {
            Ok(Some(o)) => o,
            _ => self.rank().cmp(&other.rank()),
        }
    }

    /// Equality that treats two NULLs as equal: what grouping, DISTINCT
    /// and `IS NOT DISTINCT FROM` use.
    pub fn same(&self, other: &Value) -> bool {
        match (self.is_null(), other.is_null()) {
            (true, true) => true,
            (false, false) => match (self, other) {
                (Value::List(a), Value::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same(y)),
                _ => matches!(self.compare(other), Ok(Some(Ordering::Equal))),
            },
            _ => false,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            Value::Decimal(d) => Some(d.to_f64()),
            _ => None,
        }
    }

    /// The value as the column type wants it, or an error if it is another
    /// kind. Widens numbers (an int into a float or decimal column), reads
    /// text as a uuid, a timestamp or a date where the column is one, and
    /// puts JSON into its stored form.
    pub fn coerce(self, to: &Type) -> DbResult<Value> {
        let mismatch = |v: &Value| DbError::type_mismatch(format!("expected {}, got {}", to.name(), v.type_name()));
        Ok(match (self, to) {
            (Value::Null, _) => Value::Null,
            (v @ Value::Bool(_), Type::Bool) => v,
            (v @ Value::Int(_), Type::Int) => v,
            (v @ Value::Float(_), Type::Float) => v,
            (Value::Int(n), Type::Float) => Value::Float(n as f64),
            (Value::Decimal(d), Type::Decimal(limits)) => Value::Decimal(fit_decimal(d, limits)?),
            (Value::Int(n), Type::Decimal(limits)) => Value::Decimal(fit_decimal(Decimal::from(n), limits)?),
            (Value::Float(f), Type::Decimal(limits)) => {
                let d = Decimal::from_f64(f).ok_or_else(|| DbError::new(ErrorCode::OutOfRange, "a decimal cannot hold NaN, infinity or a number that large"))?;
                Value::Decimal(fit_decimal(d, limits)?)
            }
            (Value::Text(s), Type::Decimal(limits)) => {
                let d = s.parse().map_err(|_| DbError::type_mismatch(format!("not a decimal number: {s:?}")))?;
                Value::Decimal(fit_decimal(d, limits)?)
            }
            (v @ Value::Text(_), Type::Text) => v,
            (v @ Value::Bytes(_), Type::Bytes) => v,
            (v @ Value::Uuid(_), Type::Uuid) => v,
            (Value::Text(s), Type::Uuid) => {
                Value::Uuid(parse_uuid(&s).ok_or_else(|| DbError::type_mismatch(format!("not a uuid: {s:?}")))?)
            }
            (v @ Value::Timestamp(_), Type::Timestamp) => v,
            (Value::Text(s), Type::Timestamp) => {
                Value::Timestamp(time::parse_timestamp(&s, 0).ok_or_else(|| DbError::type_mismatch(format!("not a timestamp: {s:?}")))?)
            }
            (Value::Date(d), Type::Timestamp) => Value::Timestamp(i64::from(d) * time::DAY_MICROS),
            (v @ Value::Date(_), Type::Date) => v,
            (Value::Text(s), Type::Date) => {
                Value::Date(time::parse_date(&s).ok_or_else(|| DbError::type_mismatch(format!("not a date: {s:?}")))?)
            }
            (Value::Json(j), Type::Json) => Value::Json(normalize_json(j)),
            (Value::List(items), Type::List(of)) => {
                Value::List(items.into_iter().map(|v| v.coerce(of)).collect::<DbResult<_>>()?)
            }
            (v, _) => return Err(mismatch(&v)),
        })
    }
}

/// Rounds a decimal to a column's scale and checks it against the
/// column's precision.
fn fit_decimal(d: Decimal, limits: &Option<(u8, u8)>) -> DbResult<Decimal> {
    let Some((precision, scale)) = *limits else { return Ok(d) };
    let d = d.round(scale);
    let digits = d.mantissa().unsigned_abs().checked_ilog10().map_or(0, |n| n + 1);
    let whole = digits.saturating_sub(u32::from(d.scale()));
    if whole > u32::from(precision.saturating_sub(scale)) {
        return Err(DbError::new(ErrorCode::OutOfRange, format!("{d} does not fit in decimal({precision}, {scale})")));
    }
    // Stored with exactly `scale` digits, so 5 in a (15, 2) column prints 5.00.
    Ok(d.rescaled(scale))
}

fn compare_numbers(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Decimal(x), Value::Decimal(y)) => x.cmp(y),
        (Value::Decimal(x), Value::Int(y)) => x.cmp(&Decimal::from(*y)),
        (Value::Int(x), Value::Decimal(y)) => Decimal::from(*x).cmp(y),
        _ => {
            let (x, y) = (a.as_f64().unwrap_or(f64::NAN), b.as_f64().unwrap_or(f64::NAN));
            x.total_cmp(&y)
        }
    }
}

fn json_rank(v: &Json) -> u8 {
    match v {
        Json::Null => 0,
        Json::Bool(_) => 1,
        Json::Number(_) => 2,
        Json::String(_) => 3,
        Json::Array(_) => 4,
        Json::Object(_) => 5,
    }
}

/// Orders JSON by structure, not by text: objects compare equal whatever
/// order their keys were written in.
pub fn compare_json(a: &Json, b: &Json) -> Ordering {
    match (a, b) {
        (Json::Bool(x), Json::Bool(y)) => x.cmp(y),
        (Json::Number(x), Json::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => x.as_f64().unwrap_or(f64::NAN).total_cmp(&y.as_f64().unwrap_or(f64::NAN)),
        },
        (Json::String(x), Json::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Json::Array(x), Json::Array(y)) => {
            for (p, q) in x.iter().zip(y) {
                let o = compare_json(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        (Json::Object(x), Json::Object(y)) => {
            let mut xs: Vec<_> = x.iter().collect();
            let mut ys: Vec<_> = y.iter().collect();
            xs.sort_by(|p, q| p.0.cmp(q.0));
            ys.sort_by(|p, q| p.0.cmp(q.0));
            for ((k1, v1), (k2, v2)) in xs.iter().zip(&ys) {
                let o = k1.cmp(k2).then_with(|| compare_json(v1, v2));
                if o != Ordering::Equal {
                    return o;
                }
            }
            xs.len().cmp(&ys.len())
        }
        _ => json_rank(a).cmp(&json_rank(b)),
    }
}

/// JSON as it is stored: object keys ordered by length, then by bytes, at
/// every depth, and no key twice. This is the order Postgres' `jsonb`
/// returns, which the apps' responses and their ETags were built on.
pub fn normalize_json(v: Json) -> Json {
    match v {
        Json::Array(items) => Json::Array(items.into_iter().map(normalize_json).collect()),
        Json::Object(map) => {
            let mut entries: Vec<(String, Json)> = map.into_iter().map(|(k, v)| (k, normalize_json(v))).collect();
            entries.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.as_bytes().cmp(b.0.as_bytes())));
            Json::Object(entries.into_iter().collect())
        }
        other => other,
    }
}

/// JSON as text the way Postgres prints `jsonb`: a space after each comma
/// and colon. Filters that match against a JSON value's text see this.
pub fn json_text(v: &Json) -> String {
    fn write(v: &Json, out: &mut String) {
        match v {
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write(item, out);
                }
                out.push(']');
            }
            Json::Object(map) => {
                out.push('{');
                for (i, (k, item)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&Json::String(k.clone()).to_string());
                    out.push_str(": ");
                    write(item, out);
                }
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(v, &mut out);
    out
}

pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    let mut n = 0;
    let mut high: Option<u8> = None;
    for (i, c) in s.bytes().enumerate() {
        if c == b'-' {
            if !matches!(i, 8 | 13 | 18 | 23) || high.is_some() {
                return None;
            }
            continue;
        }
        let digit = (c as char).to_digit(16)? as u8;
        match high.take() {
            None => high = Some(digit),
            Some(h) => {
                if n == 16 {
                    return None;
                }
                out[n] = h << 4 | digit;
                n += 1;
            }
        }
    }
    (n == 16 && high.is_none() && (s.len() == 36 || s.len() == 32)).then_some(out)
}

pub fn format_uuid(u: &[u8; 16]) -> String {
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Decimal(d) => write!(f, "{d}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Bytes(b) => {
                for byte in b {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            Value::Uuid(u) => write!(f, "{}", format_uuid(u)),
            Value::Timestamp(t) => write!(f, "{}", time::format_timestamp(*t)),
            Value::Date(d) => write!(f, "{}", time::format_date(*d)),
            Value::Json(j) => write!(f, "{}", json_text(j)),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
        }
    }
}

// ------------------------------------------------------ into a value

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(v.into())
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Value::Int(v.into())
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<Decimal> for Value {
    fn from(v: Decimal) -> Self {
        Value::Decimal(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<&String> for Value {
    fn from(v: &String) -> Self {
        Value::Text(v.clone())
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Bytes(v)
    }
}
impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Self {
        Value::Bytes(v.to_vec())
    }
}
impl From<[u8; 16]> for Value {
    fn from(v: [u8; 16]) -> Self {
        Value::Uuid(v)
    }
}
impl From<Json> for Value {
    fn from(v: Json) -> Self {
        Value::Json(v)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::List(v)
    }
}
impl From<Vec<String>> for Value {
    fn from(v: Vec<String>) -> Self {
        Value::List(v.into_iter().map(Value::Text).collect())
    }
}
impl From<Vec<&str>> for Value {
    fn from(v: Vec<&str>) -> Self {
        Value::List(v.into_iter().map(Value::from).collect())
    }
}
impl From<Vec<i64>> for Value {
    fn from(v: Vec<i64>) -> Self {
        Value::List(v.into_iter().map(Value::Int).collect())
    }
}
impl From<Vec<[u8; 16]>> for Value {
    fn from(v: Vec<[u8; 16]>) -> Self {
        Value::List(v.into_iter().map(Value::Uuid).collect())
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// A point in time, for `Value::from`: microseconds since the epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn from_millis(ms: i64) -> Self {
        Timestamp(ms * 1000)
    }
    pub fn from_secs(secs: i64) -> Self {
        Timestamp(secs * 1_000_000)
    }
    pub fn micros(self) -> i64 {
        self.0
    }
    pub fn millis(self) -> i64 {
        self.0.div_euclid(1000)
    }
    pub fn plus_secs(self, secs: i64) -> Self {
        Timestamp(self.0 + secs * 1_000_000)
    }
    /// Reads RFC 3339 and the looser forms `time::parse_timestamp` takes.
    pub fn parse(text: &str) -> Option<Self> {
        time::parse_timestamp(text, 0).map(Timestamp)
    }
    /// RFC 3339 in UTC with microseconds: `2026-03-05T10:20:30.123456Z`.
    pub fn to_rfc3339(self) -> String {
        time::format_timestamp(self.0)
    }
    pub fn now() -> Self {
        Timestamp(time::now_micros())
    }
}

impl From<Timestamp> for Value {
    fn from(v: Timestamp) -> Self {
        Value::Timestamp(v.0)
    }
}

impl From<std::time::SystemTime> for Value {
    fn from(v: std::time::SystemTime) -> Self {
        Value::Timestamp(time::system_micros(v))
    }
}

/// A calendar day, for `Value::from`: days since the epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(pub i32);

impl Date {
    pub fn from_ymd(year: i32, month: u32, day: u32) -> Option<Self> {
        time::days_from_civil(year, month, day).map(Date)
    }
    pub fn parse(text: &str) -> Option<Self> {
        time::parse_date(text).map(Date)
    }
    pub fn ymd(self) -> (i32, u32, u32) {
        time::civil_from_days(self.0)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", time::format_date(self.0))
    }
}

impl From<Date> for Value {
    fn from(v: Date) -> Self {
        Value::Date(v.0)
    }
}

/// A uuid, for `Value::from` and for reading one out of a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uuid(pub [u8; 16]);

impl Uuid {
    pub fn parse(text: &str) -> Option<Self> {
        parse_uuid(text).map(Uuid)
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", format_uuid(&self.0))
    }
}

impl From<Uuid> for Value {
    fn from(v: Uuid) -> Self {
        Value::Uuid(v.0)
    }
}

// ----------------------------------------------------- out of a value

/// Reads a Rust value out of a `Value`, for typed access to a row.
pub trait FromValue: Sized {
    fn from_value(v: Value) -> DbResult<Self>;
}

fn wrong<T>(want: &str, got: &Value) -> DbResult<T> {
    Err(DbError::type_mismatch(format!("expected {want}, got {}", got.type_name())))
}

impl FromValue for Value {
    fn from_value(v: Value) -> DbResult<Self> {
        Ok(v)
    }
}
impl FromValue for bool {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Bool(b) => Ok(b),
            other => wrong("bool", &other),
        }
    }
}
impl FromValue for i64 {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Int(n) => Ok(n),
            Value::Timestamp(t) => Ok(t),
            other => wrong("int", &other),
        }
    }
}
impl FromValue for i32 {
    fn from_value(v: Value) -> DbResult<Self> {
        let n = i64::from_value(v)?;
        i32::try_from(n).map_err(|_| DbError::type_mismatch(format!("{n} does not fit in 32 bits")))
    }
}
impl FromValue for u64 {
    fn from_value(v: Value) -> DbResult<Self> {
        let n = i64::from_value(v)?;
        u64::try_from(n).map_err(|_| DbError::type_mismatch(format!("{n} is negative")))
    }
}
impl FromValue for f64 {
    fn from_value(v: Value) -> DbResult<Self> {
        match v.as_f64() {
            Some(f) => Ok(f),
            None => wrong("number", &v),
        }
    }
}
impl FromValue for Decimal {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Decimal(d) => Ok(d),
            Value::Int(n) => Ok(Decimal::from(n)),
            other => wrong("decimal", &other),
        }
    }
}
impl FromValue for String {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Text(s) => Ok(s),
            other => wrong("text", &other),
        }
    }
}
impl FromValue for Vec<u8> {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Bytes(b) => Ok(b),
            other => wrong("bytes", &other),
        }
    }
}
impl FromValue for Uuid {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Uuid(u) => Ok(Uuid(u)),
            other => wrong("uuid", &other),
        }
    }
}
impl FromValue for [u8; 16] {
    fn from_value(v: Value) -> DbResult<Self> {
        Uuid::from_value(v).map(|u| u.0)
    }
}
impl FromValue for Timestamp {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Timestamp(t) => Ok(Timestamp(t)),
            other => wrong("timestamp", &other),
        }
    }
}
impl FromValue for Date {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Date(d) => Ok(Date(d)),
            other => wrong("date", &other),
        }
    }
}
impl FromValue for Json {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Json(j) => Ok(j),
            other => wrong("json", &other),
        }
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::Null => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}
impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(v: Value) -> DbResult<Self> {
        match v {
            Value::List(items) => items.into_iter().map(T::from_value).collect(),
            other => wrong("list", &other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_of_different_kinds_compare_by_value() {
        let one_half = Value::Decimal("1.5".parse().unwrap());
        assert_eq!(Value::Int(2).compare(&Value::Float(2.0)).unwrap(), Some(Ordering::Equal));
        assert_eq!(Value::Int(1).compare(&one_half).unwrap(), Some(Ordering::Less));
        assert_eq!(one_half.compare(&Value::Float(1.25)).unwrap(), Some(Ordering::Greater));
        assert_eq!(Value::Null.compare(&Value::Int(1)).unwrap(), None);
        assert!(Value::Int(1).compare(&Value::Text("1".into())).is_err());
    }

    #[test]
    fn sorting_puts_null_last_and_never_fails() {
        let mut values = vec![Value::Null, Value::Text("b".into()), Value::Int(3), Value::Text("a".into()), Value::Float(1.5)];
        values.sort_by(|a, b| a.sort_cmp(b));
        assert_eq!(values, vec![Value::Float(1.5), Value::Int(3), Value::Text("a".into()), Value::Text("b".into()), Value::Null]);
    }

    #[test]
    fn json_is_stored_with_keys_by_length_then_bytes() {
        let stored = normalize_json(json!({"name": "x", "b": 1, "address": {"zz": 1, "a": 2}, "a": [{"k2": 1, "k": 2}]}));
        assert_eq!(stored.to_string(), r#"{"a":[{"k":2,"k2":1}],"b":1,"name":"x","address":{"a":2,"zz":1}}"#);
        assert_eq!(json_text(&json!(["a", "b", {"k": 1}])), r#"["a", "b", {"k": 1}]"#);
        assert_eq!(compare_json(&json!({"a": 1, "b": 2}), &json!({"b": 2, "a": 1})), Ordering::Equal);
    }

    #[test]
    fn uuids_parse_in_either_case_and_print_lower() {
        let u = parse_uuid("6F9619FF-8B86-D011-B42D-00C04FC964FF").unwrap();
        assert_eq!(format_uuid(&u), "6f9619ff-8b86-d011-b42d-00c04fc964ff");
        assert_eq!(parse_uuid("6f9619ff8b86d011b42d00c04fc964ff"), Some(u));
        assert_eq!(parse_uuid("6f9619ff-8b86-d011-b42d-00c04fc964f"), None);
        assert_eq!(parse_uuid("6f9619ff8-b86-d011-b42d-00c04fc964ff"), None);
    }

    #[test]
    fn coercion_widens_and_refuses() {
        assert_eq!(Value::Int(3).coerce(&Type::Float).unwrap(), Value::Float(3.0));
        assert_eq!(Value::Int(3).coerce(&Type::Decimal(None)).unwrap(), Value::Decimal(Decimal::from(3)));
        let money = Type::decimal(15, 2);
        assert_eq!(Value::Float(12.345).coerce(&money).unwrap().to_string(), "12.35");
        assert_eq!(Value::Int(5).coerce(&money).unwrap().to_string(), "5.00");
        assert_eq!(Value::Text("-0.005".into()).coerce(&money).unwrap().to_string(), "-0.01");
        assert_eq!(Value::Float(9_999_999_999_999.99).coerce(&money).unwrap().to_string(), "9999999999999.99");
        let too_big = Value::Float(10_000_000_000_000.0).coerce(&money).unwrap_err();
        assert_eq!(too_big.code, ErrorCode::OutOfRange);
        assert!(Value::Float(f64::NAN).coerce(&money).is_err());
        assert_eq!(Value::Text("2026-03-05".into()).coerce(&Type::Date).unwrap(), Value::Date(20517));
        assert!(Value::Text("x".into()).coerce(&Type::Int).is_err());
        assert!(Value::Text("2026-02-30".into()).coerce(&Type::Date).is_err());
        let list = Value::from(vec!["a", "b"]).coerce(&Type::list(Type::Text)).unwrap();
        assert_eq!(list, Value::from(vec!["a", "b"]));
        assert!(Value::from(vec![1i64]).coerce(&Type::list(Type::Text)).is_err());
    }
}
