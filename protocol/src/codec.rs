//! How values, definitions, queries and errors are written as bytes: on
//! the wire, and for values and definitions also on disk and in dumps.
//!
//! Integers are little-endian or varints, strings and lists carry their
//! length first, and every enum is one tag byte. Decoding never trusts the
//! input: lengths are checked against what is left, and nesting is capped.

use crate::decimal::Decimal;
use crate::error::{DbError, DbResult, ErrorCode};
use crate::expr::{AggFunc, ArithOp, CmpOp, Expr, Func, SubqueryKind};
use crate::query::*;
use crate::schema::*;
use crate::value::{Type, Value};

/// How deep expressions and queries may nest. Decoding recurses, and the
/// input may come from anyone who can reach the port.
const MAX_DEPTH: u32 = 100;

fn malformed(what: &str) -> DbError {
    DbError::new(ErrorCode::InvalidRequest, format!("malformed message: {what}"))
}

// ---------------------------------------------------------------- writer

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::new() }
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    /// A signed integer, small in either direction staying short.
    pub fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    pub fn bytes(&mut self, v: &[u8]) {
        self.varint(v.len() as u64);
        self.buf.extend_from_slice(v);
    }
    pub fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
    pub fn opt_str(&mut self, v: &Option<String>) {
        match v {
            Some(s) => {
                self.u8(1);
                self.str(s);
            }
            None => self.u8(0),
        }
    }
    pub fn strs(&mut self, v: &[String]) {
        self.varint(v.len() as u64);
        for s in v {
            self.str(s);
        }
    }
}

// ---------------------------------------------------------------- reader

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: u32,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0, depth: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    pub fn position(&self) -> usize {
        self.pos
    }
    fn take(&mut self, n: usize) -> DbResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(malformed("ends early"));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
    pub fn u8(&mut self) -> DbResult<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> DbResult<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(malformed("flag is neither 0 nor 1")),
        }
    }
    pub fn u16(&mut self) -> DbResult<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> DbResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> DbResult<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn varint(&mut self) -> DbResult<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift == 63 && byte > 1 {
                return Err(malformed("number too large"));
            }
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(malformed("number too long"));
            }
        }
    }
    pub fn zigzag(&mut self) -> DbResult<i64> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }
    /// A count of items that each take at least one byte, so a count
    /// larger than what is left is a lie.
    pub fn count(&mut self) -> DbResult<usize> {
        let n = self.varint()?;
        if n > self.remaining() as u64 {
            return Err(malformed("count exceeds the message"));
        }
        Ok(n as usize)
    }
    pub fn bytes(&mut self) -> DbResult<&'a [u8]> {
        let n = self.count()?;
        self.take(n)
    }
    pub fn str(&mut self) -> DbResult<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| malformed("text is not UTF-8"))
    }
    pub fn opt_str(&mut self) -> DbResult<Option<String>> {
        Ok(if self.bool()? { Some(self.str()?) } else { None })
    }
    pub fn strs(&mut self) -> DbResult<Vec<String>> {
        let n = self.count()?;
        (0..n).map(|_| self.str()).collect()
    }
    fn enter(&mut self) -> DbResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(malformed("nested too deeply"));
        }
        Ok(())
    }
    fn leave(&mut self) {
        self.depth -= 1;
    }
    /// Fails unless every byte was read.
    pub fn finish(&self) -> DbResult<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(malformed("bytes left over"))
        }
    }
}

// ---------------------------------------------------------------- values

pub const TAG_NULL: u8 = 0;
pub const TAG_FALSE: u8 = 1;
pub const TAG_TRUE: u8 = 2;
pub const TAG_INT: u8 = 3;
pub const TAG_FLOAT: u8 = 4;
pub const TAG_DECIMAL: u8 = 5;
pub const TAG_TEXT: u8 = 6;
pub const TAG_BYTES: u8 = 7;
pub const TAG_UUID: u8 = 8;
pub const TAG_TIMESTAMP: u8 = 9;
pub const TAG_DATE: u8 = 10;
pub const TAG_JSON: u8 = 11;
pub const TAG_LIST: u8 = 12;

