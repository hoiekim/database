//! Evaluating expressions against rows.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use database_protocol::decimal::Decimal;
use database_protocol::error::{DbError, ErrorCode};
use database_protocol::expr::{ArithOp, CmpOp, Expr, Func, SubqueryKind};
use database_protocol::schema::TextConfig;
use database_protocol::time;
use database_protocol::value::{compare_json, json_text, parse_uuid, Json, Type, Value};

use crate::btree::Pages;
use crate::catalog::{Catalog, Table};
use crate::error::{Error, Result};
use crate::row::Row;
use crate::text;

/// What an evaluation can reach beyond the rows in front of it.
pub struct Ctx<'a> {
    pub pages: &'a dyn Pages,
    pub catalog: &'a Catalog,
    /// The clock, as `now()` reads it throughout the transaction.
    pub now: i64,
    /// When the statement must give up.
    pub deadline: Option<Instant>,
    /// Where a writing transaction's statements draw the values that the
    /// rows do not decide. `None` for a reader, which draws them fresh.
    pub tape: Option<&'a Tape>,
}

/// A value a statement drew that nothing in the database decides: a
/// random id, or the next number of a counter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Drawn {
    Uuid([u8; 16]),
    Serial(i64),
}

/// The values a transaction's statements drew, and the values they are
/// to draw.
///
/// A transaction that ran beside others is run again, alone, when it
/// commits (see `db.rs`), and must then do what it did the first time.
/// So what a statement draws is kept, and the second run is given it to
/// draw again.
#[derive(Default)]
pub struct Tape {
    drawn: Mutex<Vec<Drawn>>,
    given: Mutex<VecDeque<Drawn>>,
}

impl Tape {
    /// What has been drawn since this was last asked.
    pub fn take_drawn(&self) -> Vec<Drawn> {
        std::mem::take(&mut self.drawn.lock().unwrap())
    }

    /// Sets what the next statement draws, in order. Where it draws
    /// more, or something else, it draws fresh.
    pub fn give(&self, values: &[Drawn]) {
        let mut given = self.given.lock().unwrap();
        given.clear();
        given.extend(values);
    }

    pub fn uuid(&self) -> [u8; 16] {
        let given = self.given.lock().unwrap().pop_front();
        let uuid = match given {
            Some(Drawn::Uuid(uuid)) => uuid,
            _ => random_uuid(),
        };
        self.drawn.lock().unwrap().push(Drawn::Uuid(uuid));
        uuid
    }

    /// A counter's next number: the one given, or else what `fresh`
    /// makes of `floor`, where the counter stands. A number given is one
    /// the counter kept for this transaction when it first ran (see
    /// `Store::next_serial`), so it is used wherever the counter has got
    /// to since.
    pub fn serial(&self, floor: i64, fresh: impl FnOnce(i64) -> i64) -> i64 {
        let given = self.given.lock().unwrap().pop_front();
        let number = match given {
            Some(Drawn::Serial(number)) => number,
            _ => fresh(floor),
        };
        self.drawn.lock().unwrap().push(Drawn::Serial(number));
        number
    }
}

impl Ctx<'_> {
    pub fn check_deadline(&self) -> Result<()> {
        match self.deadline {
            Some(deadline) if Instant::now() > deadline => Err(Error::db(ErrorCode::Timeout, "the statement ran past its deadline")),
            _ => Ok(()),
        }
    }
}

/// One table's row in the expression's view.
pub struct Frame<'a> {
    /// The name expressions use for the table: its alias, or its name.
    pub alias: &'a str,
    pub table: &'a Table,
    /// `None` where a left join found no match: every column is NULL.
    pub row: Option<&'a Row>,
}

/// The aggregates' values for the group being output, by the address of
/// the expression node that asked for each.
pub type AggValues = HashMap<*const Expr, Value>;

/// Everything an expression can name.
#[derive(Clone, Copy, Default)]
pub struct Scope<'a> {
    /// The query's own table first, then each joined one.
    pub frames: &'a [Frame<'a>],
    /// The item of `each`, under its name.
    pub item: Option<(&'a str, &'a Value)>,
    /// Values already computed and named: the selected items.
    pub named: Option<(&'a [String], &'a [Value])>,
    /// In an upsert's update: the row that was proposed, by column.
    pub excluded: Option<&'a [Value]>,
    /// In an update's `returning`: the row before.
    pub old: Option<&'a Row>,
    pub depth: Option<i64>,
    pub rank: Option<f64>,
    pub aggs: Option<&'a AggValues>,
    /// The enclosing query's scope, for a subquery or a join.
    pub outer: Option<&'a Scope<'a>>,
}

fn mismatch(message: impl Into<String>) -> Error {
    Error::Db(DbError::type_mismatch(message))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Db(DbError::invalid(message))
}

fn arithmetic(message: impl Into<String>) -> Error {
    Error::db(ErrorCode::Arithmetic, message)
}

/// A column's value in a frame: the stored one, the fill for a column
/// added after the row was written, or NULL where the frame has no row.
pub fn frame_value(ctx: &Ctx, frame: &Frame, i: usize) -> Result<Value> {
    let Some(row) = frame.row else { return Ok(Value::Null) };
    Ok(match row.value(ctx.pages, frame.table.blobs, i)? {
        Some(value) => value.clone(),
        None => frame.table.cols[i].fill.clone(),
    })
}

/// Finds the frame and column a name means: the innermost table that has
/// it, looking outward through enclosing queries.
fn resolve<'s, 'a>(scope: &'s Scope<'a>, table: Option<&str>, name: &str) -> Option<(&'s Frame<'a>, usize)> {
    let mut scope = Some(scope);
    while let Some(s) = scope {
        for frame in s.frames.iter().rev() {
            if table.is_none_or(|t| t == frame.alias) {
                if let Some(i) = frame.table.by_name.get(name) {
                    return Some((frame, *i));
                }
            }
        }
        scope = s.outer;
    }
    None
}

fn column(ctx: &Ctx, scope: &Scope, table: Option<&str>, name: &str) -> Result<Value> {
    if table.is_none() {
        let mut s = Some(scope);
        while let Some(sc) = s {
            if let Some((item, value)) = sc.item {
                if item == name {
                    return Ok(value.clone());
                }
            }
            if let Some((names, values)) = sc.named {
                if let Some(i) = names.iter().position(|n| n == name) {
                    return Ok(values[i].clone());
                }
            }
            // A table's own column hides an outer query's names.
            if sc.frames.iter().any(|f| f.table.by_name.contains_key(name)) {
                break;
            }
            s = sc.outer;
        }
    }
    match resolve(scope, table, name) {
        Some((frame, i)) => frame_value(ctx, frame, i),
        None => Err(Error::Db(DbError::not_found(match table {
            Some(t) => format!("no column {t}.{name}"),
            None => format!("no column {name:?}"),
        }))),
    }
}

