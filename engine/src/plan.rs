//! Choosing a way in for a query that names none.
//!
//! A query that names its way in is run as written. One that names none
//! would read its whole table and filter; here a range of the primary
//! key or of an index is read off its filter instead, where that is sure
//! to answer the same: the same rows in the same order, or the same
//! error (R-Q23). `docs/DESIGN.md`, "Choosing a way in", gives the rules
//! and why each is safe. In short:
//!
//! - The filter stays as it is and is applied to every row read. A
//!   choice only decides which rows are read, so all it must show is
//!   that it leaves out no row the filter keeps.
//! - A way in is chosen only for a query whose filter and joins cannot
//!   fail on any row (`Env::shape`), since the whole table read would
//!   have evaluated them on the rows a range leaves out.
//! - A bound is used only if its bytes are the bytes a stored key would
//!   have: a value of the key part's own kind.
//!
//! Nothing is kept between statements. A statement is looked at each
//! time it runs, with its parameters' values and the tables as they are.

use std::borrow::Cow;

use database_protocol::expr::{CmpOp, Expr, Func};
use database_protocol::query::{Access, Bound, Join, Query};
use database_protocol::schema::IndexKind;
use database_protocol::value::{Type, Value};

use crate::catalog::Table;
use crate::eval::{self, eval, Ctx, Scope};
use crate::exec::index_parts;
use crate::keys;

/// The tree a chosen range is read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tree {
    Primary,
    /// An ordered index, by its place in the table's definition.
    Index(usize),
}

/// A run of a tree's keys: the first to read, and the first not to
/// (`None` for the tree's end).
pub type Span = (Vec<u8>, Option<Vec<u8>>);

/// What was chosen for a query.
#[derive(Debug, PartialEq)]
pub enum Choice {
    /// These runs of keys hold every row the filter keeps. They are in
    /// key order and apart. None at all: the filter keeps no row.
    Range { tree: Tree, spans: Vec<Span> },
    /// This run of keys, read in the tree's own order (or backwards),
    /// gives the rows the filter keeps in the query's `order_by`.
    Ordered { tree: Tree, span: Span, reverse: bool },
}

/// The most runs of keys one query is given: `in` lists over several
/// key parts multiply.
const MAX_SPANS: usize = 256;

/// What a value can be, as far as comparing and keys go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Bool,
    Int,
    Float,
    Decimal,
    Text,
    Bytes,
    Uuid,
    Timestamp,
    Date,
    Json,
    List,
}

impl Kind {
    fn of_type(ty: &Type) -> Kind {
        match ty {
            Type::Bool => Kind::Bool,
            Type::Int => Kind::Int,
            Type::Float => Kind::Float,
            Type::Decimal(_) => Kind::Decimal,
            Type::Text => Kind::Text,
            Type::Bytes => Kind::Bytes,
            Type::Uuid => Kind::Uuid,
            Type::Timestamp => Kind::Timestamp,
            Type::Date => Kind::Date,
            Type::Json => Kind::Json,
            Type::List(_) => Kind::List,
        }
    }

    /// `None` for NULL, which is of no kind.
    fn of_value(value: &Value) -> Option<Kind> {
        Some(match value {
            Value::Null => return None,
            Value::Bool(_) => Kind::Bool,
            Value::Int(_) => Kind::Int,
            Value::Float(_) => Kind::Float,
            Value::Decimal(_) => Kind::Decimal,
            Value::Text(_) => Kind::Text,
            Value::Bytes(_) => Kind::Bytes,
            Value::Uuid(_) => Kind::Uuid,
            Value::Timestamp(_) => Kind::Timestamp,
            Value::Date(_) => Kind::Date,
            Value::Json(_) => Kind::Json,
            Value::List(_) => Kind::List,
        })
    }

    /// Whether keys of this kind sort exactly as its values compare, so
    /// that a comparison is a range of keys. A float is not: a NaN with
    /// its sign bit set sorts before every number as a key and after
    /// every number as a value. JSON compares by structure and is keyed
    /// by its text; a list's items are compared in ways its key is not.
    fn keyed(self) -> bool {
        !matches!(self, Kind::Float | Kind::Json | Kind::List)
    }

    fn number(self) -> bool {
        matches!(self, Kind::Int | Kind::Float | Kind::Decimal)
    }

    /// A kind that a text is read as when compared with it
    /// (`eval::harmonize`).
    fn read_from_text(self) -> bool {
        matches!(self, Kind::Uuid | Kind::Timestamp | Kind::Date | Kind::Decimal)
    }