pub fn put_value(w: &mut Writer, v: &Value) {
    match v {
        Value::Null => w.u8(TAG_NULL),
        Value::Bool(false) => w.u8(TAG_FALSE),
        Value::Bool(true) => w.u8(TAG_TRUE),
        Value::Int(n) => {
            w.u8(TAG_INT);
            w.zigzag(*n);
        }
        Value::Float(f) => {
            w.u8(TAG_FLOAT);
            w.u64(f.to_bits());
        }
        Value::Decimal(d) => {
            w.u8(TAG_DECIMAL);
            w.u8(d.scale());
            w.buf.extend_from_slice(&d.mantissa().to_le_bytes());
        }
        Value::Text(s) => {
            w.u8(TAG_TEXT);
            w.str(s);
        }
        Value::Bytes(b) => {
            w.u8(TAG_BYTES);
            w.bytes(b);
        }
        Value::Uuid(u) => {
            w.u8(TAG_UUID);
            w.buf.extend_from_slice(u);
        }
        Value::Timestamp(t) => {
            w.u8(TAG_TIMESTAMP);
            w.zigzag(*t);
        }
        Value::Date(d) => {
            w.u8(TAG_DATE);
            w.zigzag(i64::from(*d));
        }
        Value::Json(j) => {
            w.u8(TAG_JSON);
            w.str(&j.to_string());
        }
        Value::List(items) => {
            w.u8(TAG_LIST);
            w.varint(items.len() as u64);
            for item in items {
                put_value(w, item);
            }
        }
    }
}

pub fn get_value(r: &mut Reader) -> DbResult<Value> {
    let tag = r.u8()?;
    get_value_tagged(r, tag)
}

/// Reads the rest of a value whose tag byte was already read.
pub fn get_value_tagged(r: &mut Reader, tag: u8) -> DbResult<Value> {
    Ok(match tag {
        TAG_NULL => Value::Null,
        TAG_FALSE => Value::Bool(false),
        TAG_TRUE => Value::Bool(true),
        TAG_INT => Value::Int(r.zigzag()?),
        TAG_FLOAT => Value::Float(f64::from_bits(r.u64()?)),
        TAG_DECIMAL => {
            let scale = r.u8()?;
            let mantissa = i128::from_le_bytes(r.take(16)?.try_into().unwrap());
            if scale > crate::decimal::MAX_SCALE {
                return Err(malformed("decimal scale out of range"));
            }
            Value::Decimal(Decimal::new(mantissa, scale))
        }
        TAG_TEXT => Value::Text(r.str()?),
        TAG_BYTES => Value::Bytes(r.bytes()?.to_vec()),
        TAG_UUID => Value::Uuid(r.take(16)?.try_into().unwrap()),
        TAG_TIMESTAMP => Value::Timestamp(r.zigzag()?),
        TAG_DATE => Value::Date(i32::try_from(r.zigzag()?).map_err(|_| malformed("date out of range"))?),
        TAG_JSON => {
            let text = r.bytes()?;
            Value::Json(parse_json(text)?)
        }
        TAG_LIST => {
            r.enter()?;
            let n = r.count()?;
            let items = (0..n).map(|_| get_value(r)).collect::<DbResult<_>>()?;
            r.leave();
            Value::List(items)
        }
        _ => return Err(malformed("unknown value tag")),
    })
}

/// Parses JSON however deep it nests, without overflowing the stack.
pub fn parse_json(text: &[u8]) -> DbResult<serde_json::Value> {
    serde_json::from_slice(text).map_err(|e| DbError::new(ErrorCode::InvalidRequest, format!("invalid JSON: {e}")))
}

pub fn put_type(w: &mut Writer, t: &Type) {
    match t {
        Type::Bool => w.u8(1),
        Type::Int => w.u8(2),
        Type::Float => w.u8(3),
        Type::Decimal(None) => w.u8(4),
        Type::Text => w.u8(5),
        Type::Bytes => w.u8(6),
        Type::Uuid => w.u8(7),
        Type::Timestamp => w.u8(8),
        Type::Date => w.u8(9),
        Type::Json => w.u8(10),
        Type::List(of) => {
            w.u8(11);
            put_type(w, of);
        }
        Type::Decimal(Some((p, s))) => {
            w.u8(12);
            w.u8(*p);
            w.u8(*s);
        }
    }
}