/// The row and column a bare column expression names, so that functions
/// on large values can read a part without the whole.
fn column_ref<'s, 'a>(scope: &'s Scope<'a>, expr: &Expr) -> Option<(&'s Frame<'a>, &'a Row, usize)> {
    let Expr::Column(table, name) = expr else { return None };
    if table.is_none() && (scope.item.is_some_and(|(n, _)| n == name) || scope.named.is_some_and(|(names, _)| names.contains(name))) {
        return None;
    }
    let (frame, i) = resolve(scope, table.as_deref(), name)?;
    let row = frame.row?;
    (i < row.stored() && row.blob(i).is_some()).then_some((frame, row, i))
}

/// A condition's value: true, false, or unknown for NULL.
pub fn truth(value: &Value) -> Result<Option<bool>> {
    match value {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        other => Err(mismatch(format!("expected a condition, got {}", other.type_name()))),
    }
}

/// Whether a filter keeps the row: only when the condition is true.
pub fn holds(ctx: &Ctx, scope: &Scope, condition: &Expr) -> Result<bool> {
    Ok(truth(&eval(ctx, scope, condition)?)? == Some(true))
}

/// Reads a text as the kind of the value it is compared with, the way an
/// untyped SQL literal is.
fn harmonize<'v>(a: &'v Value, b: &'v Value) -> Result<(Cow<'v, Value>, Cow<'v, Value>)> {
    let parse = |text: &str, like: &Value| -> Result<Value> {
        let bad = |what: &str| mismatch(format!("{text:?} is not {what}"));
        Ok(match like {
            Value::Uuid(_) => Value::Uuid(parse_uuid(text).ok_or_else(|| bad("a uuid"))?),
            Value::Timestamp(_) => Value::Timestamp(time::parse_timestamp(text, 0).ok_or_else(|| bad("a timestamp"))?),
            Value::Date(_) => Value::Date(time::parse_date(text).ok_or_else(|| bad("a date"))?),
            Value::Decimal(_) => Value::Decimal(text.trim().parse().map_err(|_| bad("a number"))?),
            _ => unreachable!(),
        })
    };
    Ok(match (a, b) {
        (Value::Text(t), Value::Uuid(_) | Value::Timestamp(_) | Value::Date(_) | Value::Decimal(_)) => (Cow::Owned(parse(t, b)?), Cow::Borrowed(b)),
        (Value::Uuid(_) | Value::Timestamp(_) | Value::Date(_) | Value::Decimal(_), Value::Text(t)) => (Cow::Borrowed(a), Cow::Owned(parse(t, a)?)),
        _ => (Cow::Borrowed(a), Cow::Borrowed(b)),
    })
}

/// Compares as a filter does: `None` if either side is NULL.
pub fn compare(a: &Value, b: &Value) -> Result<Option<Ordering>> {
    let (a, b) = harmonize(a, b)?;
    Ok(a.compare(&b)?)
}

fn same(a: &Value, b: &Value) -> Result<bool> {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ok(true),
        (false, false) => Ok(compare(a, b)? == Some(Ordering::Equal)),
        _ => Ok(false),
    }
}

fn text_arg<'v>(value: &'v Value, what: &str) -> Result<&'v str> {
    match value {
        Value::Text(s) => Ok(s),
        other => Err(mismatch(format!("{what} expects text, got {}", other.type_name()))),
    }
}

fn int_arg(value: &Value, what: &str) -> Result<i64> {
    match value {
        Value::Int(n) => Ok(*n),
        other => Err(mismatch(format!("{what} expects an integer, got {}", other.type_name()))),
    }
}

fn list_arg<'v>(value: &'v Value, what: &str) -> Result<&'v [Value]> {
    match value {
        Value::List(items) => Ok(items),
        other => Err(mismatch(format!("{what} expects a list, got {}", other.type_name()))),
    }
}

fn json_arg<'v>(value: &'v Value, what: &str) -> Result<&'v Json> {
    match value {
        Value::Json(j) => Ok(j),
        other => Err(mismatch(format!("{what} expects JSON, got {}", other.type_name()))),
    }
}

pub fn config_arg(value: &Value) -> Result<TextConfig> {
    let name = text_arg(value, "a text search configuration")?;
    TextConfig::from_name(name).ok_or_else(|| invalid(format!("no text search configuration {name:?}")))
}

/// The nearest scope, looking outward, that carries what `pick` asks for.
fn nearest<'s, 'a, T>(scope: &'s Scope<'a>, pick: impl Fn(&'s Scope<'a>) -> Option<T>) -> Option<T> {
    let mut at = Some(scope);
    while let Some(s) = at {
        if let Some(found) = pick(s) {
            return Some(found);
        }
        at = s.outer;
    }
    None
}

