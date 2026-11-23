//! Reads and writes as the client states them.
//!
//! There is no planner. A query names the way in (`Access`): the primary
//! key, one index, a walk of a tree, or a full-text search, and the range
//! of keys to read. The server reads those rows in key order, then applies
//! the rest in a fixed sequence: joins, filter, grouping, sort, offset and
//! limit, select. A filter on a column is only ever as fast as the range
//! chosen for it.

use crate::expr::Expr;
use crate::schema::TextConfig;

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub table: String,
    /// What the table is called in expressions, when the same table is
    /// read twice or a join needs to tell columns apart.
    pub alias: Option<String>,
    pub access: Access,
    pub joins: Vec<Join>,
    /// A list computed per row; the row is then seen once for each
    /// distinct item of it, with the item as a column of the given name.
    /// A row whose list is empty or NULL is dropped.
    pub unnest: Option<Select>,
    pub filter: Option<Expr>,
    /// With `group_by` or an aggregate in `select`, rows are grouped and
    /// each group gives one row.
    pub group_by: Vec<Expr>,
    /// Tested per group, after aggregation.
    pub having: Option<Expr>,
    pub order_by: Vec<Order>,
    pub offset: u64,
    pub limit: Option<u64>,
    /// Empty for every column of `table`.
    pub select: Vec<Select>,
    pub distinct: bool,
    /// Also count the rows the query would give without `offset` and
    /// `limit`.
    pub with_total: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub name: String,
    pub expr: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub expr: Expr,
    pub desc: bool,
    /// Where NULLs go. `None` puts them last ascending and first
    /// descending, as Postgres does.
    pub nulls_last: Option<bool>,
}

impl Order {
    pub fn nulls_last(mut self) -> Order {
        self.nulls_last = Some(true);
        self
    }
    pub fn nulls_first(mut self) -> Order {
        self.nulls_last = Some(false);
        self
    }
}

