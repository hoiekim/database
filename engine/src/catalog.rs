//! The catalog: every table of a database, as defined and as stored.
//!
//! A version of the database is a catalog: each table's definition, the
//! roots of its trees, its row count and its counters. A transaction works
//! on a copy and the commit swaps it in, so a schema change is as atomic
//! as any other write. A checkpoint stores the catalog whole.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use database_protocol::codec::{self, Reader, Writer};
use database_protocol::error::{DbError, ErrorCode};
use database_protocol::expr::{Expr, Func};
use database_protocol::rights::Rights;
use database_protocol::schema::*;
use database_protocol::value::{Type, Value};

use crate::error::{Error, Result};
use crate::page::{PageId, NO_PAGE};

/// Hashes a column's name, to look it up by: every column an expression
/// names is found this way for every row it is read from, so a short
/// plain hash (FNV-1a) serves better than one that withstands chosen
/// keys. The names are the schema's own.
#[derive(Clone, Copy)]
pub struct NameHasher(u64);

impl Default for NameHasher {
    fn default() -> Self {
        NameHasher(0xcbf2_9ce4_8422_2325)
    }
}

impl std::hash::Hasher for NameHasher {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 29)
    }
}

/// Columns by name.
pub type NameMap = HashMap<String, usize, std::hash::BuildHasherDefault<NameHasher>>;

#[derive(Clone, Debug)]
pub struct Column {
    pub def: ColumnDef,
    /// What rows stored before the column existed read for it.
    pub fill: Value,
    /// The column was dropped: its place in stored rows remains, its name
    /// is free again.
    pub dropped: bool,
}

#[derive(Clone, Debug)]
pub struct Index {
    pub def: IndexDef,
    /// The columns the index's keys, filter and text fields read.
    pub reads: Vec<usize>,
}

/// A table as defined: what changes only when the schema does.
#[derive(Clone, Debug)]
pub struct TableMeta {
    pub id: u32,
    pub name: String,
    /// Every column a stored row may hold, dropped ones included.
    pub cols: Vec<Column>,
    /// Live columns by name.
    pub by_name: NameMap,
    pub pk: Vec<usize>,
    pub indexes: Vec<Index>,
    pub foreign_keys: Vec<ForeignKey>,
    pub checks: Vec<Check>,
    pub volatile: bool,
    /// The type and direction of each part of the primary key, worked
    /// out when first asked for (see `exec::pk_parts`).
    pub pk_parts: std::sync::OnceLock<Vec<(Option<Type>, bool)>>,
}

/// A table as stored in one version of the database: its definition, and
/// the roots and counters that every write moves. Cheap to copy, which a
/// writing transaction does for each table it touches.
#[derive(Clone, Debug)]
pub struct Table {
    pub meta: Arc<TableMeta>,
    pub root: PageId,
    /// The tree of values too large for their rows.
    pub blobs: PageId,
    pub next_blob: u64,
    pub rows: u64,
    /// One root per index, in the definition's order.
    pub index_roots: Vec<PageId>,
    /// Per column: for an `auto_increment` one, the next number to give.
    pub serials: Vec<i64>,
}

impl std::ops::Deref for Table {
    type Target = TableMeta;
    fn deref(&self) -> &TableMeta {
        &self.meta
    }
}

/// A role of the database: who may connect under its name, and what
/// they may then do.
///
/// Roles are part of the catalog, so a version of the database is its
/// tables and, with them, who may do what to them. A change of rights is
/// then a commit like any other: logged, durable when it is answered,
/// seen whole or not at all by a reader, and undone with its transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    /// What the server needs to know the role's password by. The engine
    /// keeps it and does not read it.
    pub secret: Vec<u8>,
    /// The rights it holds on every table, those made later included.
    pub everywhere: Rights,
    /// The rights it holds on single tables besides. No entry is empty.
    pub tables: BTreeMap<String, Rights>,
}

/// What each right of a set allows, as the words that go before a
/// table's name.
fn doing(rights: Rights) -> Vec<&'static str> {
    [(Rights::READ, "read"), (Rights::INSERT, "insert into"), (Rights::UPDATE, "update"), (Rights::DELETE, "delete from"), (Rights::SCHEMA, "change the schema of")]
        .into_iter()
        .filter(|(right, _)| rights.contains(*right))
        .map(|(_, words)| words)
        .collect()
}