pub fn eval(ctx: &Ctx, scope: &Scope, expr: &Expr) -> Result<Value> {
    // While a group is being output, the values of its aggregates and of
    // everything computed from its first row are looked up, not computed.
    if let Some(value) = scope.aggs.and_then(|known| known.get(&(expr as *const Expr))) {
        return Ok(value.clone());
    }
    Ok(match expr {
        Expr::Value(v) => v.clone(),
        Expr::Column(table, name) => column(ctx, scope, table.as_deref(), name)?,
        Expr::Excluded(name) => {
            let found = nearest(scope, |s| s.excluded.zip(s.frames.first()));
            let (values, frame) = found.ok_or_else(|| invalid("excluded(..) is only for an upsert's update"))?;
            values[frame.table.col(name)?].clone()
        }
        Expr::Old(name) => {
            let found = nearest(scope, |s| s.old.zip(s.frames.first()));
            let (row, frame) = found.ok_or_else(|| invalid("old(..) is only for an update's returning"))?;
            frame_value(ctx, &Frame { alias: frame.alias, table: frame.table, row: Some(row) }, frame.table.col(name)?)?
        }
        Expr::Depth => Value::Int(nearest(scope, |s| s.depth).ok_or_else(|| invalid("depth is only for a tree walk"))?),
        Expr::Rank => Value::Float(nearest(scope, |s| s.rank).ok_or_else(|| invalid("rank is only for a full-text search"))?),
        Expr::Not(e) => match truth(&eval(ctx, scope, e)?)? {
            Some(b) => Value::Bool(!b),
            None => Value::Null,
        },
        Expr::And(parts) => {
            let mut unknown = false;
            for part in parts {
                match truth(&eval(ctx, scope, part)?)? {
                    Some(false) => return Ok(Value::Bool(false)),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown {
                Value::Null
            } else {
                Value::Bool(true)
            }
        }
        Expr::Or(parts) => {
            let mut unknown = false;
            for part in parts {
                match truth(&eval(ctx, scope, part)?)? {
                    Some(true) => return Ok(Value::Bool(true)),
                    None => unknown = true,
                    Some(false) => {}
                }
            }
            if unknown {
                Value::Null
            } else {
                Value::Bool(false)
            }
        }
        Expr::Cmp(op, a, b) => {
            let (a, b) = (eval(ctx, scope, a)?, eval(ctx, scope, b)?);
            match op {
                CmpOp::IsDistinctFrom => Value::Bool(!same(&a, &b)?),
                CmpOp::IsNotDistinctFrom => Value::Bool(same(&a, &b)?),
                _ => match compare(&a, &b)? {
                    None => Value::Null,
                    Some(o) => Value::Bool(match op {
                        CmpOp::Eq => o == Ordering::Equal,
                        CmpOp::Ne => o != Ordering::Equal,
                        CmpOp::Lt => o == Ordering::Less,
                        CmpOp::Le => o != Ordering::Greater,
                        CmpOp::Gt => o == Ordering::Greater,
                        _ => o != Ordering::Less,
                    }),
                },
            }
        }
        Expr::IsNull(e, negated) => Value::Bool(eval(ctx, scope, e)?.is_null() != *negated),
        Expr::In(e, list, negated) => {
            let value = eval(ctx, scope, e)?;
            let list = eval(ctx, scope, list)?;
            if list.is_null() {
                return Ok(Value::Null);
            }
            let found = contains(list_arg(&list, "in")?, &value)?;
            match found {
                Some(found) => Value::Bool(found != *negated),
                None => Value::Null,
            }
        }
        Expr::Like { expr, pattern, case_insensitive, negated } => {
            let (value, pattern) = (eval(ctx, scope, expr)?, eval(ctx, scope, pattern)?);
            if value.is_null() || pattern.is_null() {
                return Ok(Value::Null);
            }
            let like = text::Like::new(text_arg(&pattern, "a like pattern")?, *case_insensitive);
            Value::Bool(like.matches(text_arg(&value, "like")?) != *negated)
        }
        Expr::Arith(op, a, b) => arith(*op, &eval(ctx, scope, a)?, &eval(ctx, scope, b)?)?,
        Expr::Neg(e) => match eval(ctx, scope, e)? {
            Value::Null => Value::Null,
            Value::Int(n) => Value::Int(n.checked_neg().ok_or_else(|| arithmetic("integer overflow"))?),
            Value::Float(f) => Value::Float(-f),
            Value::Decimal(d) => Value::Decimal(d.checked_neg().ok_or_else(|| arithmetic("decimal overflow"))?),
            other => return Err(mismatch(format!("cannot negate {}", other.type_name()))),
        },
        Expr::Call(func, args) => call(ctx, scope, *func, args)?,
        Expr::Case(branches, otherwise) => {
            for (condition, value) in branches {
                if holds(ctx, scope, condition)? {
                    return eval(ctx, scope, value);
                }
            }
            match otherwise {
                Some(e) => eval(ctx, scope, e)?,
                None => Value::Null,
            }
        }
        Expr::Cast(e, ty, lenient) => {
            let value = eval(ctx, scope, e)?;
            match cast(value, ty) {
                Ok(v) => v,
                Err(Error::Db(_)) if *lenient => Value::Null,
                Err(e) => return Err(e),
            }
        }
        Expr::List(items) => Value::List(items.iter().map(|e| eval(ctx, scope, e)).collect::<Result<_>>()?),
        Expr::Agg { .. } => return Err(invalid("an aggregate is not allowed here")),
        Expr::Subquery(query, kind) => {
            let rows = crate::exec::subquery(ctx, scope, query, *kind)?;
            match kind {
                SubqueryKind::Exists => Value::Bool(!rows.is_empty()),
                SubqueryKind::Scalar => rows.into_iter().next().unwrap_or(Value::Null),
                SubqueryKind::List => Value::List(rows),
            }
        }
    })
}

/// Whether a list holds a value: `None` when it does not but holds a
/// NULL (or the value is NULL), so the answer is unknown.
fn contains(list: &[Value], value: &Value) -> Result<Option<bool>> {
    // Nothing is in an empty list, not even NULL: `NULL NOT IN ()` is true.
    if list.is_empty() {
        return Ok(Some(false));
    }
    if value.is_null() {
        return Ok(None);
    }
    let mut unknown = false;
    for item in list {
        match compare(item, value)? {
            Some(Ordering::Equal) => return Ok(Some(true)),
            None => unknown = true,
            _ => {}
        }
    }
    Ok(if unknown { None } else { Some(false) })
}

pub fn arith(op: ArithOp, a: &Value, b: &Value) -> Result<Value> {
    use Value::*;
    let overflow = || arithmetic("integer overflow");
    let zero = || arithmetic("division by zero");
    Ok(match (a, b) {
        (Null, _) | (_, Null) => Null,
        (Int(x), Int(y)) => Int(match op {
            ArithOp::Add => x.checked_add(*y).ok_or_else(overflow)?,
            ArithOp::Sub => x.checked_sub(*y).ok_or_else(overflow)?,
            ArithOp::Mul => x.checked_mul(*y).ok_or_else(overflow)?,
            ArithOp::Div => x.checked_div(*y).ok_or_else(zero)?,
            ArithOp::Mod => x.checked_rem(*y).ok_or_else(zero)?,
        }),
        // A moment and a span of microseconds.
        (Timestamp(t), Int(n)) if matches!(op, ArithOp::Add | ArithOp::Sub) => {
            Timestamp(if op == ArithOp::Add { t.checked_add(*n) } else { t.checked_sub(*n) }.ok_or_else(overflow)?)
        }
        (Int(n), Timestamp(t)) if op == ArithOp::Add => Timestamp(t.checked_add(*n).ok_or_else(overflow)?),
        (Timestamp(x), Timestamp(y)) if op == ArithOp::Sub => Int(x.checked_sub(*y).ok_or_else(overflow)?),
        // A day and a number of days.
        (Date(d), Int(n)) if matches!(op, ArithOp::Add | ArithOp::Sub) => {
            let n = i32::try_from(*n).map_err(|_| overflow())?;
            Date(if op == ArithOp::Add { d.checked_add(n) } else { d.checked_sub(n) }.ok_or_else(overflow)?)
        }
        (Date(x), Date(y)) if op == ArithOp::Sub => Int(i64::from(*x) - i64::from(*y)),
        (Decimal(_), Int(_) | Decimal(_)) | (Int(_), Decimal(_)) => {
            let dec = |v: &Value| match v {
                Decimal(d) => *d,
                Int(n) => database_protocol::decimal::Decimal::from(*n),
                _ => unreachable!(),
            };
            let (x, y) = (dec(a), dec(b));
            let out = match op {
                ArithOp::Add => x.checked_add(&y),
                ArithOp::Sub => x.checked_sub(&y),
                ArithOp::Mul => x.checked_mul(&y),
                ArithOp::Div => {
                    if y.is_zero() {
                        return Err(zero());
                    }
                    x.checked_div(&y)
                }
                ArithOp::Mod => return Err(mismatch("the remainder of a decimal is not defined")),
            };
            Decimal(out.ok_or_else(|| arithmetic("decimal overflow"))?)
        }
        (Int(_) | Float(_) | Decimal(_), Int(_) | Float(_) | Decimal(_)) => {
            let (x, y) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            Float(match op {
                ArithOp::Add => x + y,
                ArithOp::Sub => x - y,
                ArithOp::Mul => x * y,
                ArithOp::Div => {
                    if y == 0.0 {
                        return Err(zero());
                    }
                    x / y
                }
                ArithOp::Mod => {
                    if y == 0.0 {
                        return Err(zero());
                    }
                    x % y
                }
            })
        }
        _ => return Err(mismatch(format!("cannot do arithmetic on {} and {}", a.type_name(), b.type_name()))),
    })
}

/// Any value as JSON.
/// A JSON value just built around others, if it has not grown deeper
/// than a JSON value may be: what could not be read back is never made.
fn shallow(json: Json) -> Result<Value> {
    database_protocol::value::check_json_depth(&json)?;
    Ok(Value::Json(json))
}

pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::from(*n),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Decimal(d) => match (d.scale() == 0).then(|| d.to_i64()).flatten() {
            Some(n) => Json::from(n),
            None => serde_json::Number::from_f64(d.to_f64()).map_or(Json::Null, Json::Number),
        },
        Value::Text(s) => Json::String(s.clone()),
        Value::Json(j) => j.clone(),
        Value::List(items) => Json::Array(items.iter().map(to_json).collect()),
        other => Json::String(other.to_string()),
    }
}