pub fn get_type(r: &mut Reader) -> DbResult<Type> {
    Ok(match r.u8()? {
        1 => Type::Bool,
        2 => Type::Int,
        3 => Type::Float,
        4 => Type::Decimal(None),
        5 => Type::Text,
        6 => Type::Bytes,
        7 => Type::Uuid,
        8 => Type::Timestamp,
        9 => Type::Date,
        10 => Type::Json,
        11 => {
            r.enter()?;
            let of = get_type(r)?;
            r.leave();
            Type::List(Box::new(of))
        }
        12 => {
            let (p, s) = (r.u8()?, r.u8()?);
            if s > p || s > crate::decimal::MAX_SCALE {
                return Err(malformed("decimal precision and scale do not fit"));
            }
            Type::Decimal(Some((p, s)))
        }
        _ => return Err(malformed("unknown type tag")),
    })
}

// ----------------------------------------------------------- expressions

fn put_exprs(w: &mut Writer, exprs: &[Expr]) {
    w.varint(exprs.len() as u64);
    for e in exprs {
        put_expr(w, e);
    }
}

fn get_exprs(r: &mut Reader) -> DbResult<Vec<Expr>> {
    let n = r.count()?;
    (0..n).map(|_| get_expr(r)).collect()
}

fn put_opt_expr(w: &mut Writer, e: &Option<Expr>) {
    match e {
        Some(e) => {
            w.u8(1);
            put_expr(w, e);
        }
        None => w.u8(0),
    }
}

fn get_opt_expr(r: &mut Reader) -> DbResult<Option<Expr>> {
    Ok(if r.bool()? { Some(get_expr(r)?) } else { None })
}

fn put_opt_boxed(w: &mut Writer, e: &Option<Box<Expr>>) {
    match e {
        Some(e) => {
            w.u8(1);
            put_expr(w, e);
        }
        None => w.u8(0),
    }
}

fn get_opt_boxed(r: &mut Reader) -> DbResult<Option<Box<Expr>>> {
    Ok(get_opt_expr(r)?.map(Box::new))
}

pub fn put_expr(w: &mut Writer, e: &Expr) {
    match e {
        Expr::Value(v) => {
            w.u8(1);
            put_value(w, v);
        }
        Expr::Column(table, name) => {
            w.u8(2);
            w.opt_str(table);
            w.str(name);
        }
        Expr::Excluded(name) => {
            w.u8(3);
            w.str(name);
        }
        Expr::Old(name) => {
            w.u8(4);
            w.str(name);
        }
        Expr::Depth => w.u8(5),
        Expr::Rank => w.u8(6),
        Expr::Not(e) => {
            w.u8(7);
            put_expr(w, e);
        }
        Expr::And(parts) => {
            w.u8(8);
            put_exprs(w, parts);
        }
        Expr::Or(parts) => {
            w.u8(9);
            put_exprs(w, parts);
        }
        Expr::Cmp(op, a, b) => {
            w.u8(10);
            w.u8(*op as u8);
            put_expr(w, a);
            put_expr(w, b);
        }
        Expr::IsNull(e, negated) => {
            w.u8(11);
            w.bool(*negated);
            put_expr(w, e);
        }
        Expr::In(e, list, negated) => {
            w.u8(12);
            w.bool(*negated);
            put_expr(w, e);
            put_expr(w, list);
        }
        Expr::Like { expr, pattern, case_insensitive, negated } => {
            w.u8(13);
            w.bool(*case_insensitive);
            w.bool(*negated);
            put_expr(w, expr);
            put_expr(w, pattern);
        }
        Expr::Arith(op, a, b) => {
            w.u8(14);
            w.u8(*op as u8);
            put_expr(w, a);
            put_expr(w, b);
        }
        Expr::Neg(e) => {
            w.u8(15);
            put_expr(w, e);
        }
        Expr::Call(func, args) => {
            w.u8(16);
            w.u16(*func as u16);
            put_exprs(w, args);
        }
        Expr::Case(branches, otherwise) => {
            w.u8(17);
            w.varint(branches.len() as u64);
            for (condition, value) in branches {
                put_expr(w, condition);
                put_expr(w, value);
            }
            put_opt_boxed(w, otherwise);
        }
        Expr::Cast(e, ty, lenient) => {
            w.u8(18);
            w.bool(*lenient);
            put_type(w, ty);
            put_expr(w, e);
        }
        Expr::List(items) => {
            w.u8(19);
            put_exprs(w, items);
        }
        Expr::Agg { func, arg, filter, distinct } => {
            w.u8(20);
            w.u8(*func as u8);
            w.bool(*distinct);
            put_opt_boxed(w, arg);
            put_opt_boxed(w, filter);
        }
        Expr::Subquery(query, kind) => {
            w.u8(21);
            w.u8(*kind as u8);
            put_query(w, query);
        }
    }
}

