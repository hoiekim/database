//! Table definitions: columns, the primary key, indexes, foreign keys and
//! checks.

use crate::expr::{col, Expr};
use crate::value::{Type, Value};

/// The name of a table's primary key where an index name is asked for:
/// as an upsert's conflict target, or in a unique violation.
pub const PRIMARY: &str = "primary";

#[derive(Clone, Debug, PartialEq)]
pub struct TableDef {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Column names. Every table has a primary key, and its columns cannot
    /// be NULL.
    pub primary_key: Vec<String>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKey>,
    pub checks: Vec<Check>,
    /// A table whose rows need not survive a crash or a restart, such as
    /// rate-limit counters. Its writes skip the log, and it is empty when
    /// the server starts.
    pub volatile: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: Type,
    pub nullable: bool,
    /// What an insert that leaves the column out stores. Evaluated per
    /// row: a constant, `now()`, a random uuid.
    pub default: Option<Expr>,
    /// An integer column that numbers rows: an insert that leaves it out
    /// gets one more than the highest value the column has ever held.
    pub auto_increment: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub keys: Vec<IndexKey>,
    pub unique: bool,
    /// Only rows for which this is true are in the index. A unique index
    /// with a filter constrains only those rows.
    pub filter: Option<Expr>,
    pub kind: IndexKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexKey {
    /// A column, or an expression over the row's columns such as
    /// `col("email").lower()`.
    pub expr: Expr,
    pub desc: bool,
    /// The expression gives a list, and the row is indexed once under each
    /// distinct item: an index over the members of an array.
    pub each: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum IndexKind {
    /// Ordered by the keys.
    Ordered,
    /// Words of the given text expressions, for full-text search. The
    /// index's keys are a prefix every search must give in full (say, the
    /// owner's id), so each owner's words are kept apart.
    FullText { config: TextConfig, fields: Vec<TextField> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextField {
    pub expr: Expr,
    /// How much a match in this field counts toward the rank.
    pub weight: f64,
}

/// How text is cut into words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TextConfig {
    /// Runs of letters and digits, lower-cased.
    Simple = 0,
    /// The same, without common English words, and with English endings
    /// removed so that `invoices` matches `invoice`.
    English = 1,
}

impl TextConfig {
    pub fn from_u8(v: u8) -> Option<TextConfig> {
        match v {
            0 => Some(TextConfig::Simple),
            1 => Some(TextConfig::English),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TextConfig::Simple => "simple",
            TextConfig::English => "english",
        }
    }

    pub fn from_name(name: &str) -> Option<TextConfig> {
        match name {
            "simple" => Some(TextConfig::Simple),
            "english" => Some(TextConfig::English),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    /// The parent's primary key columns, or those of one of its unique
    /// indexes.
    pub ref_columns: Vec<String>,
    pub on_delete: OnDelete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OnDelete {
    /// Refuse to delete a parent that still has children.
    Restrict = 0,
    /// Delete the children with it.
    Cascade = 1,
    /// Set the children's columns to NULL.
    SetNull = 2,
}

impl OnDelete {
    pub fn from_u8(v: u8) -> Option<OnDelete> {
        match v {
            0 => Some(OnDelete::Restrict),
            1 => Some(OnDelete::Cascade),
            2 => Some(OnDelete::SetNull),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    /// A row is refused when this is false. NULL passes, as in SQL.
    pub expr: Expr,
}

// --------------------------------------------------------------- building

impl TableDef {
    pub fn new(name: &str) -> TableDef {
        TableDef {
            name: name.to_string(),
            columns: Vec::new(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            volatile: false,
        }
    }

    pub fn column(mut self, column: ColumnDef) -> TableDef {
        self.columns.push(column);
        self
    }

    pub fn primary_key<S: AsRef<str>>(mut self, columns: impl IntoIterator<Item = S>) -> TableDef {
        self.primary_key = columns.into_iter().map(|c| c.as_ref().to_string()).collect();
        self
    }

    pub fn index(mut self, index: IndexDef) -> TableDef {
        self.indexes.push(index);
        self
    }

    /// A unique index over plain columns.
    pub fn unique<S: AsRef<str>>(self, name: &str, columns: impl IntoIterator<Item = S>) -> TableDef {
        self.index(IndexDef::new(name).on(columns).unique())
    }

    /// A foreign key to the parent's primary key, named
    /// `<table>_<column>_fkey`.
    pub fn foreign_key<S: AsRef<str>>(mut self, columns: impl IntoIterator<Item = S>, ref_table: &str, on_delete: OnDelete) -> TableDef {
        let columns: Vec<String> = columns.into_iter().map(|c| c.as_ref().to_string()).collect();
        let name = format!("{}_{}_fkey", self.name, columns.join("_"));
        self.foreign_keys.push(ForeignKey { name, columns, ref_table: ref_table.to_string(), ref_columns: Vec::new(), on_delete });
        self
    }

    pub fn check(mut self, name: &str, expr: Expr) -> TableDef {
        self.checks.push(Check { name: name.to_string(), expr });
        self
    }

    pub fn volatile(mut self) -> TableDef {
        self.volatile = true;
        self
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn find_index(&self, name: &str) -> Option<&IndexDef> {
        self.indexes.iter().find(|i| i.name == name)
    }
}

impl ColumnDef {
    /// A column that cannot be NULL and has no default.
    pub fn new(name: &str, ty: Type) -> ColumnDef {
        ColumnDef { name: name.to_string(), ty, nullable: false, default: None, auto_increment: false }
    }

    pub fn nullable(mut self) -> ColumnDef {
        self.nullable = true;
        self
    }

    pub fn default(mut self, value: impl Into<Value>) -> ColumnDef {
        self.default = Some(Expr::Value(value.into()));
        self
    }

    pub fn default_expr(mut self, expr: Expr) -> ColumnDef {
        self.default = Some(expr);
        self
    }

    pub fn default_now(self) -> ColumnDef {
        self.default_expr(crate::expr::now())
    }

    pub fn default_random_uuid(self) -> ColumnDef {
        self.default_expr(Expr::Call(crate::expr::Func::RandomUuid, Vec::new()))
    }

    pub fn auto_increment(mut self) -> ColumnDef {
        self.auto_increment = true;
        self
    }
}

impl IndexDef {
    pub fn new(name: &str) -> IndexDef {
        IndexDef { name: name.to_string(), keys: Vec::new(), unique: false, filter: None, kind: IndexKind::Ordered }
    }

    /// Adds plain columns as keys, ascending.
    pub fn on<S: AsRef<str>>(mut self, columns: impl IntoIterator<Item = S>) -> IndexDef {
        self.keys.extend(columns.into_iter().map(|c| IndexKey { expr: col(c.as_ref()), desc: false, each: false }));
        self
    }

    /// Adds an expression as a key.
    pub fn on_expr(mut self, expr: Expr) -> IndexDef {
        self.keys.push(IndexKey { expr, desc: false, each: false });
        self
    }

    /// Adds a column as a key, descending.
    pub fn on_desc(mut self, column: &str) -> IndexDef {
        self.keys.push(IndexKey { expr: col(column), desc: true, each: false });
        self
    }

    /// Adds a list-valued expression as a key, indexing the row once under
    /// each distinct item.
    pub fn on_each(mut self, expr: Expr) -> IndexDef {
        self.keys.push(IndexKey { expr, desc: false, each: true });
        self
    }

    pub fn unique(mut self) -> IndexDef {
        self.unique = true;
        self
    }

    /// Makes the index partial: only rows for which `filter` is true.
    pub fn filter(mut self, filter: Expr) -> IndexDef {
        self.filter = Some(filter);
        self
    }

    /// Makes this a full-text index over the given text expressions.
    pub fn full_text(mut self, config: TextConfig, fields: impl IntoIterator<Item = (Expr, f64)>) -> IndexDef {
        let fields = fields.into_iter().map(|(expr, weight)| TextField { expr, weight }).collect();
        self.kind = IndexKind::FullText { config, fields };
        self
    }
}