/// A JSON scalar as the value it holds. Arrays and objects stay JSON.
fn from_json(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Json(other.clone()),
    }
}

pub fn cast(value: Value, to: &Type) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    // JSON converts through the scalar it holds.
    if let (Value::Json(j), false) = (&value, matches!(to, Type::Json | Type::Text | Type::List(_))) {
        return match from_json(j) {
            Value::Json(_) => Err(mismatch(format!("a JSON array or object is not {}", to.name()))),
            Value::Decimal(_) | Value::Float(_) if matches!(to, Type::Decimal(_)) => Value::Text(j.to_string()).coerce(to).map_err(Error::Db),
            scalar => cast(scalar, to),
        };
    }
    let bad = |v: &Value| mismatch(format!("cannot convert {} to {}", v.type_name(), to.name()));
    Ok(match (to, value) {
        (Type::Text, Value::Text(s)) => Value::Text(s),
        (Type::Text, Value::Json(j)) => Value::Text(json_text(&j)),
        (Type::Text, Value::Bytes(b)) => Value::Text(format!("\\x{}", Value::Bytes(b))),
        (Type::Text, v) => Value::Text(v.to_string()),
        (Type::Int, Value::Int(n)) => Value::Int(n),
        (Type::Int, Value::Bool(b)) => Value::Int(b.into()),
        (Type::Int, Value::Float(f)) => {
            // 2^63 itself does not fit; as a float it is exact, and the
            // largest float below it does.
            let r = f.round();
            if !(r >= -9_223_372_036_854_775_808.0 && r < 9_223_372_036_854_775_808.0) {
                return Err(Error::db(ErrorCode::OutOfRange, format!("{f} does not fit in an integer")));
            }
            Value::Int(r as i64)
        }
        (Type::Int, Value::Decimal(d)) => Value::Int(d.round(0).to_i64().ok_or_else(|| Error::db(ErrorCode::OutOfRange, format!("{d} does not fit in an integer")))?),
        (Type::Int, Value::Text(s)) => Value::Int(s.trim().parse().map_err(|_| mismatch(format!("{s:?} is not an integer")))?),
        (Type::Int, Value::Timestamp(t)) => Value::Int(t),
        (Type::Int, Value::Date(d)) => Value::Int(d.into()),
        (Type::Float, Value::Text(s)) => Value::Float(s.trim().parse().map_err(|_| mismatch(format!("{s:?} is not a number")))?),
        (Type::Float, v @ (Value::Int(_) | Value::Float(_) | Value::Decimal(_))) => Value::Float(v.as_f64().unwrap()),
        (Type::Decimal(_), v @ (Value::Int(_) | Value::Float(_) | Value::Decimal(_) | Value::Text(_))) => v.coerce(to)?,
        (Type::Bool, Value::Bool(b)) => Value::Bool(b),
        (Type::Bool, Value::Int(n)) => Value::Bool(n != 0),
        (Type::Bool, Value::Text(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Value::Bool(true),
            "false" | "f" | "no" | "n" | "off" | "0" => Value::Bool(false),
            _ => return Err(mismatch(format!("{s:?} is not a boolean"))),
        },
        (Type::Timestamp, Value::Int(n)) => Value::Timestamp(n),
        (Type::Date, Value::Timestamp(t)) => Value::Date(t.div_euclid(time::DAY_MICROS) as i32),
        (Type::Date, Value::Int(n)) => Value::Date(i32::try_from(n).map_err(|_| Error::db(ErrorCode::OutOfRange, "date out of range"))?),
        (Type::Uuid | Type::Timestamp | Type::Date, v @ (Value::Text(_) | Value::Uuid(_) | Value::Timestamp(_) | Value::Date(_))) => v.coerce(to)?,
        (Type::Bytes, Value::Bytes(b)) => Value::Bytes(b),
        (Type::Bytes, Value::Text(s)) => Value::Bytes(s.into_bytes()),
        (Type::Json, Value::Text(s)) => Value::Json(database_protocol::codec::parse_json(s.as_bytes())?),
        (Type::Json, v) => Value::Json(to_json(&v)),
        (Type::List(of), Value::List(items)) => Value::List(items.into_iter().map(|v| cast(v, of)).collect::<Result<_>>()?),
        (Type::List(of), Value::Json(Json::Array(items))) => Value::List(items.into_iter().map(|j| cast(Value::Json(j), of)).collect::<Result<_>>()?),
        (_, v) => return Err(bad(&v)),
    })
}