pub fn get_expr(r: &mut Reader) -> DbResult<Expr> {
    r.enter()?;
    let boxed = |r: &mut Reader| get_expr(r).map(Box::new);
    let e = match r.u8()? {
        1 => Expr::Value(get_value(r)?),
        2 => Expr::Column(r.opt_str()?, r.str()?),
        3 => Expr::Excluded(r.str()?),
        4 => Expr::Old(r.str()?),
        5 => Expr::Depth,
        6 => Expr::Rank,
        7 => Expr::Not(boxed(r)?),
        8 => Expr::And(get_exprs(r)?),
        9 => Expr::Or(get_exprs(r)?),
        10 => {
            let op = CmpOp::from_u8(r.u8()?).ok_or_else(|| malformed("unknown comparison"))?;
            Expr::Cmp(op, boxed(r)?, boxed(r)?)
        }
        11 => {
            let negated = r.bool()?;
            Expr::IsNull(boxed(r)?, negated)
        }
        12 => {
            let negated = r.bool()?;
            Expr::In(boxed(r)?, boxed(r)?, negated)
        }
        13 => {
            let (case_insensitive, negated) = (r.bool()?, r.bool()?);
            Expr::Like { expr: boxed(r)?, pattern: boxed(r)?, case_insensitive, negated }
        }
        14 => {
            let op = ArithOp::from_u8(r.u8()?).ok_or_else(|| malformed("unknown operator"))?;
            Expr::Arith(op, boxed(r)?, boxed(r)?)
        }
        15 => Expr::Neg(boxed(r)?),
        16 => {
            let func = Func::from_u16(r.u16()?).ok_or_else(|| malformed("unknown function"))?;
            Expr::Call(func, get_exprs(r)?)
        }
        17 => {
            let n = r.count()?;
            let branches = (0..n).map(|_| Ok((get_expr(r)?, get_expr(r)?))).collect::<DbResult<_>>()?;
            Expr::Case(branches, get_opt_boxed(r)?)
        }
        18 => {
            let lenient = r.bool()?;
            let ty = get_type(r)?;
            Expr::Cast(boxed(r)?, ty, lenient)
        }
        19 => Expr::List(get_exprs(r)?),
        20 => {
            let func = AggFunc::from_u8(r.u8()?).ok_or_else(|| malformed("unknown aggregate"))?;
            let distinct = r.bool()?;
            Expr::Agg { func, distinct, arg: get_opt_boxed(r)?, filter: get_opt_boxed(r)? }
        }
        21 => {
            let kind = SubqueryKind::from_u8(r.u8()?).ok_or_else(|| malformed("unknown subquery kind"))?;
            Expr::Subquery(Box::new(get_query(r)?), kind)
        }
        _ => return Err(malformed("unknown expression tag")),
    };
    r.leave();
    Ok(e)
}

// ---------------------------------------------------------------- queries

fn put_selects(w: &mut Writer, items: &[Select]) {
    w.varint(items.len() as u64);
    for item in items {
        w.str(&item.name);
        put_expr(w, &item.expr);
    }
}

fn get_selects(r: &mut Reader) -> DbResult<Vec<Select>> {
    let n = r.count()?;
    (0..n).map(|_| Ok(Select { name: r.str()?, expr: get_expr(r)? })).collect()
}

fn put_bound(w: &mut Writer, b: &Bound) {
    match b {
        Bound::Unbounded => w.u8(0),
        Bound::Included(values) => {
            w.u8(1);
            put_exprs(w, values);
        }
        Bound::Excluded(values) => {
            w.u8(2);
            put_exprs(w, values);
        }
    }
}

fn get_bound(r: &mut Reader) -> DbResult<Bound> {
    Ok(match r.u8()? {
        0 => Bound::Unbounded,
        1 => Bound::Included(get_exprs(r)?),
        2 => Bound::Excluded(get_exprs(r)?),
        _ => return Err(malformed("unknown bound")),
    })
}

