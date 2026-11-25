//! Dump and restore: a database as a stream of bytes, and back.
//!
//! A dump is logical: table definitions and rows, not pages. It can be
//! loaded by a later version of the engine with another page layout, and
//! loading one rebuilds every index and leaves no free space behind.
//!
//! ```text
//! dump    "DBDUMP01", then records, the last one End
//! record  kind (u8), length (u32), payload, CRC-32 of kind and payload
//!
//! Database  name; a note the server keeps with it (its password's hash)
//! Table     definition; for each numbering column, its name and next number
//! Rows      table name, row count, each row's values in the definition's order
//! Roles     the database's roles, each with its rights; absent if it has none
//! Close     ends a database: its count of rows
//! End       ends the dump: its count of databases
//! ```
//!
//! Each database is dumped as of one moment, from a reading transaction
//! that holds nobody up. A dump that stops early has no End, and one that
//! was damaged fails a CRC: `Restore` refuses both and keeps nothing.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use database_protocol::codec::{self, Reader, Writer as Encoder};
use database_protocol::error::{DbError, ErrorCode};
use database_protocol::schema::TableDef;
use database_protocol::value::Value;

use crate::btree::Cursor;
use crate::catalog::Table;
use crate::db::Database;
use crate::engine::Engine;
use crate::error::{Error, Result};
use crate::eval::{frame_value, Frame};
use crate::row::Row;

const MAGIC: &[u8; 8] = b"DBDUMP01";
const DATABASE: u8 = 1;
const TABLE: u8 = 2;
const ROWS: u8 = 3;
const CLOSE: u8 = 4;
const END: u8 = 5;
const ROLES: u8 = 6;
/// Rows are written in records of about this many bytes.
const BATCH_BYTES: usize = 1 << 20;
/// No record is larger than this: the largest row, with room to spare.
const MAX_RECORD: usize = 1 << 30;

fn record(out: &mut dyn Write, kind: u8, payload: &[u8]) -> Result<()> {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&[kind]);
    hasher.update(payload);
    out.write_all(&[kind])?;
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(payload)?;
    out.write_all(&hasher.finalize().to_le_bytes())?;
    Ok(())
}

/// Writes the named databases to `out`, or all of them when none is
/// named, and returns how many rows went. `note` gives the bytes to keep
/// with each database's name.
pub fn dump(engine: &Engine, names: &[String], note: &dyn Fn(&str) -> Vec<u8>, out: &mut dyn Write) -> Result<u64> {
    let names = if names.is_empty() { engine.names() } else { names.to_vec() };
    out.write_all(MAGIC)?;
    let mut total = 0;
    for name in &names {
        total += dump_database(&engine.database(name)?, &note(name), out)?;
    }
    let mut end = Encoder::new();
    end.varint(names.len() as u64);
    record(out, END, &end.buf)?;
    out.flush()?;
    Ok(total)
}

fn dump_database(db: &Arc<Database>, note: &[u8], out: &mut dyn Write) -> Result<u64> {
    let txn = db.begin_read();
    let ctx = txn.ctx(None);
    let mut head = Encoder::new();
    head.str(db.name());
    head.bytes(note);
    record(out, DATABASE, &head.buf)?;
    let defs = txn.catalog().defs_in_order();
    for def in &defs {
        let table = txn.catalog().table(&def.name)?;
        let mut w = Encoder::new();
        codec::put_table(&mut w, def);
        let numbered: Vec<(&str, i64)> = table.cols.iter().zip(&table.serials).filter(|(c, _)| !c.dropped && c.def.auto_increment).map(|(c, n)| (c.def.name.as_str(), *n)).collect();
        w.varint(numbered.len() as u64);
        for (column, next) in numbered {
            w.str(column);
            w.zigzag(next);
        }
        record(out, TABLE, &w.buf)?;
    }
    let mut rows_total = 0u64;
    for def in &defs {
        let table: &Table = txn.catalog().table(&def.name)?;
        // A table that need not survive a restart is not worth a backup.
        if table.volatile {
            continue;
        }
        let live: Vec<usize> = (0..table.cols.len()).filter(|i| !table.cols[*i].dropped).collect();
        let mut batch = Encoder::new();
        let mut count = 0u64;
        let flush = |batch: &mut Encoder, count: &mut u64, out: &mut dyn Write| -> Result<()> {
            if *count > 0 {
                let mut w = Encoder::new();
                w.str(&table.name);
                w.varint(*count);
                w.buf.append(&mut batch.buf);
                record(out, ROWS, &w.buf)?;
                *count = 0;
            }
            Ok(())
        };
        let mut cursor = Cursor::new(ctx.pages, table.root);
        cursor.first()?;
        while let Some(value) = cursor.value() {
            let row = Row::parse(value.to_vec())?;
            let frame = Frame { alias: "", table, row: Some(&row) };
            for i in &live {
                codec::put_value(&mut batch, &frame_value(&ctx, &frame, *i)?);
            }
            count += 1;
            rows_total += 1;
            if batch.buf.len() >= BATCH_BYTES {
                flush(&mut batch, &mut count, out)?;
            }
            cursor.next()?;
        }
        flush(&mut batch, &mut count, out)?;
    }
    // The roles come last, when every table they name is there. A
    // database without any is dumped as it always was.
    let roles = &txn.catalog().roles;
    if !roles.is_empty() {
        let mut w = Encoder::new();
        w.varint(roles.len() as u64);
        for role in roles.values() {
            crate::catalog::put_role(&mut w, role);
        }
        record(out, ROLES, &w.buf)?;
    }
    let mut close = Encoder::new();
    close.varint(rows_total);
    record(out, CLOSE, &close.buf)?;
    Ok(rows_total)
}