/// Follows a path into JSON: text for an object key, an integer for an
/// array index, negative ones counting from the end.
fn json_path<'j>(mut at: &'j Json, path: &[Value]) -> Result<Option<&'j Json>> {
    for step in path {
        let next = match (at, step) {
            (_, Value::Null) => return Ok(None),
            (Json::Object(map), Value::Text(key)) => map.get(key),
            (Json::Array(items), Value::Int(i)) => {
                let i = if *i < 0 { *i + items.len() as i64 } else { *i };
                usize::try_from(i).ok().and_then(|i| items.get(i))
            }
            (_, Value::Text(_) | Value::Int(_)) => None,
            (_, other) => return Err(mismatch(format!("a JSON path is made of text and integers, not {}", other.type_name()))),
        };
        match next {
            Some(j) => at = j,
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}

/// Postgres' `a @> b`.
fn json_contains(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Object(x), Json::Object(y)) => y.iter().all(|(k, v)| x.get(k).is_some_and(|w| json_contains(w, v))),
        (Json::Array(x), Json::Array(y)) => y.iter().all(|v| x.iter().any(|w| json_contains(w, v))),
        // An array contains a bare scalar that is one of its items.
        (Json::Array(x), y) if !y.is_object() => x.iter().any(|w| compare_json(w, y) == Ordering::Equal),
        (x, y) => !x.is_object() && !x.is_array() && compare_json(x, y) == Ordering::Equal,
    }
}

fn json_has_key(j: &Json, key: &str) -> bool {
    match j {
        Json::Object(map) => map.contains_key(key),
        Json::Array(items) => items.iter().any(|i| i.as_str() == Some(key)),
        Json::String(s) => s == key,
        _ => false,
    }
}

fn json_strip_nulls(j: &Json) -> Json {
    match j {
        Json::Object(map) => Json::Object(map.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), json_strip_nulls(v))).collect()),
        Json::Array(items) => Json::Array(items.iter().map(json_strip_nulls).collect()),
        other => other.clone(),
    }
}

fn random_uuid() -> [u8; 16] {
    use std::io::Read;
    use std::sync::{Mutex, OnceLock};
    static SOURCE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let mut bytes = [0u8; 16];
    let source = SOURCE.get_or_init(|| std::fs::File::open("/dev/urandom").ok().map(Mutex::new));
    let read = source.as_ref().is_some_and(|file| file.lock().unwrap().read_exact(&mut bytes).is_ok());
    if !read {
        // No random device: the clock and an address, mixed. Unique
        // enough for ids, not a secret.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut rng = crate::vfs::Rng::new(time::now_micros() as u64 ^ n.rotate_left(32) ^ (&bytes as *const _ as u64));
        bytes[..8].copy_from_slice(&rng.next().to_le_bytes());
        bytes[8..].copy_from_slice(&rng.next().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    bytes
}

/// The first `n` characters of UTF-8 bytes that may be cut mid-character
/// at the end.
fn first_chars(bytes: &[u8], n: usize) -> String {
    let valid = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""),
    };
    chars(valid, 0, n).to_string()
}

/// The `n` characters of `text` after its first `skip`, or as many as
/// there are: found by where they start and end, and taken in one piece.
fn chars(text: &str, skip: usize, n: usize) -> &str {
    // In plain ASCII a character is a byte.
    if text.is_ascii() {
        let from = skip.min(text.len());
        return &text[from..from.saturating_add(n).min(text.len())];
    }
    let mut starts = text.char_indices().map(|(at, _)| at);
    let Some(from) = starts.nth(skip) else { return "" };
    let to = if n == 0 { from } else { starts.nth(n - 1).unwrap_or(text.len()) };
    &text[from..to]
}

fn substr(text: &str, start: i64, len: i64) -> String {
    // SQL counts from 1, and a start before 1 uses up length.
    let end = start.saturating_add(len.max(0));
    let from = start.max(1);
    if end <= from {
        return String::new();
    }
    chars(text, (from - 1) as usize, (end - from) as usize).to_string()
}

/// Functions that can answer from a large value's length or a part of
/// it. `None` when the argument is not a large stored value.
fn call_on_blob(ctx: &Ctx, scope: &Scope, func: Func, args: &[Expr]) -> Result<Option<Value>> {
    let Some((frame, row, i)) = args.first().and_then(|arg| column_ref(scope, arg)) else { return Ok(None) };
    let rest: Vec<Value> = args[1..].iter().map(|e| eval(ctx, scope, e)).collect::<Result<_>>()?;
    if rest.iter().any(Value::is_null) {
        return Ok(None);
    }
    let blobs = frame.table.blobs;
    Ok(match func {
        Func::OctetLength => row.octet_length(i).map(|n| Value::Int(n as i64)),
        Func::SubstrBytes if rest.len() == 2 => {
            let (start, len) = (int_arg(&rest[0], "substr_bytes")?, int_arg(&rest[1], "substr_bytes")?);
            let end = start.saturating_add(len.max(0));
            let from = start.max(1);
            row.byte_range(ctx.pages, blobs, i, (from - 1) as u64, (end - from).max(0) as u64)?.map(Value::Bytes)
        }
        Func::Left if rest.len() == 1 && row.is_text(i) => {
            let n = int_arg(&rest[0], "left")?;
            if n < 0 {
                return Ok(None);
            }
            // A character is at most four bytes.
            row.byte_range(ctx.pages, blobs, i, 0, (n as u64).saturating_mul(4))?.map(|bytes| Value::Text(first_chars(&bytes, n as usize)))
        }
        Func::Substr if rest.len() == 2 && row.is_text(i) => {
            let (start, len) = (int_arg(&rest[0], "substr")?, int_arg(&rest[1], "substr")?);
            let end = start.saturating_add(len.max(0)).max(1);
            // Everything up to the last character wanted, at most.
            row.byte_range(ctx.pages, blobs, i, 0, (end as u64).saturating_mul(4))?.map(|bytes| {
                let head = match std::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(e) => std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""),
                };
                Value::Text(substr(head, start, len))
            })
        }
        _ => None,
    })
}

