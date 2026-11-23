//! Running queries.
//!
//! A query is carried out as written: read the rows its access path names,
//! in key order; for each, run the joins; repeat it per item of `each`;
//! test the filter; then group, sort, cut and select. Nothing is
//! reordered or chosen: there is no planner.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use database_protocol::error::{DbError, ErrorCode};
use database_protocol::expr::{AggFunc, ArithOp, Expr, SubqueryKind};
use database_protocol::message::RowSet;
use database_protocol::query::*;
use database_protocol::schema::IndexKind;
use database_protocol::value::{Type, Value};

use crate::btree::{self, Cursor};
use crate::catalog::{Table, TableMeta};
use crate::error::{Error, Result};
use crate::eval::{self, eval, holds, AggValues, Ctx, Frame, Scope};
use crate::keys;
use crate::row::Row;

fn invalid(message: impl Into<String>) -> Error {
    Error::Db(DbError::invalid(message))
}

/// A row an access path produced.
pub struct Hit {
    /// The row's primary key, as stored.
    pub pk: Vec<u8>,
    pub row: Row,
    pub depth: Option<i64>,
    pub rank: Option<f64>,
}

/// The type and direction of each part of a key, for reading bounds.
pub type KeyParts = Vec<(Option<Type>, bool)>;

/// The type a key value or a bound is read as. A decimal is read as a
/// decimal but not rounded to its column's scale: a stored value is at
/// that scale already, and a bound must keep its digits, or `= 1.005`
/// would find the row that holds 1.01.
fn key_type(ty: &Type) -> Type {
    match ty {
        Type::Decimal(_) => Type::Decimal(None),
        other => other.clone(),
    }
}

pub fn pk_parts(table: &Table) -> KeyParts {
    table.pk.iter().map(|i| (Some(key_type(&table.cols[*i].def.ty)), false)).collect()
}

/// An index's keys followed by the primary key, which every entry ends
/// with, so a bound may name a whole entry.
pub fn index_parts(table: &Table, index: usize) -> KeyParts {
    let mut parts: KeyParts = table.indexes[index]
        .def
        .keys
        .iter()
        .map(|key| {
            // A plain column's type is known; so is the type of each
            // item of a list column. An expression's is not, and its
            // bounds are taken as they are given.
            let ty = match &key.expr {
                Expr::Column(None, name) => table.by_name.get(name).and_then(|i| match (&table.cols[*i].def.ty, key.each) {
                    (Type::List(item), true) => Some(key_type(item)),
                    (_, true) => None,
                    (ty, false) => Some(key_type(ty)),
                }),
                _ => None,
            };
            (ty, key.desc)
        })
        .collect();
    parts.extend(pk_parts(table));
    parts
}

/// The bytes of a key prefix from its values.
pub fn encode_prefix(values: Vec<Value>, parts: &KeyParts) -> Result<Vec<u8>> {
    if values.len() > parts.len() {
        return Err(invalid(format!("a bound of {} values for a key of {} parts", values.len(), parts.len())));
    }
    let mut out = Vec::new();
    for (value, (ty, desc)) in values.into_iter().zip(parts) {
        let value = match ty {
            Some(ty) => value.coerce(ty)?,
            None => value,
        };
        keys::encode(&mut out, &value, *desc);
    }
    Ok(out)
}

fn bound_prefix(ctx: &Ctx, scope: &Scope, exprs: &[Expr], parts: &KeyParts) -> Result<Vec<u8>> {
    encode_prefix(exprs.iter().map(|e| eval(ctx, scope, e)).collect::<Result<_>>()?, parts)
}