    /// Any value of the kind, to ask `eval::harmonize` how a text reads
    /// beside one.
    fn sample(self) -> Value {
        match self {
            Kind::Uuid => Value::Uuid([0; 16]),
            Kind::Timestamp => Value::Timestamp(0),
            Kind::Date => Value::Date(0),
            Kind::Decimal => Value::Decimal(database_protocol::decimal::Decimal::from(0i64)),
            _ => Value::Null,
        }
    }
}

/// What is known of an expression that cannot fail.
enum Shape {
    /// The same for every row of the query, and this.
    Value(Value),
    /// Read from the query's rows: of this kind, or NULL.
    Row(Kind),
}

impl Shape {
    fn kind(&self) -> Option<Kind> {
        match self {
            Shape::Value(value) => Kind::of_value(value),
            Shape::Row(kind) => Some(*kind),
        }
    }

    /// Whether it is a condition: true, false or NULL.
    fn condition(&self) -> bool {
        matches!(self.kind(), None | Some(Kind::Bool))
    }

    fn text(&self) -> bool {
        matches!(self.kind(), None | Some(Kind::Text))
    }
}

/// One of the conditions a filter is the `and` of, as far as a key can
/// use it.
enum Cond<'q> {
    /// `expr op value`, turned around if it was written the other way.
    /// The value is not NULL unless `op` is `IsNotDistinctFrom`.
    Cmp { op: CmpOp, expr: &'q Expr, value: Value },
    /// `expr in (values)`, NULLs left out: they equal nothing.
    In { expr: &'q Expr, values: Vec<Value> },
    /// `expr is null`.
    Null { expr: &'q Expr },
    /// Holds for no row: a comparison with NULL, a constant that is not
    /// true.
    Never,
    /// Anything else: applied to the rows read, and no help in choosing
    /// them.
    Other,
}

/// A filter taken apart at its top-level `and`s.
struct Conds<'q> {
    written: Vec<&'q Expr>,
    usable: Vec<Cond<'q>>,
}

/// The query's own tables, and what lies around it.
struct Env<'a> {
    ctx: &'a Ctx<'a>,
    /// What the query's expressions call each of its tables, and the
    /// table: its own first, then each join's. A name is looked for from
    /// the last to the first, as `eval::resolve` does.
    frames: Vec<(&'a str, &'a Table)>,
    /// The scope the query runs in: the enclosing query's row, for a
    /// join or a subquery.
    outer: &'a Scope<'a>,
}

impl<'a> Env<'a> {
    /// The frame and column a name means, if it is one of the query's
    /// own.
    fn own(&self, table: Option<&str>, name: &str) -> Option<(usize, usize)> {
        self.frames.iter().enumerate().rev().find_map(|(n, (alias, t))| {
            if table.is_some_and(|wanted| wanted != *alias) {
                return None;
            }
            t.by_name.get(name).map(|i| (n, *i))
        })
    }

    /// Whether an expression reads nothing of the query's rows and
    /// draws nothing, so that it is the same for every row and may be
    /// computed once.
    fn fixed(&self, expr: &Expr) -> bool {
        let mut fixed = true;
        expr.visit(&mut |e| match e {
            Expr::Column(table, name) => fixed &= self.own(table.as_deref(), name).is_none(),
            Expr::Subquery(..) | Expr::Agg { .. } | Expr::Excluded(_) | Expr::Old(_) | Expr::Depth | Expr::Rank | Expr::Call(Func::RandomUuid, _) => fixed = false,
            _ => {}
        });
        fixed
    }