fn put_ranges(w: &mut Writer, ranges: &[KeyRange]) {
    w.varint(ranges.len() as u64);
    for range in ranges {
        put_bound(w, &range.lower);
        put_bound(w, &range.upper);
        w.bool(range.nulls);
    }
}

fn get_ranges(r: &mut Reader) -> DbResult<Vec<KeyRange>> {
    let n = r.count()?;
    (0..n).map(|_| Ok(KeyRange { lower: get_bound(r)?, upper: get_bound(r)?, nulls: r.bool()? })).collect()
}

fn put_access(w: &mut Writer, a: &Access) {
    match a {
        Access::Primary { ranges, reverse } => {
            w.u8(1);
            w.bool(*reverse);
            put_ranges(w, ranges);
        }
        Access::Index { index, ranges, reverse } => {
            w.u8(2);
            w.str(index);
            w.bool(*reverse);
            put_ranges(w, ranges);
        }
        Access::Walk { start, parent, direction, index, prune, stop, max_depth } => {
            w.u8(3);
            w.varint(start.len() as u64);
            for key in start {
                put_exprs(w, key);
            }
            w.str(parent);
            w.u8(*direction as u8);
            w.opt_str(index);
            put_opt_expr(w, prune);
            put_opt_expr(w, stop);
            w.u32(*max_depth);
        }
        Access::Search { index, prefix, query, match_prefix } => {
            w.u8(4);
            w.str(index);
            put_exprs(w, prefix);
            put_expr(w, query);
            w.bool(*match_prefix);
        }
    }
}

fn get_access(r: &mut Reader) -> DbResult<Access> {
    Ok(match r.u8()? {
        1 => {
            let reverse = r.bool()?;
            Access::Primary { ranges: get_ranges(r)?, reverse }
        }
        2 => {
            let index = r.str()?;
            let reverse = r.bool()?;
            Access::Index { index, ranges: get_ranges(r)?, reverse }
        }
        3 => {
            let n = r.count()?;
            let start = (0..n).map(|_| get_exprs(r)).collect::<DbResult<_>>()?;
            let parent = r.str()?;
            let direction = match r.u8()? {
                0 => Direction::Up,
                1 => Direction::Down,
                _ => return Err(malformed("unknown direction")),
            };
            Access::Walk { start, parent, direction, index: r.opt_str()?, prune: get_opt_expr(r)?, stop: get_opt_expr(r)?, max_depth: r.u32()? }
        }
        4 => Access::Search { index: r.str()?, prefix: get_exprs(r)?, query: get_expr(r)?, match_prefix: r.bool()? },
        _ => return Err(malformed("unknown access")),
    })
}

pub fn put_query(w: &mut Writer, q: &Query) {
    w.str(&q.table);
    w.opt_str(&q.alias);
    put_access(w, &q.access);
    w.varint(q.joins.len() as u64);
    for join in &q.joins {
        w.u8(join.kind as u8);
        put_query(w, &join.query);
    }
    match &q.unnest {
        Some(item) => {
            w.u8(1);
            w.str(&item.name);
            put_expr(w, &item.expr);
        }
        None => w.u8(0),
    }
    put_opt_expr(w, &q.filter);
    put_exprs(w, &q.group_by);
    put_opt_expr(w, &q.having);
    w.varint(q.order_by.len() as u64);
    for order in &q.order_by {
        put_expr(w, &order.expr);
        w.bool(order.desc);
        w.u8(match order.nulls_last {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        });
    }
    w.varint(q.offset);
    match q.limit {
        Some(n) => {
            w.u8(1);
            w.varint(n);
        }
        None => w.u8(0),
    }
    put_selects(w, &q.select);
    w.bool(q.distinct);
    w.bool(q.with_total);
}