/// A range as the first key to read and the first key not to: `None` for
/// the tree's own ends. An empty range is `Ok(None)`.
fn range_bytes(ctx: &Ctx, scope: &Scope, range: &KeyRange, parts: &KeyParts) -> Result<Option<(Option<Vec<u8>>, Option<Vec<u8>>)>> {
    // The bytes of a bound, or `None` if it holds a NULL that is to
    // match nothing, as a comparison with NULL does.
    let prefix = |exprs: &[Expr]| -> Result<Option<Vec<u8>>> {
        let values = exprs.iter().map(|e| eval(ctx, scope, e)).collect::<Result<Vec<_>>>()?;
        if !range.nulls && values.iter().any(Value::is_null) {
            return Ok(None);
        }
        encode_prefix(values, parts).map(Some)
    };
    let start = match &range.lower {
        Bound::Unbounded => None,
        Bound::Included(exprs) => match prefix(exprs)? {
            Some(bytes) => Some(bytes),
            None => return Ok(None),
        },
        Bound::Excluded(exprs) => match prefix(exprs)?.and_then(|bytes| keys::successor(&bytes)) {
            Some(next) => Some(next),
            None => return Ok(None),
        },
    };
    let end = match &range.upper {
        Bound::Unbounded => None,
        Bound::Included(exprs) => match prefix(exprs)? {
            Some(bytes) => keys::successor(&bytes),
            None => return Ok(None),
        },
        Bound::Excluded(exprs) => match prefix(exprs)? {
            Some(bytes) => Some(bytes),
            None => return Ok(None),
        },
    };
    if let (Some(s), Some(e)) = (&start, &end) {
        if s >= e {
            return Ok(None);
        }
    }
    Ok(Some((start, end)))
}

/// Walks a tree's entries between two keys, either way, until `f` says
/// stop. Returns whether it ran to the end.
fn walk_range(
    ctx: &Ctx,
    root: u32,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
    reverse: bool,
    f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
) -> Result<bool> {
    let mut cursor = Cursor::new(ctx.pages, root);
    if reverse {
        match end {
            Some(e) => cursor.seek_back(e, false)?,
            None => cursor.last()?,
        }
    } else {
        match start {
            Some(s) => cursor.seek(s)?,
            None => cursor.first()?,
        }
    }
    let mut n = 0u32;
    while let (Some(key), Some(value)) = (cursor.key(), cursor.value()) {
        let inside = if reverse { start.is_none_or(|s| key >= s) } else { end.is_none_or(|e| key < e) };
        if !inside {
            break;
        }
        if !f(key, value)? {
            return Ok(false);
        }
        n += 1;
        if n % 256 == 0 {
            ctx.check_deadline()?;
        }
        if reverse {
            cursor.prev()?;
        } else {
            cursor.next()?;
        }
    }
    Ok(true)
}

/// Where the primary key starts in an index entry's key.
pub fn entry_pk<'k>(key: &'k [u8], value: &[u8]) -> Result<&'k [u8]> {
    let mut pos = 0;
    let offset = crate::page::get_varint(value, &mut pos).ok_or_else(|| Error::corrupt("an index entry is malformed"))? as usize;
    key.get(offset..).ok_or_else(|| Error::corrupt("an index entry is malformed"))
}

pub fn fetch(ctx: &Ctx, table: &Table, pk: &[u8]) -> Result<Option<Row>> {
    btree::get(ctx.pages, table.root, pk)?.map(Row::parse).transpose()
}

fn fetch_indexed(ctx: &Ctx, table: &Table, pk: &[u8]) -> Result<Row> {
    fetch(ctx, table, pk)?.ok_or_else(|| Error::corrupt(format!("an index of table {:?} points at a row that is not there", table.name)))
}

