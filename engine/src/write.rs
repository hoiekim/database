//! Writing: inserts, updates, deletes and schema changes inside one
//! transaction.
//!
//! Every change goes through three small functions that touch the trees
//! (`apply_insert`, `apply_update`, `apply_delete`) and a handful for the
//! schema. Each also appends a record of what it did to the transaction's
//! log, in terms of rows and columns, not pages. After a crash those
//! records are fed to the same functions again: the result is the same
//! rows and the same indexes, wherever the pages land.

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use database_protocol::codec::{self, Reader, Writer as Encoder};
use database_protocol::error::{DbError, ErrorCode};
use database_protocol::expr::{Expr, Func};
use database_protocol::message::RowSet;
use database_protocol::query::*;
use database_protocol::schema::*;
use database_protocol::value::Value;

use crate::blob;
use crate::btree::{self, Cursor};
use crate::catalog::{plain_columns, Catalog, Table, TableMeta};
use crate::error::{Error, Result};
use crate::eval::{eval, frame_value, holds, Ctx, Frame, Scope, Tape};
use crate::exec::{self, encode_prefix, entry_pk, fetch, for_each_match, index_parts, pk_parts};
use crate::fts;
use crate::keys;
use crate::page::{put_varint, PageId, NO_PAGE};
use crate::row::{self, Row};
use crate::store::Writer;

const OP_INSERT: u8 = 1;
const OP_UPDATE: u8 = 2;
const OP_DELETE: u8 = 3;
const OP_CREATE_TABLE: u8 = 4;
const OP_DROP_TABLE: u8 = 5;
const OP_ADD_COLUMN: u8 = 6;
const OP_DROP_COLUMN: u8 = 7;
const OP_CREATE_INDEX: u8 = 8;
const OP_DROP_INDEX: u8 = 9;

/// A writing transaction's working state.
pub struct Tx {
    pub writer: Writer,
    /// The catalog as this transaction has changed it.
    pub catalog: Catalog,
    /// What the transaction did, for the log. Empty if it changed only
    /// tables that need not survive a crash.
    pub log: Encoder,
    pub now: i64,
    pub deadline: Option<Instant>,
    /// False while the log is being replayed: nothing is recorded twice.
    pub logging: bool,
    /// What its statements draw: random ids and counters' numbers.
    pub tape: Tape,
    /// The tables it has made, which nobody else can see until it commits.
    created: Vec<u32>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Db(DbError::invalid(message))
}

fn violation(code: ErrorCode, table: &str, constraint: &str, message: String) -> Error {
    Error::Db(DbError::new(code, message).with_constraint(table, constraint))
}

/// One entry a row has in an index.
struct Entry {
    key: Vec<u8>,
    value: Vec<u8>,
    /// For a unique index: the part of the key that must not repeat.
    /// `None` when a key value is NULL, which never conflicts.
    unique: Option<usize>,
}

fn row_scope<'a>(frame: &'a [Frame<'a>]) -> Scope<'a> {
    Scope { frames: frame, ..Scope::default() }
}

/// What a statement returns, with `ALL` spelled out as the table's
/// columns.
fn returned(table: &Table, items: &[Select]) -> Vec<Select> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if item.name == ALL && item.expr == Expr::Column(None, ALL.to_string()) {
            out.extend(table.cols.iter().filter(|c| !c.dropped).map(|c| Select { name: c.def.name.clone(), expr: Expr::Column(None, c.def.name.clone()) }));
        } else {
            out.push(item.clone());
        }
    }
    out
}

fn is_full_text(table: &Table, i: usize) -> bool {
    matches!(table.indexes[i].def.kind, IndexKind::FullText { .. })
}

/// The entries `row` has in index `i` of `table`.
fn index_entries(ctx: &Ctx, table: &Table, i: usize, row: &Row, pk: &[u8]) -> Result<Vec<Entry>> {
    let def = &table.indexes[i].def;
    let frame = [Frame { alias: &table.name, table, row: Some(row) }];
    let scope = row_scope(&frame);
    if let Some(filter) = &def.filter {
        if !holds(ctx, &scope, filter)? {
            return Ok(Vec::new());
        }
    }
    let parts = index_parts(table, i);
    // Each key's values: one, or for `each` one per distinct item.
    let mut prefixes: Vec<(Vec<u8>, bool)> = vec![(Vec::new(), false)];
    for (n, key) in def.keys.iter().enumerate() {
        let value = eval(ctx, &scope, &key.expr)?;
        let items = if key.each {
            match value {
                Value::Null => return Ok(Vec::new()),
                Value::List(items) => {
                    let mut distinct: Vec<Value> = Vec::new();
                    for item in items {
                        if !distinct.iter().any(|d| d.same(&item)) {
                            distinct.push(item);
                        }
                    }
                    distinct
                }
                other => return Err(Error::Db(DbError::type_mismatch(format!("index {:?} expects a list, got {}", def.name, other.type_name())))),
            }
        } else {
            vec![value]
        };
        let mut next = Vec::with_capacity(prefixes.len() * items.len());
        for (prefix, has_null) in &prefixes {
            for item in &items {
                let item = match &parts[n].0 {
                    Some(ty) => item.clone().coerce(ty)?,
                    None => item.clone(),
                };
                let mut key_bytes = prefix.clone();
                keys::encode(&mut key_bytes, &item, parts[n].1);
                next.push((key_bytes, *has_null || item.is_null()));
            }
        }
        prefixes = next;
    }
    let mut out = Vec::new();
    match &def.kind {
        IndexKind::Ordered => {
            for (mut key, has_null) in prefixes {
                let offset = key.len();
                key.extend_from_slice(pk);
                let mut value = Vec::new();
                put_varint(&mut value, offset as u64);
                out.push(Entry { key, value, unique: (def.unique && !has_null).then_some(offset) });
            }
        }
        // A full-text index is not a tree of entries: see `fts`.
        IndexKind::FullText { .. } => {}
    }
    Ok(out)
}

/// The keys of the entries `row` has in index `i`, for the consistency
/// check.
pub fn entry_keys(ctx: &Ctx, table: &Table, i: usize, row: &Row, pk: &[u8]) -> Result<Vec<Vec<u8>>> {
    Ok(index_entries(ctx, table, i, row, pk)?.into_iter().map(|e| e.key).collect())
}

fn pk_of(table: &Table, values: &[Value]) -> Result<Vec<u8>> {
    encode_prefix(table.pk.iter().map(|i| values[*i].clone()).collect(), pk_parts(table))
}

/// The primary key of another row that already holds this unique entry.
fn unique_holder(ctx: &Ctx, table: &Table, i: usize, entry: &Entry, own_pk: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(offset) = entry.unique else { return Ok(None) };
    let prefix = &entry.key[..offset];
    let mut cursor = Cursor::new(ctx.pages, table.index_roots[i]);
    cursor.seek(prefix)?;
    while let (Some(key), Some(value)) = (cursor.key(), cursor.value()) {
        if !key.starts_with(prefix) {
            break;
        }
        // A longer key may merely start with these bytes: only an entry
        // whose key part ends where this one's does is the same key.
        let pk = entry_pk(key, value)?;
        if key.len() - pk.len() == offset && pk != own_pk {
            return Ok(Some(pk.to_vec()));
        }
        cursor.next()?;
    }
    Ok(None)
}