pub fn get_query(r: &mut Reader) -> DbResult<Query> {
    r.enter()?;
    let table = r.str()?;
    let alias = r.opt_str()?;
    let access = get_access(r)?;
    let n = r.count()?;
    let mut joins = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = match r.u8()? {
            0 => JoinKind::Inner,
            1 => JoinKind::Left,
            _ => return Err(malformed("unknown join kind")),
        };
        joins.push(Join { kind, query: get_query(r)? });
    }
    let unnest = if r.bool()? { Some(Select { name: r.str()?, expr: get_expr(r)? }) } else { None };
    let filter = get_opt_expr(r)?;
    let group_by = get_exprs(r)?;
    let having = get_opt_expr(r)?;
    let n = r.count()?;
    let mut order_by = Vec::with_capacity(n);
    for _ in 0..n {
        let expr = get_expr(r)?;
        let desc = r.bool()?;
        let nulls_last = match r.u8()? {
            0 => None,
            1 => Some(false),
            2 => Some(true),
            _ => return Err(malformed("unknown null order")),
        };
        order_by.push(Order { expr, desc, nulls_last });
    }
    let offset = r.varint()?;
    let limit = if r.bool()? { Some(r.varint()?) } else { None };
    let select = get_selects(r)?;
    let distinct = r.bool()?;
    let with_total = r.bool()?;
    r.leave();
    Ok(Query { table, alias, access, joins, unnest, filter, group_by, having, order_by, offset, limit, select, distinct, with_total })
}

fn put_assigns(w: &mut Writer, set: &[Assign]) {
    w.varint(set.len() as u64);
    for a in set {
        w.str(&a.column);
        put_expr(w, &a.value);
    }
}

fn get_assigns(r: &mut Reader) -> DbResult<Vec<Assign>> {
    let n = r.count()?;
    (0..n).map(|_| Ok(Assign { column: r.str()?, value: get_expr(r)? })).collect()
}

pub fn put_insert(w: &mut Writer, i: &Insert) {
    w.str(&i.table);
    w.strs(&i.columns);
    match &i.source {
        InsertSource::Values(rows) => {
            w.u8(0);
            w.varint(rows.len() as u64);
            for row in rows {
                put_exprs(w, row);
            }
        }
        InsertSource::Query(q) => {
            w.u8(1);
            put_query(w, q);
        }
    }
    match &i.on_conflict {
        OnConflict::Error => w.u8(0),
        OnConflict::DoNothing { target } => {
            w.u8(1);
            w.opt_str(target);
        }
        OnConflict::DoUpdate { target, set, filter } => {
            w.u8(2);
            w.str(target);
            put_assigns(w, set);
            put_opt_expr(w, filter);
        }
    }
    put_selects(w, &i.returning);
}

pub fn get_insert(r: &mut Reader) -> DbResult<Insert> {
    let table = r.str()?;
    let columns = r.strs()?;
    let source = match r.u8()? {
        0 => {
            let n = r.count()?;
            InsertSource::Values((0..n).map(|_| get_exprs(r)).collect::<DbResult<_>>()?)
        }
        1 => InsertSource::Query(Box::new(get_query(r)?)),
        _ => return Err(malformed("unknown insert source")),
    };
    let on_conflict = match r.u8()? {
        0 => OnConflict::Error,
        1 => OnConflict::DoNothing { target: r.opt_str()? },
        2 => OnConflict::DoUpdate { target: r.str()?, set: get_assigns(r)?, filter: get_opt_expr(r)? },
        _ => return Err(malformed("unknown conflict action")),
    };
    Ok(Insert { table, columns, source, on_conflict, returning: get_selects(r)? })
}

pub fn put_update(w: &mut Writer, u: &Update) {
    put_query(w, &u.rows);
    put_assigns(w, &u.set);
    put_selects(w, &u.returning);
}

pub fn get_update(r: &mut Reader) -> DbResult<Update> {
    Ok(Update { rows: get_query(r)?, set: get_assigns(r)?, returning: get_selects(r)? })
}

pub fn put_delete(w: &mut Writer, d: &Delete) {
    put_query(w, &d.rows);
    put_selects(w, &d.returning);
}

pub fn get_delete(r: &mut Reader) -> DbResult<Delete> {
    Ok(Delete { rows: get_query(r)?, returning: get_selects(r)? })
}

// ----------------------------------------------------------------- schema

pub fn put_column(w: &mut Writer, c: &ColumnDef) {
    w.str(&c.name);
    put_type(w, &c.ty);
    w.bool(c.nullable);
    put_opt_expr(w, &c.default);
    w.bool(c.auto_increment);
}