    /// What an expression gives, if evaluating it cannot fail on any row
    /// the query's tables could hold; `None` if it might, or if this
    /// does not know. An expression that reads no row is computed, here
    /// and once: if that fails, so would the filter have.
    fn shape(&self, expr: &Expr) -> Option<Shape> {
        if self.fixed(expr) {
            return eval(self.ctx, self.outer, expr).ok().map(Shape::Value);
        }
        let condition = |e: &Expr| self.shape(e).filter(Shape::condition);
        let text = |e: &Expr| self.shape(e).filter(Shape::text);
        Some(match expr {
            Expr::Column(table, name) => {
                let (frame, i) = self.own(table.as_deref(), name)?;
                Shape::Row(Kind::of_type(&self.frames[frame].1.cols[i].def.ty))
            }
            Expr::Not(e) => {
                condition(e)?;
                Shape::Row(Kind::Bool)
            }
            Expr::And(parts) | Expr::Or(parts) => {
                for part in parts {
                    condition(part)?;
                }
                Shape::Row(Kind::Bool)
            }
            Expr::IsNull(e, _) => {
                self.shape(e)?;
                Shape::Row(Kind::Bool)
            }
            Expr::Cmp(_, a, b) => {
                if !compares(&self.shape(a)?, &self.shape(b)?) {
                    return None;
                }
                Shape::Row(Kind::Bool)
            }
            Expr::In(e, list, _) => {
                let value = self.shape(e)?;
                for item in self.items(list)? {
                    if !compares(&item, &value) {
                        return None;
                    }
                }
                Shape::Row(Kind::Bool)
            }
            Expr::Like { expr, pattern, .. } => {
                text(expr)?;
                text(pattern)?;
                Shape::Row(Kind::Bool)
            }
            Expr::Call(Func::Lower | Func::Upper | Func::Trim, args) if args.len() == 1 => {
                text(&args[0])?;
                Shape::Row(Kind::Text)
            }
            Expr::Call(Func::Length | Func::OctetLength, args) if args.len() == 1 => {
                text(&args[0])?;
                Shape::Row(Kind::Int)
            }
            // Any count of characters is taken, a negative one too.
            Expr::Call(Func::Left, args) if args.len() == 2 => {
                text(&args[0])?;
                self.shape(&args[1]).filter(|n| matches!(n.kind(), None | Some(Kind::Int)))?;
                Shape::Row(Kind::Text)
            }
            // A cast that gives NULL where it cannot convert.
            Expr::Cast(e, ty, true) => {
                self.shape(e)?;
                Shape::Row(Kind::of_type(ty))
            }
            _ => return None,
        })
    }

    /// The items of an `in`'s list, when it is a list of expressions or
    /// a value that is one. A list that is NULL has no items: the `in`
    /// is then NULL for every row, and fails for none.
    fn items(&self, list: &Expr) -> Option<Vec<Shape>> {
        match list {
            Expr::List(items) if !self.fixed(list) => items.iter().map(|item| self.shape(item)).collect(),
            _ => match self.shape(list)? {
                Shape::Value(Value::List(items)) => Some(items.into_iter().map(Shape::Value).collect()),
                Shape::Value(Value::Null) => Some(Vec::new()),
                _ => None,
            },
        }
    }