impl Role {
    /// What the role may do to a table.
    pub fn rights_on(&self, table: &str) -> Rights {
        self.tables.get(table).map_or(self.everywhere, |rights| *rights | self.everywhere)
    }

    /// Fails unless the role holds every one of `rights` on `table`. Said
    /// the same whether or not there is such a table: a role is not told
    /// which names exist among those it has no right to.
    pub fn need(&self, table: &str, rights: Rights) -> Result<()> {
        let missing = rights.without(self.rights_on(table));
        if missing.is_empty() {
            return Ok(());
        }
        Err(Error::db(ErrorCode::AccessDenied, format!("role {:?} may not {} table {table:?}", self.name, doing(missing).join(" or "))))
    }

    /// Fails unless the role holds `rights` on every table: what a
    /// statement about no one table needs.
    pub fn need_everywhere(&self, rights: Rights, what: &str) -> Result<()> {
        if self.everywhere.contains(rights) {
            return Ok(());
        }
        Err(Error::db(ErrorCode::AccessDenied, format!("role {:?} may not {what}: that takes the right to {} every table", self.name, doing(rights.without(self.everywhere)).join(" and "))))
    }

    /// Whether the role may change anything at all.
    pub fn changes_something(&self) -> bool {
        let beyond_reading = Rights::ALL.without(Rights::READ);
        self.everywhere.intersects(beyond_reading) || self.tables.values().any(|rights| rights.intersects(beyond_reading))
    }
}

/// A role's name: what a database's may be, since the two are written
/// side by side in a greeting.
pub fn check_role_name(name: &str) -> Result<()> {
    let ok = !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !ok {
        return Err(invalid(format!("{name:?} is not a valid role name: use a-z, 0-9, _ and -, at most 64 of them")));
    }
    Ok(())
}

impl Role {
    /// A role by this name that holds nothing: what a connection runs as
    /// once its role has been dropped. Every table is then refused it.
    pub fn without_rights(name: &str) -> Role {
        Role { name: name.to_string(), secret: Vec::new(), everywhere: Rights::NONE, tables: BTreeMap::new() }
    }
}

/// The role of that name in a catalog, or one that holds nothing.
pub fn role_in(catalog: &Catalog, name: &str) -> Arc<Role> {
    catalog.roles.get(name).cloned().unwrap_or_else(|| Arc::new(Role::without_rights(name)))
}

/// A database's roles, by name.
pub type Roles = BTreeMap<String, Arc<Role>>;

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    /// By name. The names are shared, not copied, between the versions
    /// of the catalog, of which every writing transaction makes one.
    pub tables: BTreeMap<Arc<str>, Arc<Table>>,
    pub next_table_id: u32,
    /// Shared between versions until one changes them, and each role
    /// until it is changed: a role that is the same allocation in two
    /// versions is the same role.
    pub roles: Arc<Roles>,
}

/// The catalog as it has always been stored, and as it is stored with
/// roles after its tables. A database without roles is stored as the
/// first, byte for byte, so a build from before roles reads it; one with
/// roles is the second, which such a build refuses rather than serve the
/// database to its one password with the roles forgotten.
const STORED_PLAIN: u8 = 1;
const STORED_WITH_ROLES: u8 = 2;

/// Writes a role as the catalog and the log store it.
pub fn put_role(w: &mut Writer, role: &Role) {
    w.str(&role.name);
    w.bytes(&role.secret);
    w.u8(role.everywhere.bits());
    w.varint(role.tables.len() as u64);
    for (table, rights) in &role.tables {
        w.str(table);
        w.u8(rights.bits());
    }
}

pub fn get_role(r: &mut Reader) -> std::result::Result<Role, DbError> {
    let name = r.str()?;
    let secret = r.bytes()?.to_vec();
    let everywhere = Rights::from_bits(r.u8()?)?;
    let mut tables = BTreeMap::new();
    for _ in 0..r.count()? {
        let table = r.str()?;
        tables.insert(table, Rights::from_bits(r.u8()?)?);
    }
    Ok(Role { name, secret, everywhere, tables })
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Db(DbError::invalid(message))
}