fn bad(what: impl Into<String>) -> Error {
    Error::db(ErrorCode::InvalidRequest, format!("the dump cannot be loaded: {}", what.into()))
}

fn damaged(e: DbError) -> Error {
    bad(format!("a record is malformed ({})", e.message))
}

/// Loads a dump fed to it in pieces. Databases are built under temporary
/// names and only take their places, all together, when the dump's end
/// has been read and every count agrees.
pub struct Restore<'e> {
    engine: &'e Engine,
    replace: bool,
    buffer: Vec<u8>,
    started: bool,
    ended: bool,
    current: Option<Loading>,
    /// Finished databases: the name each is for, the name it was built
    /// under, and the note that came with it.
    done: Vec<(String, String, Vec<u8>)>,
}

struct Loading {
    name: String,
    note: Vec<u8>,
    temp: String,
    txn: crate::db::WriteTxn,
    defs: Vec<(TableDef, Vec<(String, i64)>)>,
    created: bool,
    /// Per table: its live columns' positions in a stored row.
    rows: u64,
    columns: HashMap<String, usize>,
}

impl<'e> Restore<'e> {
    pub fn new(engine: &'e Engine, replace: bool) -> Restore<'e> {
        Restore { engine, replace, buffer: Vec::new(), started: false, ended: false, current: None, done: Vec::new() }
    }

    /// Takes the next bytes of the dump.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(bytes);
        let mut at = 0;
        if !self.started {
            if self.buffer.len() < MAGIC.len() {
                return Ok(());
            }
            if &self.buffer[..MAGIC.len()] != MAGIC {
                return Err(bad("it does not start as a dump does"));
            }
            self.started = true;
            at = MAGIC.len();
        }
        loop {
            let rest = &self.buffer[at..];
            if rest.len() < 5 {
                break;
            }
            let len = u32::from_le_bytes(rest[1..5].try_into().unwrap()) as usize;
            if len > MAX_RECORD {
                return Err(bad("a record's length is impossible"));
            }
            if rest.len() < 5 + len + 4 {
                break;
            }
            if self.ended {
                return Err(bad("there is data after its end"));
            }
            let (kind, payload) = (rest[0], &rest[5..5 + len]);
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&[kind]);
            hasher.update(payload);
            if hasher.finalize().to_le_bytes() != rest[5 + len..5 + len + 4] {
                return Err(bad("a record fails its checksum"));
            }
            let payload = payload.to_vec();
            at += 5 + len + 4;
            self.record(kind, &payload)?;
        }
        self.buffer.drain(..at);
        Ok(())
    }

    fn record(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        let mut r = Reader::new(payload);
        match kind {
            DATABASE => {
                if self.current.is_some() {
                    return Err(bad("a database starts inside another"));
                }
                let name = r.str().map_err(damaged)?;
                let note = r.bytes().map_err(damaged)?.to_vec();
                if !self.replace && self.engine.database(&name).is_ok() {
                    return Err(Error::db(ErrorCode::AlreadyExists, format!("database {name:?} already exists; restore with replace to overwrite it")));
                }
                if self.done.iter().any(|(n, _, _)| *n == name) {
                    return Err(bad(format!("database {name:?} is in it twice")));
                }
                // Built beside the live one, under a name no client can use.
                crate::engine::check_database_name(&name)?;
                let temp = format!("{name}{}", crate::engine::RESTORING);
                if self.engine.database(&temp).is_ok() {
                    self.engine.drop_database(&temp)?;
                }
                let db = self.engine.create_database_unchecked(&temp)?;
                let mut txn = db.begin_write(Duration::from_secs(60), false)?;
                txn.raw()?.logging = false;
                self.current = Some(Loading { name, note, temp, txn, defs: Vec::new(), created: false, rows: 0, columns: HashMap::new() });
            }
            TABLE => {
                let loading = self.current.as_mut().ok_or_else(|| bad("a table outside any database"))?;
                if loading.created {
                    return Err(bad("a table definition after rows"));
                }
                let def = codec::get_table(&mut r).map_err(damaged)?;
                let n = r.count().map_err(damaged)?;
                let numbered = (0..n).map(|_| Ok((r.str()?, r.zigzag()?))).collect::<std::result::Result<Vec<_>, DbError>>().map_err(damaged)?;
                loading.defs.push((def, numbered));
            }
            ROWS => {
                let loading = self.current.as_mut().ok_or_else(|| bad("rows outside any database"))?;
                loading.create_tables()?;
                let table = r.str().map_err(damaged)?;
                let width = *loading.columns.get(&table).ok_or_else(|| bad(format!("rows for a table {table:?} it never defined")))?;
                let count = r.varint().map_err(damaged)?;
                for _ in 0..count {
                    let values = (0..width).map(|_| codec::get_value(&mut r)).collect::<std::result::Result<Vec<Value>, DbError>>().map_err(damaged)?;
                    loading.txn.raw()?.apply_insert(&table, &values)?;
                    loading.rows += 1;
                }
            }
            ROLES => {
                let loading = self.current.as_mut().ok_or_else(|| bad("roles outside any database"))?;
                loading.create_tables()?;
                for _ in 0..r.count().map_err(damaged)? {
                    let role = crate::catalog::get_role(&mut r).map_err(damaged)?;
                    crate::catalog::check_role_name(&role.name).map_err(|_| bad(format!("{:?} is not a role's name", role.name)))?;
                    // A right on a table the dump does not hold would be
                    // a right on whatever is given that name later.
                    if let Some(table) = role.tables.keys().find(|table| !loading.columns.contains_key(*table)) {
                        return Err(bad(format!("role {:?} has rights on a table {table:?} it never defined", role.name)));
                    }
                    loading.txn.raw()?.restore_role(role);
                }
            }
            CLOSE => {
                let mut loading = self.current.take().ok_or_else(|| bad("a database ends that never began"))?;
                loading.create_tables()?;
                if r.varint().map_err(damaged)? != loading.rows {
                    return Err(bad(format!("database {:?} does not hold the rows it counts", loading.name)));
                }
                // The numbering columns go on from where they were.
                for (def, numbered) in &loading.defs {
                    for (column, next) in numbered {
                        loading.txn.raw()?.set_serial(&def.name, column, *next)?;
                    }
                }
                loading.txn.commit()?;
                self.done.push((loading.name, loading.temp, loading.note));
            }
            END => {
                if self.current.is_some() {
                    return Err(bad("it ends inside a database"));
                }
                if r.varint().map_err(damaged)? != self.done.len() as u64 {
                    return Err(bad("it does not hold the databases it counts"));
                }
                self.ended = true;
            }
            _ => return Err(bad("a record of an unknown kind")),
        }
        r.finish().map_err(damaged)
    }

    /// Ends the load. If the dump was whole, its databases take their
    /// places; their names and notes are returned. Otherwise nothing is
    /// kept.
    pub fn finish(mut self) -> Result<Vec<(String, Vec<u8>)>> {
        if !self.ended || !self.buffer.is_empty() {
            return Err(bad("it is cut short"));
        }
        let done = std::mem::take(&mut self.done);
        let mut restored = Vec::new();
        for (name, temp, note) in done {
            self.engine.replace_database(&temp, &name)?;
            restored.push((name, note));
        }
        Ok(restored)
    }
}

impl Drop for Restore<'_> {
    /// Whatever was built and not put in place is removed.
    fn drop(&mut self) {
        if let Some(loading) = self.current.take() {
            let temp = loading.temp.clone();
            drop(loading);
            let _ = self.engine.drop_database(&temp);
        }
        for (_, temp, _) in self.done.drain(..) {
            let _ = self.engine.drop_database(&temp);
        }
    }
}

impl Loading {
    /// Creates the tables once all are known. Foreign keys are attached
    /// after every table exists, since they may point at each other.
    fn create_tables(&mut self) -> Result<()> {
        if self.created {
            return Ok(());
        }
        self.created = true;
        let tx = self.txn.raw()?;
        for (def, _) in &self.defs {
            let mut bare = def.clone();
            bare.foreign_keys.clear();
            tx.create_table(&bare, false)?;
            self.columns.insert(def.name.clone(), def.columns.len());
        }
        for (def, _) in &self.defs {
            tx.set_foreign_keys(&def.name, &def.foreign_keys)?;
        }
        Ok(())
    }
}