    /// A condition of the filter, as a key can use it; `None` if it
    /// could fail.
    fn cond<'q>(&self, expr: &'q Expr) -> Option<Cond<'q>> {
        let whole = self.shape(expr).filter(Shape::condition)?;
        if let Shape::Value(value) = whole {
            return Some(if value == Value::Bool(true) { Cond::Other } else { Cond::Never });
        }
        Some(match expr {
            Expr::Cmp(op, a, b) => {
                let (left, right, op) = match (self.shape(a)?, self.shape(b)?) {
                    (Shape::Row(_), Shape::Value(value)) => (&**a, value, *op),
                    (Shape::Value(value), Shape::Row(_)) => (&**b, value, turned(*op)),
                    _ => return Some(Cond::Other),
                };
                match op {
                    CmpOp::IsDistinctFrom => Cond::Other,
                    CmpOp::IsNotDistinctFrom if right.is_null() => Cond::Null { expr: left },
                    // A comparison with NULL is NULL, whatever the row.
                    _ if right.is_null() => Cond::Never,
                    _ => Cond::Cmp { op, expr: left, value: right },
                }
            }
            Expr::In(e, list, false) => {
                let Shape::Row(_) = self.shape(e)? else { return Some(Cond::Other) };
                // An item read from the row is no bound; an item that is
                // NULL equals nothing.
                let mut values = Vec::new();
                for item in self.items(list)? {
                    match item {
                        Shape::Value(Value::Null) => {}
                        Shape::Value(value) => values.push(value),
                        Shape::Row(_) => return Some(Cond::Other),
                    }
                }
                if values.is_empty() {
                    Cond::Never
                } else {
                    Cond::In { expr: e, values }
                }
            }
            Expr::IsNull(e, false) => match self.shape(e)? {
                Shape::Row(_) => Cond::Null { expr: e },
                Shape::Value(_) => Cond::Other,
            },
            _ => Cond::Other,
        })
    }

    /// The filter as the conditions that must all hold, each as it is
    /// written and as a key can use it, or `None` if any of it could
    /// fail on some row.
    fn conds<'q>(&self, filter: Option<&'q Expr>) -> Option<Conds<'q>> {
        fn split<'q>(expr: &'q Expr, out: &mut Vec<&'q Expr>) {
            match expr {
                Expr::And(parts) => parts.iter().for_each(|part| split(part, out)),
                other => out.push(other),
            }
        }
        let mut written = Vec::new();
        if let Some(filter) = filter {
            split(filter, &mut written);
        }
        let usable = written.iter().map(|part| self.cond(part)).collect::<Option<Vec<_>>>()?;
        Some(Conds { written, usable })
    }

    /// Whether a join cannot fail, whatever row it is run for: the
    /// whole table read runs it for rows a range leaves out. Only a
    /// plain lookup is known not to: a table that exists, no joins of
    /// its own, a filter that cannot fail, and a way in that is either
    /// not named or a range of a key whose bounds are of the key's
    /// kinds.
    fn join_cannot_fail(&self, join: &'a Join) -> Option<(&'a str, &'a Table)> {
        let inner = &join.query;
        if inner.table.is_empty() || !inner.joins.is_empty() || inner.unnest.is_some() || inner.is_aggregate() || !inner.order_by.is_empty() || inner.offset != 0 {
            return None;
        }
        let table: &'a Table = self.ctx.catalog.table(&inner.table).ok()?;
        let parts = match &inner.access {
            Access::Whole => None,
            Access::Primary { ranges, .. } => Some((Cow::Borrowed(crate::exec::pk_parts(table)), ranges)),
            Access::Index { index, ranges, .. } => {
                let i = table.index(index).ok()?;
                if !matches!(table.indexes[i].def.kind, IndexKind::Ordered) {
                    return None;
                }
                Some((Cow::Owned(index_parts(table, i)), ranges))
            }
            Access::Walk { .. } | Access::Search { .. } => return None,
        };
        // A bound is made into key bytes by converting each value to
        // its part's type. That cannot fail for NULL, for a value of
        // the part's own kind, or where the part has no type to convert
        // to (an expression's).
        if let Some((parts, ranges)) = parts {
            for bound in ranges.iter().flat_map(|range| [&range.lower, &range.upper]) {
                let (Bound::Included(exprs) | Bound::Excluded(exprs)) = bound else { continue };
                if exprs.len() > parts.len() {
                    return None;
                }
                for (expr, (ty, _)) in exprs.iter().zip(parts.iter()) {
                    let shape = self.shape(expr)?;
                    let Some(ty) = ty else { continue };
                    match shape.kind() {
                        None => {}
                        Some(kind) if kind == Kind::of_type(ty) && (kind.keyed() || kind == Kind::Float) => {}
                        Some(_) => return None,
                    }
                }
            }
        }
        let alias = inner.alias.as_deref().unwrap_or(&table.name);
        let mut frames = self.frames.clone();
        frames.push((alias, table));
        Env { ctx: self.ctx, frames, outer: self.outer }.conds(inner.filter.as_ref())?;
        Some((alias, table))
    }

    /// Whether the query writes this expression of its own table as the
    /// table's definition does (an index's key or condition, which names
    /// columns without their table).
    fn same_as_stored(&self, stored: &Expr, expr: &Expr) -> bool {
        // Every column the query's expression names must be the table's
        // own, not a joined table's of the same name.
        let mut own = true;
        let mut named = false;
        expr.visit(&mut |e| match e {
            Expr::Column(table, name) => {
                own &= matches!(self.own(table.as_deref(), name), Some((0, _)));
                named |= table.is_some();
            }
            Expr::Subquery(..) => own = false,
            _ => {}
        });
        if !own {
            return false;
        }
        if !named {
            return stored == expr;
        }
        let mut bare = expr.clone();
        unqualify(&mut bare);
        *stored == bare
    }
}