impl From<Expr> for Order {
    fn from(expr: Expr) -> Self {
        Order { expr, desc: false, nulls_last: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Access {
    /// Rows by primary key, in key order.
    Primary { ranges: Vec<KeyRange>, reverse: bool },
    /// Rows through an index, in the index's order.
    Index { index: String, ranges: Vec<KeyRange>, reverse: bool },
    /// Rows of a tree held in one table, where `parent` is the column
    /// pointing at the parent row's primary key. Starts at the rows with
    /// the given primary keys (depth 0) and follows the links, breadth
    /// first, reaching each row once.
    Walk {
        start: Vec<Vec<Expr>>,
        parent: String,
        direction: Direction,
        /// For `Down`: an index whose first key is the `parent` column.
        /// Children are read in its order.
        index: Option<String>,
        /// A row for which this is false is not reached, and nothing
        /// beyond it. Not applied to the start rows.
        prune: Option<Expr>,
        /// A row for which this is true is reached but not followed.
        stop: Option<Expr>,
        max_depth: u32,
    },
    /// Rows matching a full-text search, through a full-text index. Rows
    /// come in no particular order: sort by `Expr::Rank`.
    Search { index: String, prefix: Vec<Expr>, query: Expr, match_prefix: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    /// From a row to its parent, and on up.
    Up = 0,
    /// From a row to its children, and on down.
    Down = 1,
}

/// A span of keys. Each bound is a prefix of the key: with an index on
/// `(user_id, date)`, the range from `Included([u])` to `Included([u])` is
/// every entry of that user, and from `Excluded([u, d])` on is that user's
/// and later users' entries after `d`.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyRange {
    pub lower: Bound,
    pub upper: Bound,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Bound {
    Unbounded,
    Included(Vec<Expr>),
    Excluded(Vec<Expr>),
}

fn exprs<E: Into<Expr>>(values: impl IntoIterator<Item = E>) -> Vec<Expr> {
    values.into_iter().map(Into::into).collect()
}

impl KeyRange {
    pub fn all() -> KeyRange {
        KeyRange { lower: Bound::Unbounded, upper: Bound::Unbounded }
    }

    /// Every key that starts with these values. Given the whole key of a
    /// unique index, at most one row.
    pub fn eq<E: Into<Expr>>(prefix: impl IntoIterator<Item = E>) -> KeyRange {
        let prefix = exprs(prefix);
        KeyRange { lower: Bound::Included(prefix.clone()), upper: Bound::Included(prefix) }
    }

    pub fn new(lower: Bound, upper: Bound) -> KeyRange {
        KeyRange { lower, upper }
    }

    /// Keys starting with `prefix`, whose next part lies between the two
    /// bounds.
    pub fn within<E: Into<Expr>>(prefix: impl IntoIterator<Item = E>, lower: Bound, upper: Bound) -> KeyRange {
        let prefix = exprs(prefix);
        let extend = |bound: Bound, open_end: bool| match bound {
            Bound::Unbounded if prefix.is_empty() => Bound::Unbounded,
            Bound::Unbounded => {
                let _ = open_end;
                Bound::Included(prefix.clone())
            }
            Bound::Included(rest) => Bound::Included(prefix.iter().cloned().chain(rest).collect()),
            Bound::Excluded(rest) => Bound::Excluded(prefix.iter().cloned().chain(rest).collect()),
        };
        KeyRange { lower: extend(lower, false), upper: extend(upper, true) }
    }
}

impl Bound {
    pub fn included<E: Into<Expr>>(values: impl IntoIterator<Item = E>) -> Bound {
        Bound::Included(exprs(values))
    }
    pub fn excluded<E: Into<Expr>>(values: impl IntoIterator<Item = E>) -> Bound {
        Bound::Excluded(exprs(values))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    /// Run once per row so far. Its ranges and filter may read that row's
    /// columns, which is how the two are matched; with a limit it gives
    /// the first few matches per row.
    pub query: Query,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JoinKind {
    /// Rows without a match are dropped.
    Inner = 0,
    /// Rows without a match are kept, with NULL for the joined columns.
    Left = 1,
}

impl Query {
    /// Every row of a table, in primary key order, until narrowed.
    pub fn table(name: &str) -> Query {
        Query {
            table: name.to_string(),
            alias: None,
            access: Access::Primary { ranges: vec![KeyRange::all()], reverse: false },
            joins: Vec::new(),
            unnest: None,
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            offset: 0,
            limit: None,
            select: Vec::new(),
            distinct: false,
            with_total: false,
        }
    }

    pub fn alias(mut self, alias: &str) -> Query {
        self.alias = Some(alias.to_string());
        self
    }

    /// The row with this primary key.
    pub fn key<E: Into<Expr>>(mut self, key: impl IntoIterator<Item = E>) -> Query {
        self.access = Access::Primary { ranges: vec![KeyRange::eq(key)], reverse: false };
        self
    }

    /// The rows with these primary keys, in the order given.
    pub fn keys<E: Into<Expr>, K: IntoIterator<Item = E>>(mut self, keys: impl IntoIterator<Item = K>) -> Query {
        self.access = Access::Primary { ranges: keys.into_iter().map(KeyRange::eq).collect(), reverse: false };
        self
    }

    /// Rows whose primary key is in the range.
    pub fn range(mut self, range: KeyRange) -> Query {
        self.access = Access::Primary { ranges: vec![range], reverse: false };
        self
    }

    /// Rows through an index, in its order.
    pub fn index(mut self, index: &str, range: KeyRange) -> Query {
        self.access = Access::Index { index: index.to_string(), ranges: vec![range], reverse: false };
        self
    }

    /// Rows through an index, one range after another: `IN` over a key.
    pub fn index_ranges(mut self, index: &str, ranges: impl IntoIterator<Item = KeyRange>) -> Query {
        self.access = Access::Index { index: index.to_string(), ranges: ranges.into_iter().collect(), reverse: false };
        self
    }

    /// Reads the chosen range from its far end.
    pub fn reverse(mut self) -> Query {
        match &mut self.access {
            Access::Primary { reverse, .. } | Access::Index { reverse, .. } => *reverse = true,
            _ => {}
        }
        self
    }

    /// The rows above the start rows: each one's parent, grandparent, and
    /// so on. `parent` is the column holding the parent's primary key.
    pub fn ancestors<E: Into<Expr>, K: IntoIterator<Item = E>>(mut self, start: impl IntoIterator<Item = K>, parent: &str) -> Query {
        self.access = Access::Walk {
            start: start.into_iter().map(exprs).collect(),
            parent: parent.to_string(),
            direction: Direction::Up,
            index: None,
            prune: None,
            stop: None,
            max_depth: 1000,
        };
        self
    }

    /// The rows below the start rows. `index` is an index whose first key
    /// is the `parent` column.
    pub fn descendants<E: Into<Expr>, K: IntoIterator<Item = E>>(mut self, start: impl IntoIterator<Item = K>, parent: &str, index: &str) -> Query {
        self.access = Access::Walk {
            start: start.into_iter().map(exprs).collect(),
            parent: parent.to_string(),
            direction: Direction::Down,
            index: Some(index.to_string()),
            prune: None,
            stop: None,
            max_depth: 1000,
        };
        self
    }

    /// In a walk: do not reach rows for which this is false, nor anything
    /// beyond them.
    pub fn prune(mut self, keep: Expr) -> Query {
        if let Access::Walk { prune, .. } = &mut self.access {
            *prune = Some(keep);
        }
        self
    }

    /// In a walk: reach rows for which this is true, but go no further.
    pub fn stop_at(mut self, condition: Expr) -> Query {
        if let Access::Walk { stop, .. } = &mut self.access {
            *stop = Some(condition);
        }
        self
    }

    pub fn max_depth(mut self, depth: u32) -> Query {
        if let Access::Walk { max_depth, .. } = &mut self.access {
            *max_depth = depth;
        }
        self
    }

    /// Rows whose indexed text holds every word of `query`.
    pub fn search<E: Into<Expr>>(mut self, index: &str, prefix: impl IntoIterator<Item = E>, query: impl Into<Expr>) -> Query {
        self.access = Access::Search { index: index.to_string(), prefix: exprs(prefix), query: query.into(), match_prefix: false };
        self
    }

    /// In a search: each word of the query also matches longer words that
    /// start with it.
    pub fn match_prefix(mut self) -> Query {
        if let Access::Search { match_prefix, .. } = &mut self.access {
            *match_prefix = true;
        }
        self
    }

    pub fn join(mut self, query: Query) -> Query {
        self.joins.push(Join { kind: JoinKind::Inner, query });
        self
    }

    pub fn left_join(mut self, query: Query) -> Query {
        self.joins.push(Join { kind: JoinKind::Left, query });
        self
    }

    /// Sees each row once per distinct item of a list computed from it,
    /// the item readable as the column `name`: SQL's `unnest`. The filter
    /// and everything after it apply to the repeated rows.
    pub fn each(mut self, name: &str, list: impl Into<Expr>) -> Query {
        self.unnest = Some(Select { name: name.to_string(), expr: list.into() });
        self
    }

    /// Keeps rows for which the condition is true. Called again, both
    /// conditions must hold.
    pub fn filter(mut self, condition: Expr) -> Query {
        self.filter = Some(match self.filter.take() {
            Some(existing) => existing.and(condition),
            None => condition,
        });
        self
    }

    pub fn group_by(mut self, keys: impl IntoIterator<Item = Expr>) -> Query {
        self.group_by.extend(keys);
        self
    }

    pub fn having(mut self, condition: Expr) -> Query {
        self.having = Some(condition);
        self
    }

    pub fn order_by(mut self, order: impl Into<Order>) -> Query {
        self.order_by.push(order.into());
        self
    }

    pub fn offset(mut self, n: u64) -> Query {
        self.offset = n;
        self
    }

    pub fn limit(mut self, n: u64) -> Query {
        self.limit = Some(n);
        self
    }

    /// Returns these columns of the table instead of all of them.
    pub fn columns<S: AsRef<str>>(mut self, names: impl IntoIterator<Item = S>) -> Query {
        for name in names {
            let name = name.as_ref();
            let short = name.rsplit('.').next().unwrap_or(name);
            self.select.push(Select { name: short.to_string(), expr: crate::expr::col(name) });
        }
        self
    }

    /// Adds a named expression to what the query returns.
    pub fn select(mut self, name: &str, expr: impl Into<Expr>) -> Query {
        self.select.push(Select { name: name.to_string(), expr: expr.into() });
        self
    }

    pub fn distinct(mut self) -> Query {
        self.distinct = true;
        self
    }

    pub fn with_total(mut self) -> Query {
        self.with_total = true;
        self
    }

    /// Whether the query groups its rows.
    pub fn is_aggregate(&self) -> bool {
        !self.group_by.is_empty() || self.select.iter().any(|s| s.expr.has_aggregate()) || self.having.is_some()
    }
}

// ----------------------------------------------------------------- writes

#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    pub table: String,
    /// The columns the rows give values for. The rest take their defaults.
    pub columns: Vec<String>,
    pub source: InsertSource,
    pub on_conflict: OnConflict,
    /// What to return for each row inserted or updated. Empty for nothing
    /// but the count.
    pub returning: Vec<Select>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    /// One row per row of the query, its columns in order.
    Query(Box<Query>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum OnConflict {
    /// A row that breaks a unique index fails the statement.
    Error,
    /// Such a row is skipped: when it conflicts on the named index, or on
    /// any unique index when none is named.
    DoNothing { target: Option<String> },
    /// The existing row is updated instead, if `filter` holds for it.
    /// `set` may read the existing row's columns and, through
    /// `excluded(..)`, the row that was proposed.
    DoUpdate { target: String, set: Vec<Assign>, filter: Option<Expr> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Assign {
    pub column: String,
    pub value: Expr,
}

impl Insert {
    pub fn into(table: &str) -> Insert {
        Insert {
            table: table.to_string(),
            columns: Vec::new(),
            source: InsertSource::Values(Vec::new()),
            on_conflict: OnConflict::Error,
            returning: Vec::new(),
        }
    }

    pub fn columns<S: AsRef<str>>(mut self, names: impl IntoIterator<Item = S>) -> Insert {
        self.columns = names.into_iter().map(|n| n.as_ref().to_string()).collect();
        self
    }

    /// Adds a row, its values in the order of `columns`.
    pub fn values<E: Into<Expr>>(mut self, row: impl IntoIterator<Item = E>) -> Insert {
        match &mut self.source {
            InsertSource::Values(rows) => rows.push(exprs(row)),
            InsertSource::Query(_) => self.source = InsertSource::Values(vec![exprs(row)]),
        }
        self
    }

    /// Sets one column of a single-row insert: `Insert::into("t").set("a", 1)`.
    pub fn set(mut self, column: &str, value: impl Into<Expr>) -> Insert {
        self.columns.push(column.to_string());
        match &mut self.source {
            InsertSource::Values(rows) => {
                if rows.is_empty() {
                    rows.push(Vec::new());
                }
                rows[0].push(value.into());
            }
            InsertSource::Query(_) => self.source = InsertSource::Values(vec![vec![value.into()]]),
        }
        self
    }

    pub fn from_query(mut self, query: Query) -> Insert {
        self.source = InsertSource::Query(Box::new(query));
        self
    }

    pub fn on_conflict_do_nothing(mut self) -> Insert {
        self.on_conflict = OnConflict::DoNothing { target: None };
        self
    }

    pub fn on_conflict_do_nothing_on(mut self, index: &str) -> Insert {
        self.on_conflict = OnConflict::DoNothing { target: Some(index.to_string()) };
        self
    }

    /// On a conflict with the named unique index (`schema::PRIMARY` for
    /// the primary key), update the existing row instead.
    pub fn on_conflict_update(mut self, index: &str) -> Insert {
        self.on_conflict = OnConflict::DoUpdate { target: index.to_string(), set: Vec::new(), filter: None };
        self
    }

    /// In the conflict update: assign a column.
    pub fn update(mut self, column: &str, value: impl Into<Expr>) -> Insert {
        if let OnConflict::DoUpdate { set, .. } = &mut self.on_conflict {
            set.push(Assign { column: column.to_string(), value: value.into() });
        }
        self
    }

    /// In the conflict update: take the proposed values for these columns.
    pub fn update_excluded<S: AsRef<str>>(mut self, columns: impl IntoIterator<Item = S>) -> Insert {
        if let OnConflict::DoUpdate { set, .. } = &mut self.on_conflict {
            for c in columns {
                set.push(Assign { column: c.as_ref().to_string(), value: Expr::Excluded(c.as_ref().to_string()) });
            }
        }
        self
    }

    /// In the conflict update: only update when this holds for the
    /// existing row; otherwise leave it and report nothing for the row.
    pub fn update_where(mut self, condition: Expr) -> Insert {
        if let OnConflict::DoUpdate { filter, .. } = &mut self.on_conflict {
            *filter = Some(condition);
        }
        self
    }

    pub fn returning<S: AsRef<str>>(mut self, columns: impl IntoIterator<Item = S>) -> Insert {
        for name in columns {
            self.returning.push(Select { name: name.as_ref().to_string(), expr: crate::expr::col(name.as_ref()) });
        }
        self
    }

    pub fn returning_expr(mut self, name: &str, expr: impl Into<Expr>) -> Insert {
        self.returning.push(Select { name: name.to_string(), expr: expr.into() });
        self
    }
}

/// Changes the rows a query finds. The query's table is the one updated;
/// its joins only help choose rows and supply values. With an order and a
/// limit, the first few rows in that order.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub rows: Query,
    pub set: Vec<Assign>,
    /// What to return for each row changed: its new columns, `old(..)` for
    /// the ones before, and joined tables' columns.
    pub returning: Vec<Select>,
}

impl Update {
    pub fn rows(rows: Query) -> Update {
        Update { rows, set: Vec::new(), returning: Vec::new() }
    }

    /// Every row of the table, until narrowed with `key`, `index` or
    /// `filter`.
    pub fn table(name: &str) -> Update {
        Update::rows(Query::table(name))
    }

    pub fn key<E: Into<Expr>>(mut self, key: impl IntoIterator<Item = E>) -> Update {
        self.rows = self.rows.key(key);
        self
    }

    pub fn index(mut self, index: &str, range: KeyRange) -> Update {
        self.rows = self.rows.index(index, range);
        self
    }

    pub fn filter(mut self, condition: Expr) -> Update {
        self.rows = self.rows.filter(condition);
        self
    }

    pub fn set(mut self, column: &str, value: impl Into<Expr>) -> Update {
        self.set.push(Assign { column: column.to_string(), value: value.into() });
        self
    }

    pub fn returning<S: AsRef<str>>(mut self, columns: impl IntoIterator<Item = S>) -> Update {
        for name in columns {
            let name = name.as_ref();
            let short = name.rsplit('.').next().unwrap_or(name);
            self.returning.push(Select { name: short.to_string(), expr: crate::expr::col(name) });
        }
        self
    }

    pub fn returning_expr(mut self, name: &str, expr: impl Into<Expr>) -> Update {
        self.returning.push(Select { name: name.to_string(), expr: expr.into() });
        self
    }
}

/// Removes the rows a query finds from the query's table.
#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    pub rows: Query,
    /// What to return for each row removed, read before it goes.
    pub returning: Vec<Select>,
}

impl Delete {
    pub fn rows(rows: Query) -> Delete {
        Delete { rows, returning: Vec::new() }
    }

    pub fn from(table: &str) -> Delete {
        Delete::rows(Query::table(table))
    }

    pub fn key<E: Into<Expr>>(mut self, key: impl IntoIterator<Item = E>) -> Delete {
        self.rows = self.rows.key(key);
        self
    }

    pub fn index(mut self, index: &str, range: KeyRange) -> Delete {
        self.rows = self.rows.index(index, range);
        self
    }

    pub fn filter(mut self, condition: Expr) -> Delete {
        self.rows = self.rows.filter(condition);
        self
    }

    pub fn returning<S: AsRef<str>>(mut self, columns: impl IntoIterator<Item = S>) -> Delete {
        for name in columns {
            self.returning.push(Select { name: name.as_ref().to_string(), expr: crate::expr::col(name.as_ref()) });
        }
        self
    }

    pub fn returning_expr(mut self, name: &str, expr: impl Into<Expr>) -> Delete {
        self.returning.push(Select { name: name.to_string(), expr: expr.into() });
        self
    }
}

/// Words a full-text search would look for in `text`, as the server cuts
/// them. See `schema::TextConfig`.
pub fn search_config_name(config: TextConfig) -> &'static str {
    config.name()
}