fn call(ctx: &Ctx, scope: &Scope, func: Func, args: &[Expr]) -> Result<Value> {
    // These handle NULL themselves or take any number of arguments.
    match func {
        Func::Now => return Ok(Value::Timestamp(ctx.now)),
        Func::RandomUuid => return Ok(Value::Uuid(ctx.tape.map_or_else(random_uuid, Tape::uuid))),
        Func::Coalesce => {
            for arg in args {
                let v = eval(ctx, scope, arg)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        Func::OctetLength | Func::SubstrBytes | Func::Left | Func::Substr => {
            if let Some(value) = call_on_blob(ctx, scope, func, args)? {
                return Ok(value);
            }
        }
        _ => {}
    }
    let a: Vec<Value> = args.iter().map(|e| eval(ctx, scope, e)).collect::<Result<_>>()?;
    let name = format!("{func:?}").to_lowercase();
    let arity = |ok: bool| if ok { Ok(()) } else { Err(invalid(format!("{name} was given {} arguments", a.len()))) };
    match func {
        Func::Least | Func::Greatest => {
            let mut best: Option<&Value> = None;
            for v in a.iter().filter(|v| !v.is_null()) {
                let better = match best {
                    None => true,
                    Some(b) => {
                        let o = compare(v, b)?.unwrap_or(Ordering::Equal);
                        if func == Func::Least {
                            o == Ordering::Less
                        } else {
                            o == Ordering::Greater
                        }
                    }
                };
                if better {
                    best = Some(v);
                }
            }
            return Ok(best.cloned().unwrap_or(Value::Null));
        }
        Func::NullIf => {
            arity(a.len() == 2)?;
            return Ok(if compare(&a[0], &a[1])? == Some(Ordering::Equal) { Value::Null } else { a[0].clone() });
        }
        Func::ListConcat => {
            arity(a.len() == 2)?;
            let mut out = Vec::new();
            for v in &a {
                if !v.is_null() {
                    out.extend_from_slice(list_arg(v, "list_concat")?);
                }
            }
            return Ok(Value::List(out));
        }
        Func::JsonElems => {
            arity(a.len() == 1)?;
            return Ok(Value::List(match &a[0] {
                Value::Json(Json::Array(items)) => items.iter().map(|j| Value::Json(j.clone())).collect(),
                _ => Vec::new(),
            }));
        }
        Func::JsonTexts => {
            arity(a.len() == 1)?;
            return Ok(Value::List(match &a[0] {
                Value::Json(Json::Array(items)) => items
                    .iter()
                    .map(|j| match j {
                        Json::Null => Value::Null,
                        Json::String(s) => Value::Text(s.clone()),
                        other => Value::Text(database_protocol::value::json_text(other)),
                    })
                    .collect(),
                _ => Vec::new(),
            }));
        }
        Func::JsonPluck => {
            arity(a.len() == 2)?;
            let key = text_arg(&a[1], "json_pluck")?;
            return Ok(Value::List(match &a[0] {
                Value::Json(Json::Array(items)) => items
                    .iter()
                    .filter_map(|j| match j.get(key)? {
                        Json::String(s) => Some(Value::Text(s.clone())),
                        Json::Null => None,
                        other => Some(Value::Text(json_text(other))),
                    })
                    .collect(),
                _ => Vec::new(),
            }));
        }
        Func::JsonArray => return shallow(Json::Array(a.iter().map(to_json).collect())),
        Func::JsonObject => {
            arity(a.len() % 2 == 0)?;
            let mut map = serde_json::Map::new();
            for pair in a.chunks(2) {
                map.insert(text_arg(&pair[0], "a JSON object key")?.to_string(), to_json(&pair[1]));
            }
            return shallow(Json::Object(map));
        }
        Func::ToJson => {
            arity(a.len() == 1)?;
            return shallow(to_json(&a[0]));
        }
        Func::JsonGet | Func::JsonGetText => {
            arity(!a.is_empty())?;
            if a[0].is_null() {
                return Ok(Value::Null);
            }
            let found = json_path(json_arg(&a[0], &name)?, &a[1..])?;
            return Ok(match (found, func) {
                (None, _) => Value::Null,
                (Some(j), Func::JsonGet) => Value::Json(j.clone()),
                (Some(Json::Null), _) => Value::Null,
                (Some(Json::String(s)), _) => Value::Text(s.clone()),
                (Some(j), _) => Value::Text(json_text(j)),
            });
        }
        _ => {}
    }
    // Everything below gives NULL for a NULL argument.
    if a.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    Ok(match func {
        Func::Lower => {
            arity(a.len() == 1)?;
            Value::Text(text_arg(&a[0], &name)?.to_lowercase())
        }
        Func::Upper => {
            arity(a.len() == 1)?;
            Value::Text(text_arg(&a[0], &name)?.to_uppercase())
        }
        Func::Length => {
            arity(a.len() == 1)?;
            match &a[0] {
                Value::Text(s) => Value::Int(s.chars().count() as i64),
                Value::Bytes(b) => Value::Int(b.len() as i64),
                Value::List(items) => Value::Int(items.len() as i64),
                other => return Err(mismatch(format!("length expects text, got {}", other.type_name()))),
            }
        }
        Func::OctetLength => {
            arity(a.len() == 1)?;
            match &a[0] {
                Value::Text(s) => Value::Int(s.len() as i64),
                Value::Bytes(b) => Value::Int(b.len() as i64),
                other => return Err(mismatch(format!("octet_length expects text or bytes, got {}", other.type_name()))),
            }
        }
        Func::Trim => {
            arity(a.len() == 1)?;
            Value::Text(text_arg(&a[0], &name)?.trim_matches(' ').to_string())
        }
        Func::Left => {
            arity(a.len() == 2)?;
            let (s, n) = (text_arg(&a[0], &name)?, int_arg(&a[1], &name)?);
            let keep = if n >= 0 { n as usize } else { s.chars().count().saturating_sub(n.unsigned_abs() as usize) };
            Value::Text(chars(s, 0, keep).to_string())
        }
        Func::Substr => {
            arity(a.len() == 3)?;
            Value::Text(substr(text_arg(&a[0], &name)?, int_arg(&a[1], &name)?, int_arg(&a[2], &name)?))
        }
        Func::SubstrBytes => {
            arity(a.len() == 3)?;
            let bytes: &[u8] = match &a[0] {
                Value::Text(s) => s.as_bytes(),
                Value::Bytes(b) => b,
                other => return Err(mismatch(format!("substr_bytes expects text or bytes, got {}", other.type_name()))),
            };
            let (start, len) = (int_arg(&a[1], &name)?, int_arg(&a[2], &name)?);
            let end = (start.saturating_add(len.max(0)).max(1) as usize - 1).min(bytes.len());
            let from = (start.max(1) as usize - 1).min(end);
            Value::Bytes(bytes[from..end].to_vec())
        }
        Func::StrPos => {
            arity(a.len() == 2)?;
            let (s, needle) = (text_arg(&a[0], &name)?, text_arg(&a[1], &name)?);
            Value::Int(s.find(needle).map_or(0, |at| s[..at].chars().count() as i64 + 1))
        }
        Func::Concat => {
            let mut out = String::new();
            for v in &a {
                match v {
                    Value::Text(s) => out.push_str(s),
                    other => out.push_str(&other.to_string()),
                }
            }
            Value::Text(out)
        }
        Func::Replace => {
            arity(a.len() == 3)?;
            Value::Text(text_arg(&a[0], &name)?.replace(text_arg(&a[1], &name)?, text_arg(&a[2], &name)?))
        }
        Func::Hex => {
            arity(a.len() == 1)?;
            match &a[0] {
                v @ Value::Bytes(_) => Value::Text(v.to_string()),
                other => return Err(mismatch(format!("hex expects bytes, got {}", other.type_name()))),
            }
        }
        Func::Similarity => {
            arity(a.len() == 2)?;
            Value::Float(text::similarity(text_arg(&a[0], &name)?, text_arg(&a[1], &name)?))
        }
        Func::Abs | Func::Ceil | Func::Floor | Func::Sign => {
            arity(a.len() == 1)?;
            match (&a[0], func) {
                (Value::Int(n), Func::Abs) => Value::Int(n.checked_abs().ok_or_else(|| arithmetic("integer overflow"))?),
                (Value::Int(n), Func::Sign) => Value::Int(n.signum()),
                (Value::Int(n), _) => Value::Int(*n),
                (Value::Float(f), Func::Abs) => Value::Float(f.abs()),
                (Value::Float(f), Func::Ceil) => Value::Float(f.ceil()),
                (Value::Float(f), Func::Floor) => Value::Float(f.floor()),
                (Value::Float(f), _) => Value::Float(if *f > 0.0 { 1.0 } else if *f < 0.0 { -1.0 } else { 0.0 }),
                (Value::Decimal(d), Func::Abs) => Value::Decimal(d.checked_abs().ok_or_else(|| arithmetic("decimal overflow"))?),
                (Value::Decimal(d), Func::Sign) => Value::Int(d.mantissa().signum() as i64),
                (Value::Decimal(d), _) => {
                    let whole = Decimal::new(d.mantissa().div_euclid(10i128.pow(u32::from(d.scale()))), 0);
                    let exact = whole == *d;
                    Value::Decimal(if func == Func::Ceil && !exact { whole.checked_add(&Decimal::from(1)).unwrap_or(whole) } else { whole })
                }
                (other, _) => return Err(mismatch(format!("{name} expects a number, got {}", other.type_name()))),
            }
        }
        Func::Round => {
            arity(a.len() == 1 || a.len() == 2)?;
            let digits = if a.len() == 2 { int_arg(&a[1], &name)? } else { 0 };
            match &a[0] {
                Value::Int(n) => Value::Int(*n),
                Value::Float(f) => {
                    let scale = 10f64.powi(digits as i32);
                    Value::Float((f * scale).round() / scale)
                }
                Value::Decimal(d) => Value::Decimal(d.round(digits.clamp(0, 30) as u8)),
                other => return Err(mismatch(format!("round expects a number, got {}", other.type_name()))),
            }
        }
        Func::Power => {
            arity(a.len() == 2)?;
            match (a[0].as_f64(), a[1].as_f64()) {
                (Some(x), Some(y)) => Value::Float(x.powf(y)),
                _ => return Err(mismatch("power expects two numbers")),
            }
        }
        Func::ParseTimestamp => {
            arity(a.len() == 1 || a.len() == 2)?;
            let offset = if a.len() == 2 { int_arg(&a[1], &name)? as i32 } else { 0 };
            match &a[0] {
                Value::Text(s) => time::parse_timestamp(s, offset).map_or(Value::Null, Value::Timestamp),
                Value::Timestamp(t) => Value::Timestamp(*t),
                _ => Value::Null,
            }
        }
        Func::DatePart => {
            arity(a.len() == 2)?;
            let part = text_arg(&a[0], &name)?;
            let (days, of_day) = match &a[1] {
                Value::Timestamp(t) => (t.div_euclid(time::DAY_MICROS) as i32, t.rem_euclid(time::DAY_MICROS)),
                Value::Date(d) => (*d, 0),
                other => return Err(mismatch(format!("date_part expects a timestamp or a date, got {}", other.type_name()))),
            };
            let (year, month, day) = time::civil_from_days(days);
            let seconds = of_day / time::SECOND_MICROS;
            match part {
                "year" => Value::Int(year.into()),
                "month" => Value::Int(month.into()),
                "day" => Value::Int(day.into()),
                "hour" => Value::Int(seconds / 3600),
                "minute" => Value::Int(seconds / 60 % 60),
                "second" => Value::Float((of_day % (60 * time::SECOND_MICROS)) as f64 / 1e6),
                "dow" => Value::Int((i64::from(days) + 4).rem_euclid(7)),
                "epoch" => Value::Float(f64::from(days) * 86_400.0 + of_day as f64 / 1e6),
                other => return Err(invalid(format!("date_part does not know {other:?}"))),
            }
        }
        Func::JsonTypeof => {
            arity(a.len() == 1)?;
            Value::Text(
                match json_arg(&a[0], &name)? {
                    Json::Null => "null",
                    Json::Bool(_) => "boolean",
                    Json::Number(_) => "number",
                    Json::String(_) => "string",
                    Json::Array(_) => "array",
                    Json::Object(_) => "object",
                }
                .to_string(),
            )
        }
        Func::JsonLen => {
            arity(a.len() == 1)?;
            match json_arg(&a[0], &name)? {
                Json::Array(items) => Value::Int(items.len() as i64),
                _ => Value::Null,
            }
        }
        Func::JsonContains => {
            arity(a.len() == 2)?;
            Value::Bool(json_contains(json_arg(&a[0], &name)?, json_arg(&a[1], &name)?))
        }
        Func::JsonHasKey => {
            arity(a.len() == 2)?;
            Value::Bool(json_has_key(json_arg(&a[0], &name)?, text_arg(&a[1], &name)?))
        }
        Func::JsonHasAny | Func::JsonHasAll => {
            arity(a.len() == 2)?;
            let j = json_arg(&a[0], &name)?;
            let mut keys = list_arg(&a[1], &name)?.iter().map(|k| text_arg(k, &name));
            Value::Bool(if func == Func::JsonHasAny {
                keys.try_fold(false, |acc, k| k.map(|k| acc || json_has_key(j, k)))?
            } else {
                keys.try_fold(true, |acc, k| k.map(|k| acc && json_has_key(j, k)))?
            })
        }
        Func::JsonMerge => {
            arity(a.len() == 2)?;
            Value::Json(match (json_arg(&a[0], &name)?, json_arg(&a[1], &name)?) {
                (Json::Object(x), Json::Object(y)) => {
                    let mut out = x.clone();
                    for (k, v) in y {
                        out.insert(k.clone(), v.clone());
                    }
                    Json::Object(out)
                }
                (Json::Array(x), Json::Array(y)) => Json::Array(x.iter().chain(y).cloned().collect()),
                (Json::Array(x), y) => Json::Array(x.iter().chain(std::iter::once(y)).cloned().collect()),
                (x, Json::Array(y)) => Json::Array(std::iter::once(x).chain(y).cloned().collect()),
                (x, y) => Json::Array(vec![x.clone(), y.clone()]),
            })
        }
        Func::JsonStripNulls => {
            arity(a.len() == 1)?;
            Value::Json(json_strip_nulls(json_arg(&a[0], &name)?))
        }
        Func::JsonRemove => {
            arity(a.len() == 2)?;
            Value::Json(match (json_arg(&a[0], &name)?, &a[1]) {
                (Json::Object(map), Value::Text(key)) => Json::Object(map.iter().filter(|(k, _)| *k != key).map(|(k, v)| (k.clone(), v.clone())).collect()),
                (Json::Array(items), Value::Text(key)) => Json::Array(items.iter().filter(|i| i.as_str() != Some(key)).cloned().collect()),
                (Json::Array(items), Value::Int(i)) => {
                    let at = if *i < 0 { *i + items.len() as i64 } else { *i };
                    Json::Array(items.iter().enumerate().filter(|(n, _)| *n as i64 != at).map(|(_, v)| v.clone()).collect())
                }
                (j, _) => j.clone(),
            })
        }
        Func::ListLen => {
            arity(a.len() == 1)?;
            Value::Int(list_arg(&a[0], &name)?.len() as i64)
        }
        Func::ListOverlaps => {
            arity(a.len() == 2)?;
            let (x, y) = (list_arg(&a[0], &name)?, list_arg(&a[1], &name)?);
            let mut found = false;
            for item in x {
                if contains(y, item)? == Some(true) {
                    found = true;
                    break;
                }
            }
            Value::Bool(found)
        }
        Func::ListDistinct => {
            arity(a.len() == 1)?;
            let mut out: Vec<Value> = Vec::new();
            for item in list_arg(&a[0], &name)? {
                if !out.iter().any(|o| o.same(item)) {
                    out.push(item.clone());
                }
            }
            Value::List(out)
        }
        Func::ListPosition => {
            arity(a.len() == 2)?;
            let items = list_arg(&a[0], &name)?;
            let mut at = Value::Null;
            for (i, item) in items.iter().enumerate() {
                if compare(item, &a[1])? == Some(Ordering::Equal) {
                    at = Value::Int(i as i64 + 1);
                    break;
                }
            }
            at
        }
        Func::Matches | Func::MatchRank => {
            arity(a.len() == 4)?;
            let prefix = truth(&a[3])?.unwrap_or(false);
            let count = text::match_count(text_arg(&a[0], &name)?, text_arg(&a[1], &name)?, config_arg(&a[2])?, prefix);
            if func == Func::Matches {
                Value::Bool(count.is_some())
            } else {
                Value::Float(f64::from(count.unwrap_or(0)))
            }
        }
        Func::Headline => {
            arity(a.len() == 7)?;
            let words = |v: &Value| int_arg(v, &name).map(|n| n.clamp(1, 10_000) as usize);
            Value::Text(text::headline(
                text_arg(&a[0], &name)?,
                text_arg(&a[1], &name)?,
                config_arg(&a[2])?,
                text_arg(&a[3], &name)?,
                text_arg(&a[4], &name)?,
                words(&a[5])?,
                words(&a[6])?,
            ))
        }
        Func::Now | Func::RandomUuid | Func::Coalesce | Func::Least | Func::Greatest | Func::NullIf | Func::ListConcat | Func::JsonElems | Func::JsonTexts | Func::JsonPluck
        | Func::JsonArray | Func::JsonObject | Func::ToJson | Func::JsonGet | Func::JsonGetText => unreachable!("handled above"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_are_cut_where_counting_them_one_by_one_would() {
        for text in ["", "a", "plain ascii text", "héllo wörld", "日本語のテキスト", "mixed: é日x𝄞y", "𝄞𝄞𝄞"] {
            let count = text.chars().count();
            for skip in 0..count + 3 {
                for n in 0..count + 3 {
                    let want: String = text.chars().skip(skip).take(n).collect();
                    assert_eq!(chars(text, skip, n), want, "{text:?} skip {skip} take {n}");
                }
            }
        }
        // Bytes cut in the middle of a character end at the last whole one.
        let bytes = "aé日".as_bytes();
        assert_eq!(first_chars(&bytes[..bytes.len() - 1], 10), "aé");
        assert_eq!(first_chars(bytes, 2), "aé");
        assert_eq!(substr("héllo", 2, 3), "éll");
        assert_eq!(substr("héllo", 0, 3), "hé");
        assert_eq!(substr("héllo", 9, 3), "");
    }
}