/// Takes the table's name off every column an expression names.
fn unqualify(expr: &mut Expr) {
    match expr {
        Expr::Column(table, _) => *table = None,
        Expr::Not(e) | Expr::IsNull(e, _) | Expr::Neg(e) | Expr::Cast(e, _, _) => unqualify(e),
        Expr::And(parts) | Expr::Or(parts) | Expr::List(parts) | Expr::Call(_, parts) => parts.iter_mut().for_each(unqualify),
        Expr::Cmp(_, a, b) | Expr::Arith(_, a, b) | Expr::In(a, b, _) => {
            unqualify(a);
            unqualify(b);
        }
        Expr::Like { expr, pattern, .. } => {
            unqualify(expr);
            unqualify(pattern);
        }
        Expr::Case(branches, otherwise) => {
            for (condition, value) in branches {
                unqualify(condition);
                unqualify(value);
            }
            if let Some(e) = otherwise {
                unqualify(e);
            }
        }
        Expr::Agg { arg, filter, .. } => {
            if let Some(e) = arg {
                unqualify(e);
            }
            if let Some(e) = filter {
                unqualify(e);
            }
        }
        Expr::Value(_) | Expr::Excluded(_) | Expr::Old(_) | Expr::Depth | Expr::Rank | Expr::Subquery(..) | Expr::Param(_) => {}
    }
}

/// Whether comparing two such values cannot fail (`eval::compare`).
fn compares(a: &Shape, b: &Shape) -> bool {
    let (Some(x), Some(y)) = (a.kind(), b.kind()) else { return true };
    // A text beside a uuid, a timestamp, a date or a decimal is read as
    // one, and fails if it does not read so. That is known only of a
    // text that is the same for every row.
    let reads = |text: &Shape, kind: Kind| matches!(text, Shape::Value(value) if eval::harmonize(value, &kind.sample()).is_ok());
    match (x, y) {
        (Kind::Text, kind) if kind.read_from_text() => reads(a, kind),
        (kind, Kind::Text) if kind.read_from_text() => reads(b, kind),
        (Kind::List, _) | (_, Kind::List) => false,
        (Kind::Date, Kind::Timestamp) | (Kind::Timestamp, Kind::Date) => true,
        _ => x == y || (x.number() && y.number()),
    }
}

/// The comparison that holds for `b, a` exactly when `op` does for
/// `a, b`.
fn turned(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        same => same,
    }
}

/// A value as a bound on a key part of `kind`: the value itself if it
/// is of that kind, a text read as it where a comparison would read it
/// so, and otherwise nothing. An integer beside a decimal column
/// compares, but its key bytes are not a decimal's, and it is not used.
fn as_kind(value: &Value, kind: Kind) -> Option<Value> {
    match Kind::of_value(value) {
        Some(own) if own == kind => Some(value.clone()),
        Some(Kind::Text) if kind.read_from_text() => eval::harmonize(value, &kind.sample()).ok().map(|(text, _)| text.into_owned()),
        _ => None,
    }
}

/// One part of a tree's key.
struct Part<'a> {
    of: PartOf<'a>,
    desc: bool,
    kind: Kind,
}

enum PartOf<'a> {
    /// A column of the table, by its place.
    Column(usize),
    /// An index's key expression.
    Expr(&'a Expr),
}

/// A tree a query could be read through, with the parts of its key.
struct Candidate<'a> {
    tree: Tree,
    parts: Vec<Part<'a>>,
    /// How many of the parts, from the first, make up a unique key:
    /// bound whole, each run of keys holds at most one row. 0 where the
    /// tree has none.
    unique: usize,
}

/// The parts of a tree's key that conditions could bind: the primary
/// key's columns, or an index's keys followed by the primary key, which
/// every entry ends with.
fn candidates<'a>(env: &Env<'a>, table: &'a Table, conds: &Conds) -> Vec<Candidate<'a>> {
    let column = |i: usize| Part { of: PartOf::Column(i), desc: false, kind: Kind::of_type(&table.cols[i].def.ty) };
    let mut out = vec![Candidate { tree: Tree::Primary, parts: table.pk.iter().map(|i| column(*i)).collect(), unique: table.pk.len() }];
    // An index's own expressions name the table's columns bare.
    let bare = Env { ctx: env.ctx, frames: vec![("", table)], outer: env.outer };
    'indexes: for (i, index) in table.indexes.iter().enumerate() {
        let def = &index.def;
        // An index over the items of a list holds a row once per item,
        // and a full-text index is not a tree of entries.
        if !matches!(def.kind, IndexKind::Ordered) || def.keys.iter().any(|key| key.each) {
            continue;
        }
        // A partial index holds only the rows its condition is true
        // for. It holds every row the filter keeps only if the filter
        // demands that very condition.
        if let Some(only) = &def.filter {
            let demanded = conds.written.iter().any(|expr| env.same_as_stored(only, expr));
            if !demanded {
                continue;
            }
        }
        let mut parts = Vec::with_capacity(def.keys.len() + table.pk.len());
        for key in &def.keys {
            let part = match &key.expr {
                Expr::Column(None, name) => match table.by_name.get(name) {
                    Some(c) => Part { desc: key.desc, ..column(*c) },
                    None => continue 'indexes,
                },
                expr => match bare.shape(expr) {
                    Some(Shape::Row(kind)) => Part { of: PartOf::Expr(expr), desc: key.desc, kind },
                    // Its kind is not known: nothing can be bound from
                    // here on, and the parts before it still can.
                    _ => break,
                },
            };
            parts.push(part);
        }
        let whole = parts.len() == def.keys.len();
        if whole {
            parts.extend(table.pk.iter().map(|c| column(*c)));
        }
        out.push(Candidate { tree: Tree::Index(i), parts, unique: if def.unique && whole { def.keys.len() } else { 0 } });
    }
    out
}