/// Checks that a column's type can be stored: a decimal has at most 38
/// digits, and no more after the point than a decimal can keep.
fn check_type(column: &str, ty: &Type) -> Result<()> {
    match ty {
        Type::Decimal(Some((precision, scale))) if *precision == 0 || *precision > 38 || scale > precision || *scale > database_protocol::decimal::MAX_SCALE => {
            Err(invalid(format!("column {column:?}: decimal({precision}, {scale}) is not a size a decimal can have")))
        }
        Type::List(item) => check_type(column, item),
        _ => Ok(()),
    }
}

pub fn check_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 || name.contains(['.', '\0']) || name.chars().any(char::is_control) {
        return Err(invalid(format!("{name:?} is not a valid {kind} name")));
    }
    Ok(())
}

/// The column names an expression reads, outside any subquery.
pub fn columns_read(expr: &Expr) -> Vec<String> {
    let mut out = Vec::new();
    expr.visit(&mut |e| {
        if let Expr::Column(_, name) = e {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    });
    out
}

/// Refuses expressions that cannot be stored in a definition: ones whose
/// value is not fixed by the row alone.
fn check_stored_expr(what: &str, expr: &Expr, allow_clock: bool) -> Result<()> {
    let mut problem = None;
    expr.visit(&mut |e| match e {
        Expr::Subquery(..) => problem = Some("a subquery"),
        Expr::Agg { .. } => problem = Some("an aggregate"),
        Expr::Excluded(_) | Expr::Old(_) | Expr::Depth | Expr::Rank | Expr::Param(_) => problem = Some("a value that only a statement has"),
        Expr::Call(Func::Now | Func::RandomUuid, _) if !allow_clock => problem = Some("the clock or a random value"),
        _ => {}
    });
    match problem {
        Some(p) => Err(invalid(format!("{what} cannot use {p}"))),
        None => Ok(()),
    }
}

impl Table {
    /// Builds a new, empty table from a definition, checking it.
    pub fn create(def: &TableDef, id: u32, catalog: &Catalog) -> Result<Table> {
        check_name("table", &def.name)?;
        if def.columns.is_empty() {
            return Err(invalid(format!("table {:?} has no columns", def.name)));
        }
        let mut meta = TableMeta {
            id,
            name: def.name.clone(),
            cols: Vec::new(),
            by_name: NameMap::default(),
            pk: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            volatile: def.volatile,
            pk_parts: Default::default(),
        };
        for column in &def.columns {
            meta.add_column(column.clone(), Value::Null)?;
        }
        if def.primary_key.is_empty() {
            return Err(invalid(format!("table {:?} has no primary key", def.name)));
        }
        for name in &def.primary_key {
            let i = meta.col(name)?;
            if meta.pk.contains(&i) {
                return Err(invalid(format!("column {name:?} is in the primary key twice")));
            }
            // A key column is never NULL, whatever the definition said.
            meta.cols[i].def.nullable = false;
            meta.pk.push(i);
        }
        for index in &def.indexes {
            meta.add_index(index.clone())?;
        }
        for fk in &def.foreign_keys {
            let fk = meta.resolve_foreign_key(fk, catalog)?;
            meta.foreign_keys.push(fk);
        }
        for check in &def.checks {
            meta.add_check(check.clone())?;
        }
        Ok(Table::empty(meta))
    }

    /// A table of this definition with nothing in it.
    pub fn empty(meta: TableMeta) -> Table {
        Table { root: NO_PAGE, blobs: NO_PAGE, next_blob: 1, rows: 0, index_roots: vec![NO_PAGE; meta.indexes.len()], serials: vec![1; meta.cols.len()], meta: Arc::new(meta) }
    }
}

impl TableMeta {
    /// The index of a live column.
    pub fn col(&self, name: &str) -> Result<usize> {
        self.by_name.get(name).copied().ok_or_else(|| Error::Db(DbError::not_found(format!("table {:?} has no column {name:?}", self.name))))
    }

    /// How many bytes of a value's start column `i` keeps in the row
    /// when the value is stored outside it.
    pub fn keep_start(&self, i: usize) -> usize {
        self.cols.get(i).map_or(0, |col| col.def.keep_start as usize)
    }

    pub fn index(&self, name: &str) -> Result<usize> {
        self.indexes
            .iter()
            .position(|i| i.def.name == name)
            .ok_or_else(|| Error::Db(DbError::not_found(format!("table {:?} has no index {name:?}", self.name))))
    }

    pub fn add_column(&mut self, def: ColumnDef, fill: Value) -> Result<usize> {
        check_name("column", &def.name)?;
        if self.by_name.contains_key(&def.name) {
            return Err(Error::db(ErrorCode::AlreadyExists, format!("table {:?} already has a column {:?}", self.name, def.name)));
        }
        if def.auto_increment && def.ty != Type::Int {
            return Err(invalid(format!("column {:?} numbers rows, so it must be an int", def.name)));
        }
        check_type(&def.name, &def.ty)?;
        if def.keep_start > 0 {
            if !matches!(def.ty, Type::Text | Type::Bytes) {
                return Err(invalid(format!("column {:?} keeps the start of its values in the row, which only a text or bytes column can", def.name)));
            }
            if def.keep_start > MAX_KEEP_START {
                return Err(invalid(format!("column {:?} keeps {} bytes of its values in the row, and a row has room for {MAX_KEEP_START} at most", def.name, def.keep_start)));
            }
        }
        if let Some(default) = &def.default {
            check_stored_expr("a default", default, true)?;
            if !columns_read(default).is_empty() {
                return Err(invalid("a default cannot read other columns"));
            }
        }
        self.by_name.insert(def.name.clone(), self.cols.len());
        self.cols.push(Column { def, fill, dropped: false });
        Ok(self.cols.len() - 1)
    }

    fn reads(&self, exprs: &[&Expr]) -> Result<Vec<usize>> {
        let mut out = Vec::new();
        for expr in exprs {
            for name in columns_read(expr) {
                let i = self.col(&name)?;
                if !out.contains(&i) {
                    out.push(i);
                }
            }
        }
        Ok(out)
    }

    /// Checks an index definition against the table and adds it, empty.
    pub fn add_index(&mut self, def: IndexDef) -> Result<usize> {
        check_name("index", &def.name)?;
        if def.name == PRIMARY || self.indexes.iter().any(|i| i.def.name == def.name) {
            return Err(Error::db(ErrorCode::AlreadyExists, format!("table {:?} already has an index {:?}", self.name, def.name)));
        }
        let mut exprs: Vec<&Expr> = def.keys.iter().map(|k| &k.expr).collect();
        exprs.extend(&def.filter);
        match &def.kind {
            IndexKind::Ordered => {
                if def.keys.is_empty() {
                    return Err(invalid(format!("index {:?} has no keys", def.name)));
                }
            }
            IndexKind::FullText { fields, .. } => {
                if fields.is_empty() {
                    return Err(invalid(format!("full-text index {:?} has no text", def.name)));
                }
                if def.unique || def.keys.iter().any(|k| k.each || k.desc) {
                    return Err(invalid(format!("full-text index {:?}: its keys are a plain prefix, and it cannot be unique", def.name)));
                }
                exprs.extend(fields.iter().map(|f| &f.expr));
            }
        }
        for expr in &exprs {
            check_stored_expr("an index", expr, false)?;
        }
        let reads = self.reads(&exprs)?;
        self.indexes.push(Index { def, reads });
        Ok(self.indexes.len() - 1)
    }

    pub fn add_check(&mut self, check: Check) -> Result<()> {
        check_name("check", &check.name)?;
        check_stored_expr("a check", &check.expr, false)?;
        self.reads(&[&check.expr])?;
        self.checks.push(check);
        Ok(())
    }

    /// Fills in the parent's key columns and checks the two sides match.
    pub fn resolve_foreign_key(&self, fk: &ForeignKey, catalog: &Catalog) -> Result<ForeignKey> {
        let mut fk = fk.clone();
        let parent: &TableMeta = if fk.ref_table == self.name {
            self
        } else {
            catalog.tables.get(fk.ref_table.as_str()).map(|t| &*t.meta).ok_or_else(|| {
                Error::Db(DbError::not_found(format!("foreign key {:?} points at table {:?}, which does not exist", fk.name, fk.ref_table)))
            })?
        };
        if fk.ref_columns.is_empty() {
            fk.ref_columns = parent.pk.iter().map(|i| parent.cols[*i].def.name.clone()).collect();
        }
        if fk.columns.is_empty() || fk.columns.len() != fk.ref_columns.len() {
            return Err(invalid(format!("foreign key {:?} must name as many columns as the key it points at", fk.name)));
        }
        if parent.unique_key(&fk.ref_columns).is_none() {
            return Err(invalid(format!("foreign key {:?} must point at the primary key or a unique index of {:?}", fk.name, fk.ref_table)));
        }
        // A volatile table is empty after a restart: rows that outlive
        // one cannot be made to depend on its rows.
        if parent.volatile && !self.volatile {
            return Err(invalid(format!("foreign key {:?} points at {:?}, whose rows do not survive a restart, from a table whose rows do", fk.name, fk.ref_table)));
        }
        for (child, parent_col) in fk.columns.iter().zip(&fk.ref_columns) {
            let c = &self.cols[self.col(child)?].def;
            let p = &parent.cols[parent.col(parent_col)?].def;
            if std::mem::discriminant(&c.ty) != std::mem::discriminant(&p.ty) {
                return Err(invalid(format!("foreign key {:?}: {child:?} and {parent_col:?} have different types", fk.name)));
            }
            if fk.on_delete == OnDelete::SetNull && !c.nullable {
                return Err(invalid(format!("foreign key {:?} sets {child:?} to NULL, but that column cannot be NULL", fk.name)));
            }
        }
        Ok(fk)
    }

    /// The unique key made of exactly these columns: `None` for the
    /// primary key, the index's position otherwise. Outer `None` if no
    /// unique key has these columns.
    pub fn unique_key(&self, columns: &[String]) -> Option<Option<usize>> {
        let pk: Vec<&str> = self.pk.iter().map(|i| self.cols[*i].def.name.as_str()).collect();
        if pk.iter().copied().eq(columns.iter().map(String::as_str)) {
            return Some(None);
        }
        self.indexes.iter().position(|index| index.def.unique && index.def.filter.is_none() && plain_columns(&index.def).is_some_and(|c| c == *columns)).map(Some)
    }

    /// The definition as a client gave it and reads it back.
    pub fn def(&self) -> TableDef {
        TableDef {
            name: self.name.clone(),
            columns: self.cols.iter().filter(|c| !c.dropped).map(|c| c.def.clone()).collect(),
            primary_key: self.pk.iter().map(|i| self.cols[*i].def.name.clone()).collect(),
            indexes: self.indexes.iter().map(|i| i.def.clone()).collect(),
            foreign_keys: self.foreign_keys.clone(),
            checks: self.checks.clone(),
            volatile: self.volatile,
        }
    }
}

/// An index's keys as column names, if they are all plain ascending
/// columns.
pub fn plain_columns(def: &IndexDef) -> Option<Vec<String>> {
    def.keys
        .iter()
        .map(|k| match &k.expr {
            Expr::Column(None, name) if !k.each => Some(name.clone()),
            _ => None,
        })
        .collect()
}

impl Catalog {
    pub fn table(&self, name: &str) -> Result<&Arc<Table>> {
        self.tables.get(name).ok_or_else(|| Error::Db(DbError::not_found(format!("no table {name:?}"))))
    }

    pub fn table_by_id(&self, id: u32) -> Option<&Arc<Table>> {
        self.tables.values().find(|t| t.id == id)
    }

    /// The tables with a foreign key pointing at `parent`, each with the
    /// keys that do.
    pub fn children_of(&self, parent: &str) -> Vec<(Arc<Table>, Vec<ForeignKey>)> {
        let mut out = Vec::new();
        for table in self.tables.values() {
            let fks: Vec<ForeignKey> = table.foreign_keys.iter().filter(|fk| fk.ref_table == parent).cloned().collect();
            if !fks.is_empty() {
                out.push((table.clone(), fks));
            }
        }
        out
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(if self.roles.is_empty() { STORED_PLAIN } else { STORED_WITH_ROLES });
        w.u32(self.next_table_id);
        w.varint(self.tables.len() as u64);
        for table in self.tables.values() {
            w.u32(table.id);
            w.str(&table.name);
            w.varint(table.cols.len() as u64);
            for (col, serial) in table.cols.iter().zip(&table.serials) {
                codec::put_column(&mut w, &col.def);
                codec::put_value(&mut w, &col.fill);
                w.bool(col.dropped);
                w.zigzag(*serial);
            }
            w.varint(table.pk.len() as u64);
            for i in &table.pk {
                w.varint(*i as u64);
            }
            w.u32(table.root);
            w.u32(table.blobs);
            w.varint(table.next_blob);
            w.varint(table.rows);
            w.varint(table.indexes.len() as u64);
            for (index, root) in table.indexes.iter().zip(&table.index_roots) {
                codec::put_index(&mut w, &index.def);
                w.u32(*root);
            }
            // The foreign keys and checks travel inside a table
            // definition that holds nothing else.
            let rest = TableDef {
                name: String::new(),
                columns: Vec::new(),
                primary_key: Vec::new(),
                indexes: Vec::new(),
                foreign_keys: table.foreign_keys.clone(),
                checks: table.checks.clone(),
                volatile: table.volatile,
            };
            codec::put_table(&mut w, &rest);
        }
        if !self.roles.is_empty() {
            w.varint(self.roles.len() as u64);
            for role in self.roles.values() {
                put_role(&mut w, role);
            }
        }
        w.buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Catalog> {
        if bytes.is_empty() {
            return Ok(Catalog { tables: BTreeMap::new(), next_table_id: 1, roles: Arc::default() });
        }
        let bad = |e: DbError| Error::corrupt(format!("the stored catalog is damaged: {}", e.message));
        let mut r = Reader::new(bytes);
        let stored = r.u8().map_err(bad)?;
        if stored != STORED_PLAIN && stored != STORED_WITH_ROLES {
            return Err(Error::corrupt("the stored catalog is of an unknown version"));
        }
        let mut catalog = Catalog { tables: BTreeMap::new(), next_table_id: r.u32().map_err(bad)?, roles: Arc::default() };
        for _ in 0..r.count().map_err(bad)? {
            let id = r.u32().map_err(bad)?;
            let name = r.str().map_err(bad)?;
            let mut cols = Vec::new();
            let mut serials = Vec::new();
            let mut by_name = NameMap::default();
            for i in 0..r.count().map_err(bad)? {
                let col = Column { def: codec::get_column(&mut r).map_err(bad)?, fill: codec::get_value(&mut r).map_err(bad)?, dropped: r.bool().map_err(bad)? };
                serials.push(r.zigzag().map_err(bad)?);
                if !col.dropped {
                    by_name.insert(col.def.name.clone(), i);
                }
                cols.push(col);
            }
            let mut pk = Vec::new();
            for _ in 0..r.count().map_err(bad)? {
                pk.push(r.varint().map_err(bad)? as usize);
            }
            if pk.iter().any(|i| *i >= cols.len()) {
                return Err(Error::corrupt("the stored catalog names a key column that does not exist"));
            }
            let mut meta = TableMeta { id, name, cols, by_name, pk, indexes: Vec::new(), foreign_keys: Vec::new(), checks: Vec::new(), volatile: false, pk_parts: Default::default() };
            let root = r.u32().map_err(bad)?;
            let blobs = r.u32().map_err(bad)?;
            let next_blob = r.varint().map_err(bad)?;
            let rows = r.varint().map_err(bad)?;
            let mut index_roots = Vec::new();
            for _ in 0..r.count().map_err(bad)? {
                let def = codec::get_index(&mut r).map_err(bad)?;
                index_roots.push(r.u32().map_err(bad)?);
                meta.add_index(def).map_err(|e| Error::corrupt(format!("the stored catalog holds a bad index: {e}")))?;
            }
            let rest = codec::get_table(&mut r).map_err(bad)?;
            meta.foreign_keys = rest.foreign_keys;
            meta.checks = rest.checks;
            meta.volatile = rest.volatile;
            let table = Table { meta: Arc::new(meta), root, blobs, next_blob, rows, index_roots, serials };
            catalog.tables.insert(Arc::from(table.name.as_str()), Arc::new(table));
        }
        if stored == STORED_WITH_ROLES {
            let mut roles = Roles::new();
            for _ in 0..r.count().map_err(bad)? {
                let role = get_role(&mut r).map_err(bad)?;
                roles.insert(role.name.clone(), Arc::new(role));
            }
            catalog.roles = Arc::new(roles);
        }
        Ok(catalog)
    }

    /// Every table's definition, parents before the children that point
    /// at them, so that they can be created in this order.
    pub fn defs_in_order(&self) -> Vec<TableDef> {
        let mut out = Vec::new();
        let mut done: HashSet<&str> = HashSet::new();
        let mut left: Vec<&Arc<Table>> = self.tables.values().collect();
        while !left.is_empty() {
            let before = left.len();
            left.retain(|table| {
                let ready = table.foreign_keys.iter().all(|fk| fk.ref_table == table.name || done.contains(fk.ref_table.as_str()));
                if ready {
                    done.insert(&table.name);
                    out.push(table.def());
                }
                !ready
            });
            if left.len() == before {
                // Tables that point at each other in a circle: any order.
                out.extend(left.iter().map(|t| t.def()));
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use database_protocol::expr::{col, now};

    fn users() -> TableDef {
        TableDef::new("users")
            .column(ColumnDef::new("id", Type::Uuid).default_random_uuid())
            .column(ColumnDef::new("email", Type::Text).nullable())
            .column(ColumnDef::new("n", Type::Int).auto_increment())
            .primary_key(["id"])
            .unique("users_n", ["n"])
            .index(IndexDef::new("users_email").on_expr(col("email").lower()).unique().filter(col("email").is_not_null()))
            .check("n_positive", col("n").gt(0))
    }

    #[test]
    fn a_catalog_survives_being_stored() {
        let mut catalog = Catalog { tables: BTreeMap::new(), next_table_id: 3, roles: Arc::default() };
        let mut users = Table::create(&users(), 1, &catalog).unwrap();
        users.root = 17;
        users.rows = 5;
        users.serials[2] = 6;
        users.index_roots[1] = 23;
        catalog.tables.insert("users".into(), Arc::new(users));
        let posts = TableDef::new("posts")
            .column(ColumnDef::new("id", Type::Int))
            .column(ColumnDef::new("author", Type::Uuid).nullable())
            .primary_key(["id"])
            .foreign_key(["author"], "users", OnDelete::SetNull)
            .volatile();
        let posts = Table::create(&posts, 2, &catalog).unwrap();
        assert_eq!(posts.foreign_keys[0].ref_columns, ["id"]);
        catalog.tables.insert("posts".into(), Arc::new(posts));

        let back = Catalog::decode(&catalog.encode()).unwrap();
        assert_eq!(back.next_table_id, 3);
        let u = back.table("users").unwrap();
        assert_eq!((u.root, u.rows, u.serials[2], u.index_roots[1]), (17, 5, 6, 23));
        assert_eq!(u.def(), catalog.table("users").unwrap().def());
        assert_eq!(u.indexes[1].reads, vec![1]);
        assert!(back.table("posts").unwrap().volatile);
        assert_eq!(back.defs_in_order().iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["users", "posts"]);
        assert_eq!(back.children_of("users").len(), 1);

        // Without roles it is stored as it was before there were any;
        // with them, they come back as they were, and the stored form
        // says so in its first byte, which an older build refuses.
        let plain = catalog.encode();
        assert_eq!(plain[0], 1);
        let reader = Role { name: "reader".into(), secret: vec![7; 48], everywhere: Rights::READ, tables: BTreeMap::from([("posts".to_string(), Rights::INSERT | Rights::DELETE)]) };
        let nobody = Role { name: "nobody".into(), secret: Vec::new(), everywhere: Rights::NONE, tables: BTreeMap::new() };
        catalog.roles = Arc::new(BTreeMap::from([("reader".to_string(), Arc::new(reader.clone())), ("nobody".to_string(), Arc::new(nobody.clone()))]));
        let stored = catalog.encode();
        assert_eq!((stored[0], &stored[1..plain.len()]), (2, &plain[1..]));
        let back = Catalog::decode(&stored).unwrap();
        assert_eq!(back.roles.values().map(|role| (**role).clone()).collect::<Vec<_>>(), [nobody, reader]);
        assert_eq!(back.tables.len(), 2);
    }

    #[test]
    fn a_role_holds_what_it_was_granted_on_a_table_or_on_all() {
        let role = Role { name: "clerk".into(), secret: Vec::new(), everywhere: Rights::READ, tables: BTreeMap::from([("orders".to_string(), Rights::INSERT)]) };
        assert_eq!(role.rights_on("orders"), Rights::READ | Rights::INSERT);
        assert_eq!(role.rights_on("anything"), Rights::READ);
        assert!(role.need("orders", Rights::READ | Rights::INSERT).is_ok());
        let refused = role.need("orders", Rights::READ | Rights::UPDATE | Rights::DELETE).unwrap_err().into_db();
        assert_eq!(refused.code, ErrorCode::AccessDenied);
        assert_eq!(refused.message, "role \"clerk\" may not update or delete from table \"orders\"");
        assert!(role.need_everywhere(Rights::READ, "look").is_ok() && role.need_everywhere(Rights::SCHEMA, "create a table").is_err());
        assert!(role.changes_something());
        assert!(!Role { tables: BTreeMap::new(), ..role }.changes_something());
        for bad in ["", "Reader", "a/b", "a b", &"x".repeat(65)] {
            assert!(check_role_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_role_name("report-reader_2").is_ok());
    }

    #[test]
    fn bad_definitions_are_refused() {
        let catalog = Catalog::default();
        let base = || TableDef::new("t").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("x", Type::Text).nullable()).primary_key(["id"]);
        let refused = |def: TableDef| Table::create(&def, 1, &catalog).is_err();
        assert!(!refused(base()));
        assert!(refused(TableDef::new("t").column(ColumnDef::new("id", Type::Int))), "no primary key");
        assert!(refused(TableDef::new("t").primary_key(["id"])), "no columns");
        assert!(refused(base().primary_key(["missing"])));
        assert!(refused(base().column(ColumnDef::new("x", Type::Int))), "a column twice");
        assert!(refused(base().index(IndexDef::new("i").on(["missing"]))));
        assert!(refused(base().index(IndexDef::new("i").on_expr(now()))), "an index on the clock");
        assert!(refused(base().index(IndexDef::new("primary").on(["x"]))));
        assert!(refused(base().index(IndexDef::new("i").on(["x"])).index(IndexDef::new("i").on(["x"]))));
        assert!(refused(base().foreign_key(["x"], "nowhere", OnDelete::Cascade)));
        assert!(refused(base().column(ColumnDef::new("a.b", Type::Int))));
        assert!(refused(base().column(ColumnDef::new("s", Type::Text).auto_increment())));
        assert!(refused(base().column(ColumnDef::new("d", Type::Int).default_expr(col("id")))));
        assert!(refused(base().check("c", col("missing").gt(0))));
        // A foreign key must point at a whole unique key, with matching types.
        let mut with_parent = Catalog::default();
        with_parent.tables.insert("users".into(), Arc::new(Table::create(&users(), 1, &Catalog::default()).unwrap()));
        let child = |fk: ForeignKey| {
            let mut def = TableDef::new("c").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("u", Type::Uuid)).column(ColumnDef::new("e", Type::Text)).primary_key(["id"]);
            def.foreign_keys.push(fk);
            Table::create(&def, 2, &with_parent)
        };
        let fk = |cols: &[&str], refs: &[&str], on_delete| ForeignKey {
            name: "fk".into(),
            columns: cols.iter().map(|s| s.to_string()).collect(),
            ref_table: "users".into(),
            ref_columns: refs.iter().map(|s| s.to_string()).collect(),
            on_delete,
        };
        assert!(child(fk(&["u"], &[], OnDelete::Cascade)).is_ok());
        assert!(child(fk(&["id"], &["n"], OnDelete::Cascade)).is_ok(), "a unique index will do");
        assert!(child(fk(&["e"], &["email"], OnDelete::Cascade)).is_err(), "not a unique key of plain columns");
        assert!(child(fk(&["id"], &["id"], OnDelete::Cascade)).is_err(), "int against uuid");
        assert!(child(fk(&["u"], &[], OnDelete::SetNull)).is_err(), "cannot set a non-null column to NULL");
    }
}