pub fn get_column(r: &mut Reader) -> DbResult<ColumnDef> {
    Ok(ColumnDef { name: r.str()?, ty: get_type(r)?, nullable: r.bool()?, default: get_opt_expr(r)?, auto_increment: r.bool()? })
}

pub fn put_index(w: &mut Writer, i: &IndexDef) {
    w.str(&i.name);
    w.varint(i.keys.len() as u64);
    for key in &i.keys {
        put_expr(w, &key.expr);
        w.bool(key.desc);
        w.bool(key.each);
    }
    w.bool(i.unique);
    put_opt_expr(w, &i.filter);
    match &i.kind {
        IndexKind::Ordered => w.u8(0),
        IndexKind::FullText { config, fields } => {
            w.u8(1);
            w.u8(*config as u8);
            w.varint(fields.len() as u64);
            for field in fields {
                put_expr(w, &field.expr);
                w.u64(field.weight.to_bits());
            }
        }
    }
}

pub fn get_index(r: &mut Reader) -> DbResult<IndexDef> {
    let name = r.str()?;
    let n = r.count()?;
    let keys = (0..n).map(|_| Ok(IndexKey { expr: get_expr(r)?, desc: r.bool()?, each: r.bool()? })).collect::<DbResult<_>>()?;
    let unique = r.bool()?;
    let filter = get_opt_expr(r)?;
    let kind = match r.u8()? {
        0 => IndexKind::Ordered,
        1 => {
            let config = TextConfig::from_u8(r.u8()?).ok_or_else(|| malformed("unknown text config"))?;
            let n = r.count()?;
            let fields = (0..n).map(|_| Ok(TextField { expr: get_expr(r)?, weight: f64::from_bits(r.u64()?) })).collect::<DbResult<_>>()?;
            IndexKind::FullText { config, fields }
        }
        _ => return Err(malformed("unknown index kind")),
    };
    Ok(IndexDef { name, keys, unique, filter, kind })
}

pub fn put_table(w: &mut Writer, t: &TableDef) {
    w.str(&t.name);
    w.varint(t.columns.len() as u64);
    for c in &t.columns {
        put_column(w, c);
    }
    w.strs(&t.primary_key);
    w.varint(t.indexes.len() as u64);
    for i in &t.indexes {
        put_index(w, i);
    }
    w.varint(t.foreign_keys.len() as u64);
    for fk in &t.foreign_keys {
        w.str(&fk.name);
        w.strs(&fk.columns);
        w.str(&fk.ref_table);
        w.strs(&fk.ref_columns);
        w.u8(fk.on_delete as u8);
    }
    w.varint(t.checks.len() as u64);
    for check in &t.checks {
        w.str(&check.name);
        put_expr(w, &check.expr);
    }
    w.bool(t.volatile);
}

pub fn get_table(r: &mut Reader) -> DbResult<TableDef> {
    let name = r.str()?;
    let n = r.count()?;
    let columns = (0..n).map(|_| get_column(r)).collect::<DbResult<_>>()?;
    let primary_key = r.strs()?;
    let n = r.count()?;
    let indexes = (0..n).map(|_| get_index(r)).collect::<DbResult<_>>()?;
    let n = r.count()?;
    let mut foreign_keys = Vec::with_capacity(n);
    for _ in 0..n {
        foreign_keys.push(ForeignKey {
            name: r.str()?,
            columns: r.strs()?,
            ref_table: r.str()?,
            ref_columns: r.strs()?,
            on_delete: OnDelete::from_u8(r.u8()?).ok_or_else(|| malformed("unknown delete action"))?,
        });
    }
    let n = r.count()?;
    let checks = (0..n).map(|_| Ok(Check { name: r.str()?, expr: get_expr(r)? })).collect::<DbResult<_>>()?;
    Ok(TableDef { name, columns, primary_key, indexes, foreign_keys, checks, volatile: r.bool()? })
}

// ----------------------------------------------------------------- errors

pub fn put_error(w: &mut Writer, e: &DbError) {
    w.u8(e.code as u8);
    w.str(&e.message);
    w.opt_str(&e.constraint);
    w.opt_str(&e.table);
}

pub fn get_error(r: &mut Reader) -> DbResult<DbError> {
    Ok(DbError { code: ErrorCode::from_u8(r.u8()?), message: r.str()?, constraint: r.opt_str()?, table: r.opt_str()? })
}