/// How far a filter's conditions bind a tree's key.
struct Fit {
    /// The key bytes of the parts bound by equality: one for each
    /// combination of values, where an `in` gave several.
    prefixes: Vec<Vec<u8>>,
    /// How many leading parts are bound by equality.
    eq: usize,
    /// The columns among them: every row read has one value there.
    eq_columns: Vec<usize>,
    /// A NULL among the values bound: a unique key does not hold NULLs
    /// apart.
    null: bool,
    /// On the part after those, as rising key bytes: the value its
    /// range starts at and whether that value is inside, and the same
    /// for where it ends.
    lower: Option<(Vec<u8>, bool)>,
    upper: Option<(Vec<u8>, bool)>,
}

impl Fit {
    fn bounds(&self) -> usize {
        usize::from(self.lower.is_some()) + usize::from(self.upper.is_some())
    }
}

fn fit(env: &Env, candidate: &Candidate, conds: &[Cond]) -> Fit {
    let is_part = |expr: &Expr, part: &Part| match (&part.of, expr) {
        (PartOf::Column(c), Expr::Column(table, name)) => env.own(table.as_deref(), name) == Some((0, *c)),
        (PartOf::Column(_), _) => false,
        (PartOf::Expr(stored), expr) => env.same_as_stored(stored, expr),
    };
    let mut fit = Fit { prefixes: vec![Vec::new()], eq: 0, eq_columns: Vec::new(), null: false, lower: None, upper: None };
    for part in &candidate.parts {
        if !part.kind.keyed() {
            return fit;
        }
        // The values this part must equal one of: the first condition
        // that says so in values of the part's kind.
        let values = conds.iter().find_map(|cond| match cond {
            Cond::Cmp { op: CmpOp::Eq | CmpOp::IsNotDistinctFrom, expr, value } if is_part(expr, part) => as_kind(value, part.kind).map(|v| vec![v]),
            Cond::Null { expr } if is_part(expr, part) => Some(vec![Value::Null]),
            Cond::In { expr, values } if is_part(expr, part) => values.iter().map(|v| as_kind(v, part.kind)).collect(),
            _ => None,
        });
        let Some(values) = values.filter(|values| fit.prefixes.len() * values.len() <= MAX_SPANS) else { break };
        fit.null |= values.iter().any(Value::is_null);
        let mut next = Vec::with_capacity(fit.prefixes.len() * values.len());
        for prefix in &fit.prefixes {
            for value in &values {
                let mut bytes = prefix.clone();
                keys::encode(&mut bytes, value, part.desc);
                next.push(bytes);
            }
        }
        fit.prefixes = next;
        fit.eq += 1;
        if let PartOf::Column(c) = part.of {
            fit.eq_columns.push(c);
        }
    }
    // The part after those bound by equality may be bound by a range,
    // from each side by the tightest condition there is.
    let Some(part) = candidate.parts.get(fit.eq).filter(|part| part.kind.keyed()) else { return fit };
    for cond in conds {
        let Cond::Cmp { op, expr, value } = cond else { continue };
        if !matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge) || !is_part(expr, part) {
            continue;
        }
        let Some(value) = as_kind(value, part.kind) else { continue };
        let mut bytes = Vec::new();
        keys::encode(&mut bytes, &value, false);
        let inside = matches!(op, CmpOp::Le | CmpOp::Ge);
        if matches!(op, CmpOp::Gt | CmpOp::Ge) {
            let tighter = fit.lower.as_ref().is_none_or(|(have, was_inside)| bytes > *have || (bytes == *have && *was_inside && !inside));
            if tighter {
                fit.lower = Some((bytes, inside));
            }
        } else {
            let tighter = fit.upper.as_ref().is_none_or(|(have, was_inside)| bytes < *have || (bytes == *have && *was_inside && !inside));
            if tighter {
                fit.upper = Some((bytes, inside));
            }
        }
    }
    fit
}