/// Reads the rows an access path names, in its order.
fn scan(ctx: &Ctx, table: &Table, alias: &str, access: &Access, outer: &Scope, on_hit: &mut dyn FnMut(&Hit) -> Result<bool>) -> Result<bool> {
    match access {
        Access::Primary { ranges, reverse } => {
            let parts = pk_parts(table);
            for range in ranges {
                let Some((start, end)) = range_bytes(ctx, outer, range, &parts)? else { continue };
                let done = walk_range(ctx, table.root, start.as_deref(), end.as_deref(), *reverse, &mut |key, value| {
                    on_hit(&Hit { pk: key.to_vec(), row: Row::parse(value.to_vec())?, depth: None, rank: None })
                })?;
                if !done {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Access::Index { index, ranges, reverse } => {
            let i = table.index(index)?;
            if !matches!(table.indexes[i].def.kind, IndexKind::Ordered) {
                return Err(invalid(format!("index {index:?} is a full-text index: use search")));
            }
            let parts = index_parts(table, i);
            // An index over the items of a list holds a row once per
            // item: give each row once.
            let mut seen = table.indexes[i].def.keys.iter().any(|k| k.each).then(HashSet::new);
            for range in ranges {
                let Some((start, end)) = range_bytes(ctx, outer, range, &parts)? else { continue };
                let done = walk_range(ctx, table.index_roots[i], start.as_deref(), end.as_deref(), *reverse, &mut |key, value| {
                    let pk = entry_pk(key, value)?;
                    if let Some(seen) = &mut seen {
                        if !seen.insert(pk.to_vec()) {
                            return Ok(true);
                        }
                    }
                    on_hit(&Hit { pk: pk.to_vec(), row: fetch_indexed(ctx, table, pk)?, depth: None, rank: None })
                })?;
                if !done {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Access::Walk { start, parent, direction, index, prune, stop, max_depth } => {
            walk_tree(ctx, table, alias, outer, start, parent, *direction, index.as_deref(), prune.as_ref(), stop.as_ref(), *max_depth, on_hit)
        }
        Access::Search { index, prefix, query, match_prefix } => search(ctx, table, outer, index, prefix, query, *match_prefix, on_hit),
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_tree(
    ctx: &Ctx,
    table: &Table,
    alias: &str,
    outer: &Scope,
    start: &[Vec<Expr>],
    parent: &str,
    direction: Direction,
    index: Option<&str>,
    prune: Option<&Expr>,
    stop: Option<&Expr>,
    max_depth: u32,
    on_hit: &mut dyn FnMut(&Hit) -> Result<bool>,
) -> Result<bool> {
    let parent_col = table.col(parent)?;
    let [pk_col] = table.pk[..] else { return Err(invalid("a tree walk needs a table with a one-column primary key")) };
    let parts = pk_parts(table);
    let child_index = match (direction, index) {
        (Direction::Down, Some(name)) => {
            let i = table.index(name)?;
            let first = table.indexes[i].def.keys.first();
            if !matches!(first, Some(key) if key.expr == Expr::Column(None, parent.to_string()) && !key.each && !key.desc) {
                return Err(invalid(format!("the first key of index {name:?} must be the column {parent:?}")));
            }
            Some(i)
        }
        (Direction::Down, None) => return Err(invalid("walking down a tree needs an index on the parent column")),
        (Direction::Up, _) => None,
    };
    // Whether a row passes `expr`, read in its own frame.
    let test = |row: &Row, expr: Option<&Expr>, default: bool| -> Result<bool> {
        let Some(expr) = expr else { return Ok(default) };
        let frame = [Frame { alias, table, row: Some(row) }];
        holds(ctx, &Scope { frames: &frame, outer: Some(outer), ..Scope::default() }, expr)
    };
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    // The rows of the depth being followed: each one's key, and the value
    // that leads on (its own key going down, its parent's going up).
    let mut level: Vec<Value> = Vec::new();
    let link = |row: &Row| -> Result<Value> {
        let col = if direction == Direction::Down { pk_col } else { parent_col };
        eval::frame_value(ctx, &Frame { alias, table, row: Some(row) }, col)
    };
    for key in start {
        let pk = bound_prefix(ctx, outer, key, &parts)?;
        if !seen.insert(pk.clone()) {
            continue;
        }
        let Some(row) = fetch(ctx, table, &pk)? else { continue };
        let follow = !test(&row, stop, false)?;
        let next = link(&row)?;
        if !on_hit(&Hit { pk, row, depth: Some(0), rank: None })? {
            return Ok(false);
        }
        if follow {
            level.push(next);
        }
    }
    let mut depth = 0i64;
    while !level.is_empty() && depth < i64::from(max_depth) {
        depth += 1;
        ctx.check_deadline()?;
        let mut next_level = Vec::new();
        let mut reach = |pk: Vec<u8>, row: Row, next_level: &mut Vec<Value>| -> Result<bool> {
            if seen.contains(&pk) || !test(&row, prune, true)? {
                return Ok(true);
            }
            seen.insert(pk.clone());
            let follow = !test(&row, stop, false)?;
            let next = link(&row)?;
            if !on_hit(&Hit { pk, row, depth: Some(depth), rank: None })? {
                return Ok(false);
            }
            if follow {
                next_level.push(next);
            }
            Ok(true)
        };
        for value in level {
            if value.is_null() {
                continue;
            }
            match child_index {
                Some(i) => {
                    let prefix = encode_prefix(vec![value], &index_parts(table, i))?;
                    let end = keys::successor(&prefix);
                    let done = walk_range(ctx, table.index_roots[i], Some(&prefix), end.as_deref(), false, &mut |key, entry| {
                        let pk = entry_pk(key, entry)?;
                        reach(pk.to_vec(), fetch_indexed(ctx, table, pk)?, &mut next_level)
                    })?;
                    if !done {
                        return Ok(false);
                    }
                }
                None => {
                    let pk = encode_prefix(vec![value], &parts)?;
                    if let Some(row) = fetch(ctx, table, &pk)? {
                        if !reach(pk, row, &mut next_level)? {
                            return Ok(false);
                        }
                    }
                }
            }
        }
        level = next_level;
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn search(ctx: &Ctx, table: &Table, outer: &Scope, index: &str, prefix: &[Expr], query: &Expr, match_prefix: bool, on_hit: &mut dyn FnMut(&Hit) -> Result<bool>) -> Result<bool> {
    let i = table.index(index)?;
    let def = &table.indexes[i].def;
    if !matches!(def.kind, IndexKind::FullText { .. }) {
        return Err(invalid(format!("index {index:?} is not a full-text index")));
    }
    if prefix.len() != def.keys.len() {
        return Err(invalid(format!("a search of {index:?} must give its {} leading keys", def.keys.len())));
    }
    let query = match eval(ctx, outer, query)? {
        Value::Null => return Ok(true),
        Value::Text(s) => s,
        other => return Err(Error::Db(DbError::type_mismatch(format!("a search query is text, not {}", other.type_name())))),
    };
    // A leading key that is NULL equals no row's.
    let lead = prefix.iter().map(|e| eval(ctx, outer, e)).collect::<Result<Vec<_>>>()?;
    if lead.iter().any(Value::is_null) {
        return Ok(true);
    }
    let lead = encode_prefix(lead, &index_parts(table, i))?;
    for (n, (pk, rank)) in crate::fts::search(ctx, table, i, &lead, &query, match_prefix)?.into_iter().enumerate() {
        if n % 256 == 255 {
            ctx.check_deadline()?;
        }
        let row = fetch_indexed(ctx, table, &pk)?;
        if !on_hit(&Hit { pk, row, depth: None, rank: Some(rank) })? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The table a query reads. A query of no table reads one that has no
/// columns and a single row, so that what it selects is computed once.
fn table_of(ctx: &Ctx, name: &str) -> Result<Arc<Table>> {
    static NONE: OnceLock<Arc<Table>> = OnceLock::new();
    if !name.is_empty() {
        return Ok(ctx.catalog.table(name)?.clone());
    }
    Ok(NONE
        .get_or_init(|| {
            let meta = TableMeta { id: 0, name: String::new(), cols: Vec::new(), by_name: HashMap::new(), pk: Vec::new(), indexes: Vec::new(), foreign_keys: Vec::new(), checks: Vec::new(), volatile: true };
            Arc::new(Table::empty(meta))
        })
        .clone())
}

/// Calls `f` for each row of `q` that survives its joins and its filter,
/// with the scope the row is read in and the hit on the query's own
/// table. `f` returns false to stop; `stopped` is then set, to tell that
/// apart from a join that merely reached its limit.
pub fn for_each_match(ctx: &Ctx, q: &Query, outer: &Scope, stopped: &Cell<bool>, f: &mut dyn FnMut(&Scope, &Hit) -> Result<bool>) -> Result<()> {
    let table = table_of(ctx, &q.table)?;
    let alias = q.alias.as_deref().unwrap_or(&table.name);
    if q.table.is_empty() {
        let hit = Hit { pk: Vec::new(), row: Row::from_values(&[]), depth: None, rank: None };
        let frame = [Frame { alias, table: &table, row: Some(&hit.row) }];
        let scope = Scope { frames: &frame, outer: Some(outer), ..Scope::default() };
        return join_level(ctx, q, 0, &scope, &hit, stopped, f);
    }
    scan(ctx, &table, alias, &q.access, outer, &mut |hit| {
        let frame = [Frame { alias, table: &table, row: Some(&hit.row) }];
        let scope = Scope { frames: &frame, depth: hit.depth, rank: hit.rank, outer: Some(outer), ..Scope::default() };
        join_level(ctx, q, 0, &scope, hit, stopped, f)?;
        Ok(!stopped.get())
    })?;
    Ok(())
}

fn join_level(ctx: &Ctx, q: &Query, level: usize, scope: &Scope, base: &Hit, stopped: &Cell<bool>, f: &mut dyn FnMut(&Scope, &Hit) -> Result<bool>) -> Result<()> {
    let Some(join) = q.joins.get(level) else { return finish_row(ctx, q, scope, base, stopped, f) };
    let inner = &join.query;
    if inner.is_aggregate() || !inner.order_by.is_empty() || inner.offset != 0 || inner.unnest.is_some() {
        return Err(invalid("a join reads rows in key order: it cannot group, sort, skip or repeat them"));
    }
    let mut matches = 0u64;
    let full = Cell::new(false);
    // A join limited to no rows matches none: nothing is read for it.
    if inner.limit != Some(0) {
        for_each_match(ctx, inner, scope, &full, &mut |joined, _| {
            matches += 1;
            join_level(ctx, q, level + 1, joined, base, stopped, f)?;
            // Stop this join's read when the whole query is done, or when
            // the join has given as many rows as it was asked for.
            Ok(!stopped.get() && inner.limit.is_none_or(|limit| matches < limit))
        })?;
    }
    if matches == 0 && join.kind == JoinKind::Left && !stopped.get() {
        let table = ctx.catalog.table(&inner.table)?.clone();
        let frame = [Frame { alias: inner.alias.as_deref().unwrap_or(&table.name), table: &table, row: None }];
        let padded = Scope { frames: &frame, outer: Some(scope), ..Scope::default() };
        join_level(ctx, q, level + 1, &padded, base, stopped, f)?;
    }
    Ok(())
}

fn finish_row(ctx: &Ctx, q: &Query, scope: &Scope, base: &Hit, stopped: &Cell<bool>, f: &mut dyn FnMut(&Scope, &Hit) -> Result<bool>) -> Result<()> {
    let mut pass = |scope: &Scope| -> Result<()> {
        if let Some(filter) = &q.filter {
            if !holds(ctx, scope, filter)? {
                return Ok(());
            }
        }
        if !f(scope, base)? {
            stopped.set(true);
        }
        Ok(())
    };
    match &q.unnest {
        None => pass(scope),
        Some(each) => {
            let items = match eval(ctx, scope, &each.expr)? {
                Value::Null => return Ok(()),
                Value::List(items) => items,
                other => return Err(Error::Db(DbError::type_mismatch(format!("each expects a list, got {}", other.type_name())))),
            };
            let mut seen: Vec<&Value> = Vec::new();
            for item in &items {
                if seen.iter().any(|s| s.same(item)) {
                    continue;
                }
                seen.push(item);
                pass(&Scope { item: Some((&each.name, item)), outer: Some(scope), ..Scope::default() })?;
                if stopped.get() {
                    break;
                }
            }
            Ok(())
        }
    }
}

/// A row's values as bytes that are equal exactly when the rows are the
/// same, for grouping and for DISTINCT.
fn row_key(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        // Numbers of different kinds that are equal must agree.
        match value {
            Value::Int(n) => keys::encode(&mut out, &Value::Decimal((*n).into()), false),
            Value::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => keys::encode(&mut out, &Value::Decimal((*f as i64).into()), false),
            other => keys::encode(&mut out, other, false),
        }
    }
    out
}

fn order_cmp(a: &[Value], b: &[Value], order: &[Order]) -> Ordering {
    for ((x, y), o) in a.iter().zip(b).zip(order) {
        let nulls_last = o.nulls_last.unwrap_or(!o.desc);
        let c = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if nulls_last {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, true) => {
                if nulls_last {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, false) => {
                let c = x.sort_cmp(y);
                if o.desc {
                    c.reverse()
                } else {
                    c
                }
            }
        };
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

/// What a query returns for each row: the names, and how each is found.
fn outputs(q: &Query, table: &Table) -> (Vec<String>, Option<Vec<usize>>) {
    if q.select.is_empty() {
        let live: Vec<usize> = (0..table.cols.len()).filter(|i| !table.cols[*i].dropped).collect();
        (live.iter().map(|i| table.cols[*i].def.name.clone()).collect(), Some(live))
    } else {
        (q.select.iter().map(|s| s.name.clone()).collect(), None)
    }
}

fn project(ctx: &Ctx, q: &Query, scope: &Scope, table: &Table, all: &Option<Vec<usize>>, hit: &Hit) -> Result<Vec<Value>> {
    match all {
        Some(cols) => {
            let frame = Frame { alias: "", table, row: Some(&hit.row) };
            let mut row = Vec::with_capacity(cols.len());
            for i in cols {
                row.push(eval::frame_value(ctx, &frame, *i)?);
            }
            Ok(row)
        }
        None => {
            // Collected by hand: through `collect` a fallible iterator
            // gives no size, and the row is grown a step at a time.
            let mut row = Vec::with_capacity(q.select.len());
            for item in &q.select {
                row.push(eval(ctx, scope, &item.expr)?);
            }
            Ok(row)
        }
    }
}

/// Runs a query to its rows.
pub fn query(ctx: &Ctx, q: &Query, outer: &Scope) -> Result<RowSet> {
    let table = table_of(ctx, &q.table)?;
    if q.is_aggregate() {
        return aggregate(ctx, q, outer);
    }
    let (columns, all) = outputs(q, &table);
    let stopped = Cell::new(false);
    let wanted = q.limit.map(|limit| q.offset.saturating_add(limit));
    let mut total = 0u64;
    let mut distinct: Option<HashSet<Vec<u8>>> = q.distinct.then(HashSet::new);

    if q.order_by.is_empty() {
        let mut rows = Vec::new();
        for_each_match(ctx, q, outer, &stopped, &mut |scope, hit| {
            let row = project(ctx, q, scope, &table, &all, hit)?;
            if let Some(seen) = &mut distinct {
                if !seen.insert(row_key(&row)) {
                    return Ok(true);
                }
            }
            total += 1;
            if total > q.offset && wanted.is_none_or(|w| total <= w) {
                rows.push(row);
            }
            // With nothing to sort and no total asked for, stop reading
            // as soon as the page is full.
            Ok(q.with_total || wanted.is_none_or(|w| total < w))
        })?;
        let affected = rows.len() as u64;
        return Ok(RowSet { columns, rows, affected, total: q.with_total.then_some(total) });
    }

    // Sorted: gather each row with its sort keys. With a limit, only the
    // best rows so far are kept.
    let mut rows: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    let sort = |rows: &mut Vec<(Vec<Value>, Vec<Value>)>| rows.sort_by(|a, b| order_cmp(&a.0, &b.0, &q.order_by));
    for_each_match(ctx, q, outer, &stopped, &mut |scope, hit| {
        let row = project(ctx, q, scope, &table, &all, hit)?;
        if let Some(seen) = &mut distinct {
            if !seen.insert(row_key(&row)) {
                return Ok(true);
            }
        }
        total += 1;
        let named = Scope { named: Some((&columns, &row)), outer: Some(scope), ..Scope::default() };
        let keys = q.order_by.iter().map(|o| eval(ctx, &named, &o.expr)).collect::<Result<Vec<_>>>()?;
        rows.push((keys, row));
        if let Some(w) = wanted {
            if rows.len() as u64 > w.saturating_mul(2).saturating_add(64) {
                sort(&mut rows);
                rows.truncate(w as usize);
            }
        }
        Ok(true)
    })?;
    sort(&mut rows);
    let rows: Vec<Vec<Value>> = rows.into_iter().skip(q.offset as usize).take(q.limit.unwrap_or(u64::MAX) as usize).map(|(_, row)| row).collect();
    let affected = rows.len() as u64;
    Ok(RowSet { columns, rows, affected, total: q.with_total.then_some(total) })
}

// ------------------------------------------------------------ aggregates

fn children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Not(a) | Expr::IsNull(a, _) | Expr::Neg(a) | Expr::Cast(a, _, _) => vec![a],
        Expr::And(parts) | Expr::Or(parts) | Expr::List(parts) | Expr::Call(_, parts) => parts.iter().collect(),
        Expr::Cmp(_, a, b) | Expr::Arith(_, a, b) | Expr::In(a, b, _) => vec![a, b],
        Expr::Like { expr, pattern, .. } => vec![expr, pattern],
        Expr::Case(branches, otherwise) => branches.iter().flat_map(|(c, v)| [c, v]).chain(otherwise.as_deref()).collect(),
        _ => Vec::new(),
    }
}

/// Whether an expression names one of the query's selected items.
fn names_output(e: &Expr, outputs: &[&str]) -> bool {
    let mut found = false;
    e.visit(&mut |part| found |= matches!(part, Expr::Column(None, name) if outputs.contains(&name.as_str())));
    found
}

/// Splits an expression into its aggregates and the largest parts that
/// hold none: the latter are the same for every row of a group, and are
/// computed from its first. A part that names a selected item (`outputs`,
/// for sort keys and `having`) is left to be read from the group's output.
fn split<'q>(e: &'q Expr, outputs: &[&str], plain: &mut Vec<&'q Expr>, aggs: &mut Vec<&'q Expr>) {
    if matches!(e, Expr::Agg { .. }) {
        aggs.push(e);
    } else if !e.has_aggregate() && !names_output(e, outputs) {
        plain.push(e);
    } else if !matches!(e, Expr::Column(..)) {
        for child in children(e) {
            split(child, outputs, plain, aggs);
        }
    }
}

enum Acc {
    Count(u64),
    Sum(Value),
    Best(Value),
    Avg(Value, u64),
    Collect(Vec<Value>),
    Bool(Option<bool>),
}

struct Group {
    known: AggValues,
    accs: Vec<Acc>,
    seen: Vec<HashSet<Vec<u8>>>,
}

fn add(a: &Value, b: &Value) -> Result<Value> {
    if a.is_null() {
        return Ok(b.clone());
    }
    eval::arith(ArithOp::Add, a, b)
}

fn aggregate(ctx: &Ctx, q: &Query, outer: &Scope) -> Result<RowSet> {
    if q.select.is_empty() {
        return Err(invalid("a query that groups must say what to select"));
    }
    if q.distinct {
        return Err(invalid("distinct applies to rows, not to groups: group by the columns instead"));
    }
    let (mut plain, mut agg_nodes) = (Vec::new(), Vec::new());
    for item in &q.select {
        split(&item.expr, &[], &mut plain, &mut agg_nodes);
    }
    // A sort key or a `having` may name a selected item instead of
    // repeating its expression.
    let outputs: Vec<&str> = q.select.iter().map(|s| s.name.as_str()).collect();
    for e in q.having.iter().chain(q.order_by.iter().map(|o| &o.expr)) {
        split(e, &outputs, &mut plain, &mut agg_nodes);
    }
    for e in &q.group_by {
        if e.has_aggregate() {
            return Err(invalid("a group key cannot hold an aggregate"));
        }
    }
    let new_group = |known: AggValues| Group {
        known,
        accs: agg_nodes
            .iter()
            .map(|node| match node {
                Expr::Agg { func: AggFunc::Count, .. } => Acc::Count(0),
                Expr::Agg { func: AggFunc::Sum, .. } => Acc::Sum(Value::Null),
                Expr::Agg { func: AggFunc::Min | AggFunc::Max, .. } => Acc::Best(Value::Null),
                Expr::Agg { func: AggFunc::Avg, .. } => Acc::Avg(Value::Null, 0),
                Expr::Agg { func: AggFunc::Collect, .. } => Acc::Collect(Vec::new()),
                _ => Acc::Bool(None),
            })
            .collect(),
        seen: agg_nodes.iter().map(|_| HashSet::new()).collect(),
    };

    let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    let stopped = Cell::new(false);
    for_each_match(ctx, q, outer, &stopped, &mut |scope, _| {
        let key = row_key(&q.group_by.iter().map(|e| eval(ctx, scope, e)).collect::<Result<Vec<_>>>()?);
        let g = match index.get(&key) {
            Some(g) => *g,
            None => {
                let mut known = AggValues::new();
                for e in &plain {
                    known.insert(*e as *const Expr, eval(ctx, scope, e)?);
                }
                groups.push(new_group(known));
                index.insert(key, groups.len() - 1);
                groups.len() - 1
            }
        };
        let group = &mut groups[g];
        for (n, node) in agg_nodes.iter().enumerate() {
            let Expr::Agg { func, arg, filter, distinct } = node else { unreachable!() };
            if let Some(filter) = filter {
                if !holds(ctx, scope, filter)? {
                    continue;
                }
            }
            let value = match arg {
                Some(arg) => eval(ctx, scope, arg)?,
                None => Value::Bool(true),
            };
            if value.is_null() && *func != AggFunc::Collect {
                continue;
            }
            if *distinct && !group.seen[n].insert(row_key(std::slice::from_ref(&value))) {
                continue;
            }
            match &mut group.accs[n] {
                Acc::Count(count) => *count += 1,
                Acc::Sum(sum) => *sum = add(sum, &value)?,
                Acc::Best(best) => {
                    let better = best.is_null() || {
                        let o = eval::compare(&value, best)?.unwrap_or(Ordering::Equal);
                        if *func == AggFunc::Min {
                            o == Ordering::Less
                        } else {
                            o == Ordering::Greater
                        }
                    };
                    if better {
                        *best = value;
                    }
                }
                Acc::Avg(sum, count) => {
                    *sum = add(sum, &value)?;
                    *count += 1;
                }
                Acc::Collect(items) => items.push(value),
                Acc::Bool(state) => {
                    let b = eval::truth(&value)?.unwrap_or(false);
                    *state = Some(match (*state, func) {
                        (None, _) => b,
                        (Some(s), AggFunc::BoolAnd) => s && b,
                        (Some(s), _) => s || b,
                    });
                }
            }
        }
        Ok(true)
    })?;

    // With no group keys there is always one group, even of no rows.
    if groups.is_empty() && q.group_by.is_empty() {
        let mut known = AggValues::new();
        for e in &plain {
            known.insert(*e as *const Expr, eval(ctx, outer, e).unwrap_or(Value::Null));
        }
        groups.push(new_group(known));
    }

    let columns: Vec<String> = q.select.iter().map(|s| s.name.clone()).collect();
    let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    for mut group in groups {
        for (node, acc) in agg_nodes.iter().zip(group.accs.drain(..)) {
            let value = match acc {
                Acc::Count(n) => Value::Int(n as i64),
                Acc::Sum(v) | Acc::Best(v) => v,
                Acc::Avg(_, 0) => Value::Null,
                Acc::Avg(sum, n) => match sum {
                    Value::Decimal(d) => d.checked_div(&(n as i64).into()).map_or(Value::Null, Value::Decimal),
                    other => Value::Float(other.as_f64().unwrap_or(f64::NAN) / n as f64),
                },
                Acc::Collect(items) => Value::List(items),
                Acc::Bool(state) => state.map_or(Value::Null, Value::Bool),
            };
            group.known.insert(*node as *const Expr, value);
        }
        let scope = Scope { aggs: Some(&group.known), outer: Some(outer), ..Scope::default() };
        let row = q.select.iter().map(|s| eval(ctx, &scope, &s.expr)).collect::<Result<Vec<_>>>()?;
        let named = Scope { aggs: Some(&group.known), named: Some((&columns, &row)), outer: Some(outer), ..Scope::default() };
        if let Some(having) = &q.having {
            if !holds(ctx, &named, having)? {
                continue;
            }
        }
        let keys = q.order_by.iter().map(|o| eval(ctx, &named, &o.expr)).collect::<Result<Vec<_>>>()?;
        out.push((keys, row));
    }
    if !q.order_by.is_empty() {
        out.sort_by(|a, b| order_cmp(&a.0, &b.0, &q.order_by));
    }
    let total = out.len() as u64;
    let rows: Vec<Vec<Value>> = out.into_iter().skip(q.offset as usize).take(q.limit.unwrap_or(u64::MAX) as usize).map(|(_, row)| row).collect();
    let affected = rows.len() as u64;
    Ok(RowSet { columns, rows, affected, total: q.with_total.then_some(total) })
}

/// The first column of each row of a query inside an expression.
pub fn subquery(ctx: &Ctx, scope: &Scope, q: &Query, kind: SubqueryKind) -> Result<Vec<Value>> {
    if kind == SubqueryKind::Exists && !q.is_aggregate() {
        // One row answers the question.
        let mut found = Vec::new();
        let stopped = Cell::new(false);
        for_each_match(ctx, q, scope, &stopped, &mut |_, _| {
            found.push(Value::Bool(true));
            Ok(false)
        })?;
        return Ok(found);
    }
    let set = query(ctx, q, scope)?;
    if set.columns.is_empty() {
        return Err(Error::db(ErrorCode::InvalidRequest, "a subquery must select a column"));
    }
    Ok(set.rows.into_iter().map(|mut row| row.swap_remove(0)).collect())
}