impl Tx {
    pub fn new(writer: Writer, catalog: Catalog, now: i64) -> Tx {
        Tx { writer, catalog, log: Encoder::new(), now, deadline: None, logging: true, tape: Tape::default(), created: Vec::new() }
    }

    pub fn ctx(&self) -> Ctx<'_> {
        Ctx { pages: &self.writer, catalog: &self.catalog, now: self.now, deadline: self.deadline, tape: Some(&self.tape) }
    }

    fn table(&self, name: &str) -> Result<Arc<Table>> {
        Ok(self.catalog.table(name)?.clone())
    }

    /// The table's entry in this transaction's catalog, to change.
    fn table_mut(&mut self, name: &str) -> &mut Table {
        Arc::make_mut(self.catalog.tables.get_mut(name).expect("the table was just read"))
    }

    /// Gives an index of a table another root: for whoever rebuilt the
    /// index's tree beside the writers and now takes its place.
    pub(crate) fn set_index_root(&mut self, name: &str, index: usize, root: PageId) -> Result<()> {
        self.table(name)?;
        self.table_mut(name).index_roots[index] = root;
        Ok(())
    }

    // ------------------------------------------------------ row changes

    /// Stores a new row. `values` holds one value per stored column, of
    /// the column's type, and breaks no constraint.
    pub fn apply_insert(&mut self, name: &str, values: &[Value]) -> Result<Vec<u8>> {
        let stored = Row::from_values(values);
        self.apply_insert_row(name, values, &stored, None)
    }

    /// Who merges the full-text segments this transaction leaves due.
    fn merging(&self) -> fts::Merging {
        if self.writer.leaves_beside() {
            fts::Merging::Beside
        } else {
            fts::Merging::AtOnce
        }
    }

    /// `apply_insert` for a row already made of its values. `ahead` holds
    /// what was cut of the row's texts before the transaction began, if
    /// anything was.
    fn apply_insert_row(&mut self, name: &str, values: &[Value], stored: &Row, ahead: Option<fts::RowAhead>) -> Result<Vec<u8>> {
        let table = self.table(name)?;
        let pk = pk_of(&table, values)?;
        let (entries, words) = {
            let ctx = self.ctx();
            let entries: Vec<Vec<Entry>> = (0..table.indexes.len()).map(|i| index_entries(&ctx, &table, i, stored, &pk)).collect::<Result<_>>()?;
            let mut words = Vec::new();
            for i in (0..table.indexes.len()).filter(|i| is_full_text(&table, *i)) {
                if let Some(w) = fts::words_of_cut(&ctx, &table, i, stored, ahead)? {
                    words.push((i, w));
                }
            }
            (entries, words)
        };
        let (mut blobs, mut next_blob) = (table.blobs, table.next_blob);
        let moved = row::fitted(&mut self.writer, stored, btree::max_value(pk.len()), &mut blobs, &mut next_blob)?;
        let (root, existed) = btree::put(&mut self.writer, table.root, &pk, moved.as_deref().unwrap_or(stored.raw()))?;
        if existed {
            return Err(Error::corrupt(format!("a row of table {name:?} was inserted over another")));
        }
        let mut index_roots = table.index_roots.clone();
        for (i, entries) in entries.iter().enumerate() {
            for entry in entries {
                index_roots[i] = btree::put(&mut self.writer, index_roots[i], &entry.key, &entry.value)?.0;
            }
        }
        let merging = self.merging();
        for (i, words) in &words {
            let (root, left) = fts::add_merging(&mut self.writer, index_roots[*i], &pk, words, merging)?;
            index_roots[*i] = root;
            if left {
                self.writer.note_due();
            }
        }
        if self.logging && !table.volatile {
            self.log.u8(OP_INSERT);
            self.log.u32(table.id);
            self.log.varint(values.len() as u64);
            for value in values {
                codec::put_value(&mut self.log, value);
            }
        }
        let serial_cols: Vec<usize> = (0..table.cols.len()).filter(|i| table.cols[*i].def.auto_increment).collect();
        drop(table);
        let t = self.table_mut(name);
        t.root = root;
        t.blobs = blobs;
        t.next_blob = next_blob;
        t.rows += 1;
        t.index_roots = index_roots;
        for i in serial_cols {
            if let Value::Int(n) = values[i] {
                t.serials[i] = t.serials[i].max(n.saturating_add(1));
            }
        }
        Ok(pk)
    }

    /// The row `old` with `changes` made: the changed values, and the
    /// stored bytes of the rest, large values still where they were.
    fn changed_row(table: &Table, old: &Row, changes: &[(usize, Value)]) -> Row {
        Row::build(table.cols.len(), |i, out| {
            match changes.iter().rev().find(|(c, _)| *c == i) {
                Some((_, value)) => codec::put_value(out, value),
                None => match old.raw_col(i) {
                    Some(raw) => out.buf.extend_from_slice(raw),
                    None => codec::put_value(out, &table.cols[i].fill),
                },
            }
        })
    }

    /// Changes columns of the row with primary key `pk`.
    pub fn apply_update(&mut self, name: &str, pk: &[u8], changes: &[(usize, Value)]) -> Result<()> {
        let table = self.table(name)?;
        let old = fetch(&self.ctx(), &table, pk)?.ok_or_else(|| {
            Error::corrupt(format!(
                "a row of table {name:?} vanished before its update"
            ))
        })?;
        let new = Self::changed_row(&table, &old, changes);
        self.apply_update_rows(name, pk, changes, &old, &new)
    }

    /// `apply_update` for a row already read (`old`) and already made of
    /// its changes (`new`).
    fn apply_update_rows(
        &mut self,
        name: &str,
        pk: &[u8],
        changes: &[(usize, Value)],
        old: &Row,
        new: &Row,
    ) -> Result<()> {
        let table = self.table(name)?;
        // Only the indexes that read a changed column can change.
        let touched: Vec<usize> = (0..table.indexes.len()).filter(|i| table.indexes[*i].reads.iter().any(|c| changes.iter().any(|(ch, _)| ch == c))).collect();
        let (before, after, reworded) = {
            let ctx = self.ctx();
            let mut before = Vec::new();
            let mut after = Vec::new();
            let mut reworded = Vec::new();
            for i in &touched {
                before.push(index_entries(&ctx, &table, *i, old, pk)?);
                after.push(index_entries(&ctx, &table, *i, new, pk)?);
                if is_full_text(&table, *i) {
                    // Written again with the words it had, a row stays
                    // as it is in the index.
                    let was = fts::words_of(&ctx, &table, *i, old)?;
                    let now = fts::words_of(&ctx, &table, *i, new)?;
                    if was != now {
                        reworded.push((*i, now));
                    }
                }
            }
            (before, after, reworded)
        };
        let (mut blobs, mut next_blob) = (table.blobs, table.next_blob);
        for (i, _) in changes {
            if let Some(b) = old.blob(*i) {
                blobs = blob::free(&mut self.writer, blobs, b.id, b.len)?;
            }
        }
        let moved = row::fitted(&mut self.writer, new, btree::max_value(pk.len()), &mut blobs, &mut next_blob)?;
        let root = btree::put(&mut self.writer, table.root, pk, moved.as_deref().unwrap_or(new.raw()))?.0;
        let mut index_roots = table.index_roots.clone();
        for ((i, before), after) in touched.iter().zip(&before).zip(&after) {
            for entry in before {
                if !after.iter().any(|a| a.key == entry.key && a.value == entry.value) {
                    index_roots[*i] = btree::delete(&mut self.writer, index_roots[*i], &entry.key)?.0;
                }
            }
            for entry in after {
                if !before.iter().any(|b| b.key == entry.key && b.value == entry.value) {
                    index_roots[*i] = btree::put(&mut self.writer, index_roots[*i], &entry.key, &entry.value)?.0;
                }
            }
        }
        let merging = self.merging();
        for (i, words) in &reworded {
            let (mut root, mut left) = fts::remove_merging(&mut self.writer, index_roots[*i], pk, merging)?;
            if let Some(words) = words {
                let added = fts::add_merging(&mut self.writer, root, pk, words, merging)?;
                root = added.0;
                left |= added.1;
            }
            index_roots[*i] = root;
            if left {
                self.writer.note_due();
            }
        }
        if self.logging && !table.volatile {
            self.log.u8(OP_UPDATE);
            self.log.u32(table.id);
            self.log.bytes(pk);
            self.log.varint(changes.len() as u64);
            for (i, value) in changes {
                self.log.varint(*i as u64);
                codec::put_value(&mut self.log, value);
            }
        }
        let serials: Vec<(usize, i64)> = changes.iter().filter_map(|(i, v)| match v {
            Value::Int(n) if table.cols[*i].def.auto_increment => Some((*i, *n)),
            _ => None,
        }).collect();
        drop(table);
        let t = self.table_mut(name);
        t.root = root;
        t.blobs = blobs;
        t.next_blob = next_blob;
        t.index_roots = index_roots;
        for (i, n) in serials {
            t.serials[i] = t.serials[i].max(n.saturating_add(1));
        }
        Ok(())
    }

    /// Removes the row with primary key `pk`, its index entries and its
    /// large values.
    pub fn apply_delete(&mut self, name: &str, pk: &[u8]) -> Result<()> {
        let table = self.table(name)?;
        let old = fetch(&self.ctx(), &table, pk)?.ok_or_else(|| Error::corrupt(format!("a row of table {name:?} vanished before its delete")))?;
        let entries: Vec<Vec<Entry>> = {
            let ctx = self.ctx();
            (0..table.indexes.len()).map(|i| index_entries(&ctx, &table, i, &old, pk)).collect::<Result<_>>()?
        };
        let mut index_roots = table.index_roots.clone();
        for (i, entries) in entries.iter().enumerate() {
            for entry in entries {
                index_roots[i] = btree::delete(&mut self.writer, index_roots[i], &entry.key)?.0;
            }
            if is_full_text(&table, i) {
                let merging = self.merging();
                let (root, left) = fts::remove_merging(&mut self.writer, index_roots[i], pk, merging)?;
                index_roots[i] = root;
                if left {
                    self.writer.note_due();
                }
            }
        }
        let mut blobs = table.blobs;
        for b in old.blobs() {
            blobs = blob::free(&mut self.writer, blobs, b.id, b.len)?;
        }
        let root = btree::delete(&mut self.writer, table.root, pk)?.0;
        if self.logging && !table.volatile {
            self.log.u8(OP_DELETE);
            self.log.u32(table.id);
            self.log.bytes(pk);
        }
        drop(table);
        let t = self.table_mut(name);
        t.root = root;
        t.blobs = blobs;
        t.rows = t.rows.saturating_sub(1);
        t.index_roots = index_roots;
        Ok(())
    }

    // ------------------------------------------------------ constraints

    /// Coerces a value to its column and names the column if it does not
    /// fit.
    fn fit_column(table: &Table, i: usize, value: Value) -> Result<Value> {
        let col = &table.cols[i].def;
        value.coerce(&col.ty).map_err(|e| Error::Db(DbError::new(e.code, format!("column {:?} of {:?}: {}", col.name, table.name, e.message))))
    }

    /// Checks NOT NULL and the table's checks against a row about to be
    /// stored. `changed` limits the checks to those reading these columns.
    fn check_row(&self, table: &Table, values: &Row, changed: Option<&[usize]>) -> Result<()> {
        let ctx = self.ctx();
        let frame = [Frame { alias: &table.name, table, row: Some(values) }];
        for (i, col) in table.cols.iter().enumerate() {
            if col.dropped || col.def.nullable || changed.is_some_and(|c| !c.contains(&i)) {
                continue;
            }
            if frame_value(&ctx, &frame[0], i)?.is_null() {
                return Err(violation(ErrorCode::NotNullViolation, &table.name, &col.def.name, format!("column {:?} of {:?} cannot be null", col.def.name, table.name)));
            }
        }
        let scope = row_scope(&frame);
        for check in &table.checks {
            if let Some(changed) = changed {
                let reads = crate::catalog::columns_read(&check.expr);
                if !reads.iter().any(|name| table.by_name.get(name).is_some_and(|i| changed.contains(i))) {
                    continue;
                }
            }
            // A check fails only when it is false: NULL passes, as in SQL.
            if crate::eval::truth(&eval(&ctx, &scope, &check.expr)?)? == Some(false) {
                return Err(violation(ErrorCode::CheckViolation, &table.name, &check.name, format!("a row of {:?} fails the check {:?}", table.name, check.name)));
            }
        }
        Ok(())
    }

    /// Checks that the parents a row's foreign keys name exist.
    fn check_parents(&self, table: &Table, values: &Row, changed: Option<&[usize]>) -> Result<()> {
        let ctx = self.ctx();
        let frame = Frame { alias: &table.name, table, row: Some(values) };
        for fk in &table.foreign_keys {
            let cols: Vec<usize> = fk.columns.iter().map(|c| table.col(c)).collect::<Result<_>>()?;
            if changed.is_some_and(|c| !cols.iter().any(|i| c.contains(i))) {
                continue;
            }
            let key: Vec<Value> = cols.iter().map(|i| frame_value(&ctx, &frame, *i)).collect::<Result<_>>()?;
            if key.iter().any(Value::is_null) {
                continue;
            }
            let parent = self.table(&fk.ref_table)?;
            let found = match parent.unique_key(&fk.ref_columns) {
                Some(None) => fetch(&ctx, &parent, &encode_prefix(key.clone(), pk_parts(&parent))?)?.is_some(),
                Some(Some(i)) => {
                    let prefix = encode_prefix(key.clone(), &index_parts(&parent, i))?;
                    let mut cursor = Cursor::new(ctx.pages, parent.index_roots[i]);
                    cursor.seek(&prefix)?;
                    cursor.key().is_some_and(|k| k.starts_with(&prefix))
                }
                None => return Err(Error::corrupt(format!("foreign key {:?} points at no unique key", fk.name))),
            };
            // A row may point at itself.
            let own = fk.ref_table == table.name && {
                let refs: Vec<Value> = fk.ref_columns.iter().map(|c| table.col(c).and_then(|i| frame_value(&ctx, &frame, i))).collect::<Result<_>>()?;
                refs.iter().zip(&key).all(|(a, b)| a.same(b))
            };
            if !found && !own {
                return Err(violation(ErrorCode::ForeignKeyViolation, &table.name, &fk.name, format!("a row of {:?} points at a row of {:?} that does not exist", table.name, fk.ref_table)));
            }
        }
        Ok(())
    }

    /// Refuses a change to a key that rows of another table point at. A
    /// foreign key may name a unique index instead of the primary key, and
    /// those columns, unlike a primary key's, can be updated: not while
    /// something depends on the value they had.
    fn check_children_keep_their_parent(&self, table: &Table, old: &Row, new: &Row, changed: &[usize]) -> Result<()> {
        if changed.is_empty() {
            return Ok(());
        }
        for (child, fks) in self.catalog.children_of(&table.name) {
            for fk in &fks {
                let refs: Vec<usize> = fk.ref_columns.iter().map(|c| table.col(c)).collect::<Result<_>>()?;
                if !refs.iter().any(|i| changed.contains(i)) {
                    continue;
                }
                let ctx = self.ctx();
                let values = |row: &Row| -> Result<Vec<Value>> {
                    let frame = Frame { alias: &table.name, table, row: Some(row) };
                    refs.iter().map(|i| frame_value(&ctx, &frame, *i)).collect()
                };
                let (was, now) = (values(old)?, values(new)?);
                if was.iter().any(Value::is_null) || was.iter().zip(&now).all(|(a, b)| a.same(b)) {
                    continue;
                }
                if !self.children_pointing_at(&child, fk, &[was])?.is_empty() {
                    return Err(violation(
                        ErrorCode::ForeignKeyViolation,
                        &table.name,
                        &fk.name,
                        format!("a row of {:?} is still pointed at by rows of {:?}, so its key cannot change", table.name, child.name),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The unique index a row would break, and the row that holds the
    /// key. `only` limits the search to some indexes.
    fn find_conflict(&self, table: &Table, values: &Row, pk: &[u8], only: Option<&[usize]>) -> Result<Option<(String, Vec<u8>)>> {
        let ctx = self.ctx();
        for i in 0..table.indexes.len() {
            if !table.indexes[i].def.unique || only.is_some_and(|o| !o.contains(&i)) {
                continue;
            }
            for entry in index_entries(&ctx, table, i, values, pk)? {
                if let Some(holder) = unique_holder(&ctx, table, i, &entry, pk)? {
                    return Ok(Some((table.indexes[i].def.name.clone(), holder)));
                }
            }
        }
        Ok(None)
    }

    fn unique_error(table: &Table, index: &str) -> Error {
        violation(ErrorCode::UniqueViolation, &table.name, index, format!("a row of {:?} with this key already exists", table.name))
    }

    /// Validates and applies a change to one row, and returns the row as
    /// it now is. `old` is the row as it was, if the caller has it at
    /// hand: it is read otherwise.
    fn update_row(&mut self, name: &str, pk: &[u8], changes: Vec<(usize, Value)>, old: Option<&Row>) -> Result<Row> {
        let table = self.table(name)?;
        let read;
        let old = match old {
            Some(old) => old,
            None => {
                read = fetch(&self.ctx(), &table, pk)?.ok_or_else(|| {
                    Error::corrupt(format!(
                        "a row of table {name:?} vanished before its update"
                    ))
                })?;
                &read
            }
        };
        let mut fitted = Vec::with_capacity(changes.len());
        for (i, value) in changes {
            let value = Self::fit_column(&table, i, value)?;
            if table.pk.contains(&i) {
                let current = frame_value(&self.ctx(), &Frame { alias: name, table: &table, row: Some(old) }, i)?;
                if !current.same(&value) {
                    return Err(invalid(format!("column {:?} is part of the primary key of {name:?} and cannot be changed: delete the row and insert it again", table.cols[i].def.name)));
                }
                continue;
            }
            fitted.push((i, value));
        }
        let changed: Vec<usize> = fitted.iter().map(|(i, _)| *i).collect();
        let new = Self::changed_row(&table, old, &fitted);
        self.check_row(&table, &new, Some(&changed))?;
        let touched: Vec<usize> = (0..table.indexes.len()).filter(|i| table.indexes[*i].reads.iter().any(|c| changed.contains(c))).collect();
        if let Some((index, _)) = self.find_conflict(&table, &new, pk, Some(&touched))? {
            return Err(Self::unique_error(&table, &index));
        }
        self.check_parents(&table, &new, Some(&changed))?;
        self.check_children_keep_their_parent(&table, old, &new, &changed)?;
        self.apply_update_rows(name, pk, &fitted, old, &new)?;
        Ok(new)
    }

    /// Deletes rows and then deals with the rows that pointed at them:
    /// deleting them too, clearing their pointers, or refusing.
    fn delete_rows(&mut self, name: &str, pks: &[Vec<u8>]) -> Result<u64> {
        let table = self.table(name)?;
        let children = self.catalog.children_of(name);
        // Per foreign key pointing here: the key values of each deleted row.
        let mut orphaned: Vec<Vec<Vec<Value>>> = children.iter().flat_map(|(_, fks)| fks.iter().map(|_| Vec::new())).collect();
        let mut deleted = 0;
        for pk in pks {
            let Some(old) = fetch(&self.ctx(), &table, pk)? else { continue };
            {
                let ctx = self.ctx();
                let frame = Frame { alias: name, table: &table, row: Some(&old) };
                let mut n = 0;
                for (_, fks) in &children {
                    for fk in fks {
                        let key: Vec<Value> = fk.ref_columns.iter().map(|c| table.col(c).and_then(|i| frame_value(&ctx, &frame, i))).collect::<Result<_>>()?;
                        if !key.iter().any(Value::is_null) {
                            orphaned[n].push(key);
                        }
                        n += 1;
                    }
                }
            }
            self.apply_delete(name, pk)?;
            deleted += 1;
        }
        let mut n = 0;
        for (child, fks) in &children {
            for fk in fks {
                let keys = std::mem::take(&mut orphaned[n]);
                n += 1;
                if keys.is_empty() {
                    continue;
                }
                // The child's table may have changed above: read it anew.
                let child = self.table(&child.name)?;
                let found = self.children_pointing_at(&child, fk, &keys)?;
                if found.is_empty() {
                    continue;
                }
                match fk.on_delete {
                    OnDelete::Restrict => {
                        return Err(violation(ErrorCode::ForeignKeyViolation, name, &fk.name, format!("a row of {name:?} is still pointed at by rows of {:?}", child.name)));
                    }
                    OnDelete::Cascade => {
                        self.delete_rows(&child.name, &found)?;
                    }
                    OnDelete::SetNull => {
                        let cols: Vec<usize> = fk.columns.iter().map(|c| child.col(c)).collect::<Result<_>>()?;
                        for pk in found {
                            let current = self.table(&child.name)?;
                            if fetch(&self.ctx(), &current, &pk)?.is_some() {
                                self.apply_update(&child.name, &pk, &cols.iter().map(|i| (*i, Value::Null)).collect::<Vec<_>>())?;
                            }
                        }
                    }
                }
            }
        }
        Ok(deleted)
    }

    /// The primary keys of `child`'s rows whose foreign key holds one of
    /// `keys`: through an index that starts with the key's columns if
    /// there is one, by reading the whole table otherwise.
    fn children_pointing_at(&self, child: &Table, fk: &ForeignKey, keys: &[Vec<Value>]) -> Result<Vec<Vec<u8>>> {
        let ctx = self.ctx();
        let mut found = Vec::new();
        let index = (0..child.indexes.len()).find(|i| {
            let def = &child.indexes[*i].def;
            matches!(def.kind, IndexKind::Ordered) && def.filter.is_none() && def.keys.len() >= fk.columns.len() && plain_columns(&IndexDef { keys: def.keys[..fk.columns.len()].to_vec(), ..def.clone() }).is_some_and(|c| c == fk.columns) && def.keys[..fk.columns.len()].iter().all(|k| !k.desc)
        });
        let pk_prefix = fk.columns.len() <= child.pk.len() && fk.columns.iter().zip(&child.pk).all(|(c, i)| child.cols[*i].def.name == *c);
        if pk_prefix {
            for key in keys {
                let prefix = encode_prefix(key.clone(), pk_parts(child))?;
                let mut cursor = Cursor::new(ctx.pages, child.root);
                cursor.seek(&prefix)?;
                while let Some(k) = cursor.key() {
                    if !k.starts_with(&prefix) {
                        break;
                    }
                    found.push(k.to_vec());
                    cursor.next()?;
                }
            }
        } else if let Some(i) = index {
            for key in keys {
                let prefix = encode_prefix(key.clone(), &index_parts(child, i))?;
                let mut cursor = Cursor::new(ctx.pages, child.index_roots[i]);
                cursor.seek(&prefix)?;
                while let (Some(k), Some(v)) = (cursor.key(), cursor.value()) {
                    if !k.starts_with(&prefix) {
                        break;
                    }
                    found.push(entry_pk(k, v)?.to_vec());
                    cursor.next()?;
                }
            }
        } else {
            let cols: Vec<usize> = fk.columns.iter().map(|c| child.col(c)).collect::<Result<_>>()?;
            let mut cursor = Cursor::new(ctx.pages, child.root);
            cursor.first()?;
            let mut n = 0u32;
            while let (Some(k), Some(v)) = (cursor.key(), cursor.value()) {
                let row = Row::parse(v.to_vec())?;
                let frame = Frame { alias: &child.name, table: child, row: Some(&row) };
                let values: Vec<Value> = cols.iter().map(|i| frame_value(&ctx, &frame, *i)).collect::<Result<_>>()?;
                if keys.iter().any(|key| key.iter().zip(&values).all(|(a, b)| a.same(b))) {
                    found.push(k.to_vec());
                }
                n += 1;
                if n % 256 == 0 {
                    ctx.check_deadline()?;
                }
                cursor.next()?;
            }
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    // ------------------------------------------------------- statements

    pub fn insert(&mut self, ins: &Insert) -> Result<RowSet> {
        self.insert_ahead(ins, None)
    }

    /// `insert`, for an insert whose texts were cut into words before
    /// the transaction began: see `fts::cut_ahead`. `ahead` must have
    /// been made from this very insert.
    pub fn insert_ahead(&mut self, ins: &Insert, ahead: Option<&fts::Ahead>) -> Result<RowSet> {
        let table = self.table(&ins.table)?;
        let mut targets = Vec::with_capacity(ins.columns.len());
        for name in &ins.columns {
            let i = table.col(name)?;
            if targets.contains(&i) {
                return Err(invalid(format!("column {name:?} is given twice")));
            }
            targets.push(i);
        }
        // The rows to insert, as expressions or as another query's rows.
        let from_query: Option<Vec<Vec<Value>>> = match &ins.source {
            InsertSource::Values(_) => None,
            InsertSource::Query(q) => Some(exec::query(&self.ctx(), q, &Scope::default())?.rows),
        };
        let count = match (&ins.source, &from_query) {
            (InsertSource::Values(rows), _) => rows.len(),
            (_, Some(rows)) => rows.len(),
            _ => 0,
        };
        let returning = returned(&table, &ins.returning);
        let columns: Vec<String> = returning.iter().map(|s| s.name.clone()).collect();
        let mut out = Vec::new();
        let mut affected = 0;
        for n in 0..count {
            if n % 64 == 63 {
                self.ctx().check_deadline()?;
            }
            // Re-read the table: each row changes its counters and roots.
            let table = self.table(&ins.table)?;
            let given: Vec<Value> = match (&ins.source, &from_query) {
                (InsertSource::Values(rows), _) => {
                    let ctx = self.ctx();
                    rows[n].iter().map(|e| eval(&ctx, &Scope::default(), e)).collect::<Result<_>>()?
                }
                (_, Some(rows)) => rows[n].clone(),
                _ => unreachable!(),
            };
            if given.len() != targets.len() {
                return Err(invalid(format!("a row of {} values for {} columns", given.len(), targets.len())));
            }
            let mut values = vec![Value::Null; table.cols.len()];
            let mut set = vec![false; table.cols.len()];
            for (i, value) in targets.iter().zip(given) {
                values[*i] = Self::fit_column(&table, *i, value)?;
                set[*i] = true;
            }
            for (i, col) in table.cols.iter().enumerate() {
                if set[i] || col.dropped {
                    continue;
                }
                values[i] = if col.def.auto_increment {
                    let own = self.created.contains(&table.id);
                    Value::Int(self.tape.serial(table.serials[i], |floor| self.writer.next_serial(table.id, i, floor, own)))
                } else if let Some(default) = &col.def.default {
                    Self::fit_column(&table, i, eval(&self.ctx(), &Scope::default(), default)?)?
                } else {
                    Value::Null
                };
            }
            let proposed = Row::from_values(&values);
            self.check_row(&table, &proposed, None)?;
            let pk = pk_of(&table, &values)?;

            // Which unique key, if any, the row would break. The row
            // that holds its primary key is kept: an upsert reads it.
            let mut held = fetch(&self.ctx(), &table, &pk)?;
            let conflict = if held.is_some() {
                Some((PRIMARY.to_string(), pk.clone()))
            } else {
                self.find_conflict(&table, &proposed, &pk, None)?
            };
            let existing = match (&ins.on_conflict, conflict) {
                (_, None) => None,
                (OnConflict::Error, Some((index, _))) => return Err(Self::unique_error(&table, &index)),
                (OnConflict::DoNothing { target: None }, Some(_)) => continue,
                (OnConflict::DoNothing { target: Some(target) }, Some((index, _))) | (OnConflict::DoUpdate { target, .. }, Some((index, _))) if *target != index => {
                    // It breaks another key. Does it break the named one
                    // too? That one decides.
                    let named = self.conflict_on(&table, &proposed, &pk, target)?;
                    match (named, &ins.on_conflict) {
                        (None, _) => return Err(Self::unique_error(&table, &index)),
                        (Some(_), OnConflict::DoNothing { .. }) => continue,
                        (Some(holder), _) => Some(holder),
                    }
                }
                (OnConflict::DoNothing { .. }, Some(_)) => continue,
                (OnConflict::DoUpdate { .. }, Some((_, holder))) => Some(holder),
            };

            // The row as written, and as it was before if it was there.
            let (stored, old) = match existing {
                None => {
                    self.check_parents(&table, &proposed, None)?;
                    self.apply_insert_row(&ins.table, &values, &proposed, ahead.and_then(|ahead| ahead.row(ins, n)))?;
                    (proposed, None)
                }
                Some(holder) => {
                    let OnConflict::DoUpdate { set, filter, .. } = &ins.on_conflict else { unreachable!() };
                    let current = match held.take() {
                        Some(row) if holder == pk => row,
                        _ => fetch(&self.ctx(), &table, &holder)?.ok_or_else(|| {
                            Error::corrupt("a unique index points at a row that is not there")
                        })?,
                    };
                    let changes = {
                        let ctx = self.ctx();
                        let frame = [Frame { alias: &table.name, table: &table, row: Some(&current) }];
                        let scope = Scope { frames: &frame, excluded: Some(&values), ..Scope::default() };
                        if let Some(filter) = filter {
                            if !holds(&ctx, &scope, filter)? {
                                continue;
                            }
                        }
                        set.iter().map(|a| Ok((table.col(&a.column)?, eval(&ctx, &scope, &a.value)?))).collect::<Result<Vec<_>>>()?
                    };
                    let new = self.update_row(&ins.table, &holder, changes, Some(&current))?;
                    (new, Some(current))
                }
            };
            affected += 1;
            if !returning.is_empty() {
                // Read from the row as it was made, not from the tree
                // again. The table is read anew: its large values moved.
                let table = self.table(&ins.table)?;
                let ctx = self.ctx();
                let frame = [Frame { alias: &table.name, table: &table, row: Some(&stored) }];
                let scope = Scope { frames: &frame, old: old.as_ref(), excluded: Some(&values), ..Scope::default() };
                out.push(returning.iter().map(|s| eval(&ctx, &scope, &s.expr)).collect::<Result<Vec<_>>>()?);
            }
        }
        Ok(RowSet { columns, rows: out, affected, total: None })
    }

    /// The row holding `values`' key in the named unique index.
    fn conflict_on(&self, table: &Table, proposed: &Row, pk: &[u8], target: &str) -> Result<Option<Vec<u8>>> {
        if target == PRIMARY {
            return Ok(fetch(&self.ctx(), table, pk)?.map(|_| pk.to_vec()));
        }
        let i = table.index(target)?;
        if !table.indexes[i].def.unique {
            return Err(invalid(format!("index {target:?} is not unique, so nothing can conflict with it")));
        }
        // The proposed row is not stored, so whoever holds its key is in
        // its way, the row with its own primary key included.
        let _ = pk;
        Ok(self.find_conflict(table, proposed, &[], Some(&[i]))?.map(|(_, holder)| holder))
    }

    /// The rows a query names for an update or delete, each once, in the
    /// query's order and within its limit, with whatever `per_row`
    /// computes from the scope the row was found in.
    fn targets<T>(&self, q: &Query, per_row: &mut dyn FnMut(&Ctx, &Scope, &exec::Hit) -> Result<T>) -> Result<Vec<(Vec<u8>, T)>> {
        if q.is_aggregate() || q.unnest.is_some() || q.distinct {
            return Err(invalid("the rows of an update or delete cannot be grouped or repeated"));
        }
        let ctx = self.ctx();
        let wanted = q.limit.map(|l| q.offset.saturating_add(l));
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut found: Vec<(Vec<Value>, Vec<u8>, T)> = Vec::new();
        let stopped = Cell::new(false);
        for_each_match(&ctx, q, &Scope::default(), &stopped, &mut |scope, hit| {
            if !seen.insert(hit.pk.clone()) {
                return Ok(true);
            }
            let keys = q.order_by.iter().map(|o| eval(&ctx, scope, &o.expr)).collect::<Result<Vec<_>>>()?;
            found.push((keys, hit.pk.clone(), per_row(&ctx, scope, hit)?));
            Ok(!q.order_by.is_empty() || wanted.is_none_or(|w| (found.len() as u64) < w))
        })?;
        if !q.order_by.is_empty() {
            found.sort_by(|a, b| {
                for ((x, y), o) in a.0.iter().zip(&b.0).zip(&q.order_by) {
                    let c = x.sort_cmp(y);
                    let c = if o.desc { c.reverse() } else { c };
                    if c != std::cmp::Ordering::Equal {
                        return c;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        Ok(found.into_iter().skip(q.offset as usize).take(q.limit.unwrap_or(u64::MAX) as usize).map(|(_, pk, t)| (pk, t)).collect())
    }

    pub fn update(&mut self, upd: &Update) -> Result<RowSet> {
        let name = upd.rows.table.clone();
        let table = self.table(&name)?;
        let cols: Vec<usize> = upd.set.iter().map(|a| table.col(&a.column)).collect::<Result<_>>()?;
        let returning = returned(&table, &upd.returning);
        let columns: Vec<String> = returning.iter().map(|s| s.name.clone()).collect();
        // First find every row and work out its new values and what to
        // return, all read from the rows as they are now; then write.
        let targets = self.targets(&upd.rows, &mut |ctx, scope, hit| {
            let changes: Vec<(usize, Value)> =
                cols.iter().zip(&upd.set).map(|(i, a)| Ok((*i, Self::fit_column(&table, *i, eval(ctx, scope, &a.value)?)?))).collect::<Result<_>>()?;
            let returned = if returning.is_empty() {
                Vec::new()
            } else {
                // The row as it will be, in front of the scope it was
                // found in, so that joined columns can be returned too.
                let raw: Vec<Vec<u8>> = (0..table.cols.len())
                    .map(|i| match changes.iter().rev().find(|(c, _)| *c == i) {
                        Some((_, value)) => row::encode_value(value),
                        None => hit.row.raw_col(i).map_or_else(|| row::encode_value(&table.cols[i].fill), <[u8]>::to_vec),
                    })
                    .collect();
                let new = Row::parse(row::assemble(&raw))?;
                let alias = upd.rows.alias.as_deref().unwrap_or(&table.name);
                let frame = [Frame { alias, table: &table, row: Some(&new) }];
                let after = Scope { frames: &frame, old: Some(&hit.row), outer: Some(scope), ..Scope::default() };
                returning.iter().map(|s| eval(ctx, &after, &s.expr)).collect::<Result<Vec<_>>>()?
            };
            Ok((changes, returned))
        })?;
        let mut out = Vec::new();
        let mut affected = 0;
        for (n, (pk, (changes, returned))) in targets.into_iter().enumerate() {
            if n % 64 == 63 {
                self.ctx().check_deadline()?;
            }
            self.update_row(&name, &pk, changes, None)?;
            affected += 1;
            if !returning.is_empty() {
                out.push(returned);
            }
        }
        Ok(RowSet { columns, rows: out, affected, total: None })
    }

    pub fn delete(&mut self, del: &Delete) -> Result<RowSet> {
        let name = del.rows.table.clone();
        let returning = returned(&*self.table(&name)?, &del.returning);
        let columns: Vec<String> = returning.iter().map(|s| s.name.clone()).collect();
        let targets = self.targets(&del.rows, &mut |ctx, scope, _| returning.iter().map(|s| eval(ctx, scope, &s.expr)).collect::<Result<Vec<_>>>())?;
        let (pks, rows): (Vec<Vec<u8>>, Vec<Vec<Value>>) = targets.into_iter().unzip();
        let affected = self.delete_rows(&name, &pks)?;
        Ok(RowSet { columns, rows: if returning.is_empty() { Vec::new() } else { rows }, affected, total: None })
    }

    // ----------------------------------------------------------- schema

    fn log_op(&mut self, op: u8, write: impl FnOnce(&mut Encoder)) {
        if self.logging {
            self.log.u8(op);
            write(&mut self.log);
        }
    }

    pub fn create_table(&mut self, def: &TableDef, if_not_exists: bool) -> Result<()> {
        if self.catalog.tables.contains_key(def.name.as_str()) {
            return if if_not_exists { Ok(()) } else { Err(Error::db(ErrorCode::AlreadyExists, format!("table {:?} already exists", def.name))) };
        }
        let id = self.catalog.next_table_id.max(1);
        let table = Table::create(def, id, &self.catalog)?;
        self.catalog.next_table_id = id + 1;
        self.created.push(id);
        self.catalog.tables.insert(Arc::from(def.name.as_str()), Arc::new(table));
        self.log_op(OP_CREATE_TABLE, |w| codec::put_table(w, def));
        Ok(())
    }

    /// Frees every page of a tree.
    fn destroy_tree(&mut self, root: PageId) -> Result<()> {
        btree::destroy(&mut self.writer, root, &mut |_, _| Ok(()))
    }

    /// Frees every page of a table's indexes.
    fn destroy_indexes(&mut self, table: &Table) -> Result<()> {
        for (i, root) in table.index_roots.iter().enumerate() {
            if is_full_text(table, i) {
                fts::destroy(&mut self.writer, *root)?;
            } else {
                self.destroy_tree(*root)?;
            }
        }
        Ok(())
    }

    pub fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<()> {
        let Some(table) = self.catalog.tables.get(name).cloned() else {
            return if if_exists { Ok(()) } else { Err(Error::Db(DbError::not_found(format!("no table {name:?}")))) };
        };
        if let Some((child, _)) = self.catalog.children_of(name).into_iter().find(|(child, _)| child.name != name) {
            return Err(violation(ErrorCode::ForeignKeyViolation, name, &child.name, format!("table {name:?} is pointed at by table {:?}: drop that one first", child.name)));
        }
        self.destroy_tree(table.root)?;
        self.destroy_tree(table.blobs)?;
        self.destroy_indexes(&table)?;
        self.catalog.tables.remove(name);
        self.log_op(OP_DROP_TABLE, |w| w.str(name));
        Ok(())
    }

    /// Empties a table that need not survive a restart.
    pub fn truncate(&mut self, name: &str) -> Result<()> {
        let table = self.table(name)?;
        self.destroy_tree(table.root)?;
        self.destroy_tree(table.blobs)?;
        self.destroy_indexes(&table)?;
        drop(table);
        let t = self.table_mut(name);
        t.root = NO_PAGE;
        t.blobs = NO_PAGE;
        t.rows = 0;
        t.index_roots.iter_mut().for_each(|r| *r = NO_PAGE);
        Ok(())
    }

    fn change_meta(&mut self, name: &str, change: impl FnOnce(&mut TableMeta, &Catalog) -> Result<()>) -> Result<()> {
        let mut meta = (*self.table(name)?.meta).clone();
        change(&mut meta, &self.catalog)?;
        self.table_mut(name).meta = Arc::new(meta);
        Ok(())
    }

    /// Every row's primary key.
    fn all_keys(&self, table: &Table) -> Result<Vec<Vec<u8>>> {
        let mut keys = Vec::with_capacity(table.rows as usize);
        let mut cursor = Cursor::new(&self.writer, table.root);
        cursor.first()?;
        while let Some(key) = cursor.key() {
            keys.push(key.to_vec());
            cursor.next()?;
        }
        Ok(keys)
    }

    pub fn add_column(&mut self, name: &str, column: &ColumnDef, if_not_exists: bool) -> Result<()> {
        let table = self.table(name)?;
        if table.by_name.contains_key(&column.name) {
            return if if_not_exists { Ok(()) } else { Err(Error::db(ErrorCode::AlreadyExists, format!("table {name:?} already has a column {:?}", column.name))) };
        }
        // A default that differs per row is given to each row in turn;
        // any other is computed once and read by every existing row.
        let mut per_row = false;
        if let Some(default) = &column.default {
            default.visit(&mut |e| per_row |= matches!(e, Expr::Call(Func::RandomUuid, _)));
        }
        let fill = match &column.default {
            Some(default) if !per_row => eval(&self.ctx(), &Scope::default(), default)?.coerce(&column.ty)?,
            _ => Value::Null,
        };
        let needs_value = !column.nullable && fill.is_null() && !per_row;
        if table.rows > 0 && (needs_value || column.auto_increment) {
            return Err(violation(ErrorCode::NotNullViolation, name, &column.name, format!("column {:?} cannot be null and has no default for the rows already in {name:?}", column.name)));
        }
        self.add_column_filled(name, column, fill)?;
        if per_row && table.rows > 0 {
            let table = self.table(name)?;
            let i = table.col(&column.name)?;
            for pk in self.all_keys(&table)? {
                let value = eval(&self.ctx(), &Scope::default(), column.default.as_ref().unwrap())?.coerce(&column.ty)?;
                self.apply_update(name, &pk, &[(i, value)])?;
            }
        }
        Ok(())
    }

    fn add_column_filled(&mut self, name: &str, column: &ColumnDef, fill: Value) -> Result<()> {
        self.change_meta(name, |meta, _| meta.add_column(column.clone(), fill.clone()).map(|_| ()))?;
        self.table_mut(name).serials.push(1);
        self.log_op(OP_ADD_COLUMN, |w| {
            w.str(name);
            codec::put_column(w, column);
            codec::put_value(w, &fill);
        });
        Ok(())
    }

    pub fn drop_column(&mut self, name: &str, column: &str, if_exists: bool) -> Result<()> {
        let table = self.table(name)?;
        let Some(&i) = table.by_name.get(column) else {
            return if if_exists { Ok(()) } else { Err(Error::Db(DbError::not_found(format!("table {name:?} has no column {column:?}")))) };
        };
        if table.pk.contains(&i) {
            return Err(invalid(format!("column {column:?} is part of the primary key of {name:?}")));
        }
        if let Some(index) = table.indexes.iter().find(|index| index.reads.contains(&i)) {
            return Err(invalid(format!("column {column:?} is read by index {:?}: drop that first", index.def.name)));
        }
        let in_check = table.checks.iter().any(|c| crate::catalog::columns_read(&c.expr).iter().any(|n| n == column));
        let in_fk = table.foreign_keys.iter().any(|fk| fk.columns.iter().any(|c| c == column));
        if in_check || in_fk {
            return Err(invalid(format!("column {column:?} is used by a check or a foreign key of {name:?}")));
        }
        self.change_meta(name, |meta, _| {
            meta.cols[i].dropped = true;
            meta.by_name.remove(column);
            Ok(())
        })?;
        self.log_op(OP_DROP_COLUMN, |w| {
            w.str(name);
            w.str(column);
        });
        Ok(())
    }

    pub fn create_index(&mut self, name: &str, index: &IndexDef, if_not_exists: bool) -> Result<()> {
        let table = self.table(name)?;
        if table.indexes.iter().any(|i| i.def.name == index.name) {
            return if if_not_exists { Ok(()) } else { Err(Error::db(ErrorCode::AlreadyExists, format!("table {name:?} already has an index {:?}", index.name))) };
        }
        self.change_meta(name, |meta, _| meta.add_index(index.clone()).map(|_| ()))?;
        self.table_mut(name).index_roots.push(NO_PAGE);
        // Fill it from the rows already there.
        let table = self.table(name)?;
        let i = table.indexes.len() - 1;
        let mut root = NO_PAGE;
        for (n, pk) in self.all_keys(&table)?.into_iter().enumerate() {
            if n % 256 == 255 {
                self.ctx().check_deadline()?;
            }
            let (entries, words) = {
                let ctx = self.ctx();
                let stored = fetch(&ctx, &table, &pk)?.ok_or_else(|| Error::corrupt("a row vanished while its table was indexed"))?;
                let words = if is_full_text(&table, i) { fts::words_of(&ctx, &table, i, &stored)? } else { None };
                (index_entries(&ctx, &table, i, &stored, &pk)?, words)
            };
            if let Some(words) = words {
                root = fts::add(&mut self.writer, root, &pk, &words)?;
            }
            for entry in entries {
                if let Some(offset) = entry.unique {
                    // The tree under construction is the index so far.
                    let mut cursor = Cursor::new(&self.writer, root);
                    cursor.seek(&entry.key[..offset])?;
                    let taken = match (cursor.key(), cursor.value()) {
                        (Some(k), Some(v)) => k.starts_with(&entry.key[..offset]) && k.len() - entry_pk(k, v)?.len() == offset,
                        _ => false,
                    };
                    if taken {
                        return Err(violation(ErrorCode::UniqueViolation, name, &index.name, format!("rows of {name:?} already repeat the key of unique index {:?}", index.name)));
                    }
                }
                root = btree::put(&mut self.writer, root, &entry.key, &entry.value)?.0;
            }
        }
        self.table_mut(name).index_roots[i] = root;
        self.log_op(OP_CREATE_INDEX, |w| {
            w.str(name);
            codec::put_index(w, index);
        });
        Ok(())
    }

    pub fn drop_index(&mut self, name: &str, index: &str, if_exists: bool) -> Result<()> {
        let table = self.table(name)?;
        let Some(i) = table.indexes.iter().position(|x| x.def.name == index) else {
            return if if_exists { Ok(()) } else { Err(Error::Db(DbError::not_found(format!("table {name:?} has no index {index:?}")))) };
        };
        if is_full_text(&table, i) {
            fts::destroy(&mut self.writer, table.index_roots[i])?;
        } else {
            self.destroy_tree(table.index_roots[i])?;
        }
        self.change_meta(name, |meta, _| {
            meta.indexes.remove(i);
            Ok(())
        })?;
        self.table_mut(name).index_roots.remove(i);
        self.log_op(OP_DROP_INDEX, |w| {
            w.str(name);
            w.str(index);
        });
        Ok(())
    }

    // ------------------------------------------------- loading a dump

    /// Sets where a numbering column goes on from, never backwards.
    pub fn set_serial(&mut self, name: &str, column: &str, next: i64) -> Result<()> {
        let i = self.table(name)?.col(column)?;
        let t = self.table_mut(name);
        t.serials[i] = t.serials[i].max(next);
        Ok(())
    }

    /// Attaches foreign keys to a table created without them.
    pub fn set_foreign_keys(&mut self, name: &str, fks: &[ForeignKey]) -> Result<()> {
        self.change_meta(name, |meta, catalog| {
            for fk in fks {
                let fk = meta.resolve_foreign_key(fk, catalog)?;
                meta.foreign_keys.push(fk);
            }
            Ok(())
        })
    }

    // ----------------------------------------------------------- replay

    /// Applies one transaction's log record. The record was written by
    /// this code from changes that had passed every check, so nothing is
    /// checked again.
    pub fn replay(&mut self, record: &[u8]) -> Result<()> {
        let bad = |e: DbError| Error::corrupt(format!("a log record is damaged: {}", e.message));
        let mut r = Reader::new(record);
        let table_name = |tx: &Tx, id: u32| tx.catalog.table_by_id(id).map(|t| t.name.clone()).ok_or_else(|| Error::corrupt(format!("the log names table {id}, which does not exist")));
        while !r.is_empty() {
            match r.u8().map_err(bad)? {
                OP_INSERT => {
                    let name = table_name(self, r.u32().map_err(bad)?)?;
                    let n = r.count().map_err(bad)?;
                    let values = (0..n).map(|_| codec::get_value(&mut r)).collect::<std::result::Result<Vec<_>, _>>().map_err(bad)?;
                    self.apply_insert(&name, &values)?;
                }
                OP_UPDATE => {
                    let name = table_name(self, r.u32().map_err(bad)?)?;
                    let pk = r.bytes().map_err(bad)?.to_vec();
                    let n = r.count().map_err(bad)?;
                    let mut changes = Vec::with_capacity(n);
                    for _ in 0..n {
                        changes.push((r.varint().map_err(bad)? as usize, codec::get_value(&mut r).map_err(bad)?));
                    }
                    self.apply_update(&name, &pk, &changes)?;
                }
                OP_DELETE => {
                    let name = table_name(self, r.u32().map_err(bad)?)?;
                    let pk = r.bytes().map_err(bad)?.to_vec();
                    self.apply_delete(&name, &pk)?;
                }
                OP_CREATE_TABLE => self.create_table(&codec::get_table(&mut r).map_err(bad)?, false)?,
                OP_DROP_TABLE => self.drop_table(&r.str().map_err(bad)?, false)?,
                OP_ADD_COLUMN => {
                    let name = r.str().map_err(bad)?;
                    let column = codec::get_column(&mut r).map_err(bad)?;
                    let fill = codec::get_value(&mut r).map_err(bad)?;
                    self.add_column_filled(&name, &column, fill)?;
                }
                OP_DROP_COLUMN => {
                    let name = r.str().map_err(bad)?;
                    self.drop_column(&name, &r.str().map_err(bad)?, false)?;
                }
                OP_CREATE_INDEX => {
                    let name = r.str().map_err(bad)?;
                    self.create_index(&name, &codec::get_index(&mut r).map_err(bad)?, false)?;
                }
                OP_DROP_INDEX => {
                    let name = r.str().map_err(bad)?;
                    self.drop_index(&name, &r.str().map_err(bad)?, false)?;
                }
                other => return Err(Error::corrupt(format!("a log record holds an unknown operation {other}"))),
            }
        }
        Ok(())
    }
}