/// The key bytes of NULL, which sort after every value.
const NULL_KEY: u8 = 0xF0;

/// The run of keys under one prefix that a fit's range takes in. With
/// no range, every key that starts with the prefix. With one, the keys
/// whose next part lies in it, and never those where that part is NULL:
/// they sort after every value on a rising key and before every value
/// on a descending one, and no comparison holds for them.
fn span(prefix: &[u8], fit: &Fit, desc: bool) -> Option<Span> {
    let with = |bytes: &[u8]| {
        let mut key = prefix.to_vec();
        if desc {
            key.extend(bytes.iter().map(|b| !b));
        } else {
            key.extend_from_slice(bytes);
        }
        key
    };
    let (start, end) = if fit.bounds() == 0 {
        (prefix.to_vec(), keys::successor(prefix))
    } else {
        // Each end of the range in the order the keys are in: on a
        // descending key the larger value comes first.
        let (first, last) = if desc { (&fit.upper, &fit.lower) } else { (&fit.lower, &fit.upper) };
        let start = match first {
            Some((bytes, true)) => with(bytes),
            // After every key that holds this value.
            Some((bytes, false)) => keys::successor(&with(bytes))?,
            None if desc => keys::successor(&with(&[NULL_KEY]))?,
            None => prefix.to_vec(),
        };
        let end = match last {
            Some((bytes, true)) => keys::successor(&with(bytes)),
            Some((bytes, false)) => Some(with(bytes)),
            None if desc => keys::successor(prefix),
            None => Some(with(&[NULL_KEY])),
        };
        (start, end)
    };
    match &end {
        Some(end) if start >= *end => None,
        _ => Some((start, end)),
    }
}

/// The query's own table and its joins' as an `Env`, if the query is
/// one a way in may be chosen for: nothing in it but its filter decides
/// which rows are read, and its joins cannot fail.
fn env<'a>(ctx: &'a Ctx<'a>, q: &'a Query, table: &'a Table, alias: &'a str, outer: &'a Scope<'a>) -> Option<Env<'a>> {
    if q.unnest.is_some() {
        return None;
    }
    let mut env = Env { ctx, frames: vec![(alias, table)], outer };
    for join in &q.joins {
        let frame = env.join_cannot_fail(join)?;
        env.frames.push(frame);
    }
    Some(env)
}

/// Chooses a way in for a query that names none, or `None` to read the
/// whole table. `page` says the caller would rather have the rows in
/// the query's `order_by` than all of them: it is a sorted query with a
/// limit, and may be given `Choice::Ordered`.
pub fn choose<'a>(ctx: &'a Ctx<'a>, q: &'a Query, table: &'a Table, alias: &'a str, outer: &'a Scope<'a>, page: bool) -> Option<Choice> {
    if q.filter.is_none() && !page {
        return None;
    }
    let env = env(ctx, q, table, alias, outer)?;
    let conds = env.conds(q.filter.as_ref())?;
    if conds.usable.iter().any(|cond| matches!(cond, Cond::Never)) {
        return Some(Choice::Range { tree: Tree::Primary, spans: Vec::new() });
    }
    let candidates = candidates(&env, table, &conds);
    let fits: Vec<Fit> = candidates.iter().map(|candidate| fit(&env, candidate, &conds.usable)).collect();
    // Which of two trees that fit is read: decided by the filter and
    // the definition alone, so the same statement takes the same way.
    // A unique key bound whole; then more parts bound by equality; then
    // a range from more sides; then the primary key; then the index
    // defined first.
    let rank = |n: usize| {
        let (candidate, fit) = (&candidates[n], &fits[n]);
        let one_row = candidate.unique > 0 && fit.eq >= candidate.unique && !fit.null;
        (one_row, fit.eq, fit.bounds(), candidate.tree == Tree::Primary, std::cmp::Reverse(n))
    };
    let best = (0..candidates.len()).filter(|n| fits[*n].eq > 0 || fits[*n].bounds() > 0).max_by_key(|n| rank(*n));
    if page {
        // A narrower range is never given up for an order: only a tree
        // that binds as many parts by equality as any other is read in
        // its order.
        let most = fits.iter().map(|fit| fit.eq).max().unwrap_or(0);
        let ordered = (0..candidates.len())
            .filter(|n| fits[*n].eq == most)
            .filter_map(|n| in_order(&env, q, table, &candidates[n], &fits[n]).map(|(span, reverse)| (n, span, reverse)))
            .max_by_key(|(n, ..)| (fits[*n].bounds(), candidates[*n].tree == Tree::Primary, std::cmp::Reverse(*n)));
        if let Some((n, span, reverse)) = ordered {
            return Some(match span {
                Some(span) => Choice::Ordered { tree: candidates[n].tree, span, reverse },
                None => Choice::Range { tree: Tree::Primary, spans: Vec::new() },
            });
        }
    }
    let best = best?;
    let (candidate, fit) = (&candidates[best], &fits[best]);
    let desc = candidate.parts.get(fit.eq).is_some_and(|part| part.desc);
    let mut spans: Vec<Span> = fit.prefixes.iter().filter_map(|prefix| span(prefix, fit, desc)).collect();
    // In key order, each once: two values of an `in` may be one key.
    spans.sort();
    spans.dedup();
    Some(Choice::Range { tree: candidate.tree, spans })
}

/// Whether reading a tree's fitted run of keys in the tree's own order
/// gives the rows in the query's `order_by`, exactly as a stable sort
/// of the whole table read would: the run (`None` if it is empty), and
/// whether to read it backwards.
fn in_order(env: &Env, q: &Query, table: &Table, candidate: &Candidate, fit: &Fit) -> Option<(Option<Span>, bool)> {
    // The rows past the page are never selected, and the whole table
    // read selects every row before it sorts: what is selected must not
    // be able to fail either.
    for item in &q.select {
        env.shape(&item.expr)?;
    }
    if fit.prefixes.len() != 1 || q.order_by.is_empty() {
        return None;
    }
    let parts = candidate.parts.get(fit.eq..fit.eq + q.order_by.len())?;
    let mut columns = fit.eq_columns.clone();
    let (mut forwards, mut backwards) = (true, true);
    for (order, part) in q.order_by.iter().zip(parts) {
        let Expr::Column(named, name) = &order.expr else { return None };
        let PartOf::Column(c) = part.of else { return None };
        if env.own(named.as_deref(), name) != Some((0, c)) || !part.kind.keyed() {
            return None;
        }
        // A sort key that names a selected item sorts by that item,
        // which must then be the column itself.
        if named.is_none() {
            if let Some(item) = q.select.iter().find(|item| item.name == *name) {
                if !matches!(&item.expr, Expr::Column(t, n) if env.own(t.as_deref(), n) == Some((0, c))) {
                    return None;
                }
            }
        }
        // Read forwards, a key part gives its values rising with NULLs
        // last, or falling with NULLs first; read backwards, the other
        // way round each.
        let nulls_last = order.nulls_last.unwrap_or(!order.desc);
        let nullable = table.cols[c].def.nullable;
        forwards &= order.desc == part.desc && (!nullable || nulls_last != part.desc);
        backwards &= order.desc != part.desc && (!nullable || nulls_last == part.desc);
        columns.push(c);
    }
    // Rows that tie on the sort keys must come as the stable sort
    // leaves them, in primary key order. The entries tie on every
    // column in `columns`, so they come in the order of the key parts
    // after the sort keys, and that is primary key order if those
    // parts, the tied columns aside, lead with the primary key. Read
    // backwards the ties would come reversed, and there must be none:
    // the tied columns take in the whole primary key.
    let apart: Vec<usize> = table.pk.iter().copied().filter(|c| !columns.contains(c)).collect();
    let mut rest = candidate.parts[fit.eq + q.order_by.len()..].iter().filter(|part| !matches!(part.of, PartOf::Column(c) if columns.contains(&c)));
    let ties_in_key_order = apart.iter().all(|c| matches!(rest.next(), Some(Part { of: PartOf::Column(p), desc: false, .. }) if p == c));
    let reverse = if forwards && ties_in_key_order {
        false
    } else if backwards && apart.is_empty() {
        true
    } else {
        return None;
    };
    let desc = candidate.parts[fit.eq].desc;
    Some((span(&fit.prefixes[0], fit, desc), reverse))
}
