//! What the server chooses for a query that names no way in, against the
//! whole table (R-Q23, R-Q24, R-W14).
//!
//! The first test is differential. It makes tables with columns of every
//! type, keys and indexes of every kind and rows full of NULLs and
//! duplicates, then statements of every shape, and runs each twice: once
//! with the choice made, once reading the whole table. The two must
//! answer the same: the same rows in the same order, or the same error;
//! and for an update or a delete, the same rows returned and the same
//! tables left behind.
//!
//! The tests after it take the rules one at a time, on their edges, and
//! say which way in each statement must have been given.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use database_engine::db::Database;
use database_engine::error::Error;
use database_engine::eval::{Chosen, Read, Way};
use database_engine::vfs::{MemVfs, Rng};
use database_protocol::expr::*;
use database_protocol::message::{Ddl, RowSet};
use database_protocol::query::*;
use database_protocol::schema::*;
use database_protocol::value::{Type, Value};

// ------------------------------------------------------------ generating

const TEXTS: [&str; 9] = ["", "a", "A", "ab", "b", "a\0", "é", "zz", "inv"];

fn uuid(n: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0] = (n * 37) as u8;
    out[15] = n as u8;
    out
}

fn uuid_text(n: u64) -> String {
    database_protocol::value::format_uuid(&uuid(n))
}

/// A value of a type, from a small set so that rows and bounds often
/// meet, with now and then one from a wide set so that tables grow.
fn value(rng: &mut Rng, ty: &Type) -> Value {
    let pick = |rng: &mut Rng, n: usize| rng.below(n as u64) as usize;
    match ty {
        Type::Bool => Value::Bool(rng.chance(2)),
        Type::Int => Value::Int(match rng.below(40) {
            0 => i64::MAX,
            1 => i64::MIN,
            2..=9 => rng.below(600) as i64,
            _ => rng.below(9) as i64 - 3,
        }),
        Type::Float => Value::Float([-1.5, 0.0, -0.0, 1.0, 2.5, f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY][pick(rng, 9)]),
        Type::Decimal(None) => Value::Decimal(["0", "1", "1.0", "1.50", "-2.25", "10", "0.001", "123456789.5"][pick(rng, 8)].parse().unwrap()),
        Type::Decimal(Some(_)) => Value::Decimal(["0", "1", "1.5", "-2.25", "10.01", "7"][pick(rng, 6)].parse().unwrap()),
        Type::Text => Value::Text(if rng.chance(4) { format!("k{}", rng.below(400)) } else { TEXTS[pick(rng, TEXTS.len())].to_string() }),
        Type::Bytes => Value::Bytes(if rng.chance(4) { rng.below(500).to_be_bytes().to_vec() } else { [vec![], vec![0], vec![0, 255], vec![1], vec![255]][pick(rng, 5)].clone() }),
        Type::Uuid => {
            let of = if rng.chance(4) { 250 } else { 6 };
            Value::Uuid(uuid(rng.below(of)))
        }
        Type::Timestamp => Value::Timestamp(if rng.chance(4) { rng.below(500) as i64 } else { [0, 1, -5, 86_400_000_000, 1_700_000_000_000_000, i64::MAX][pick(rng, 6)] }),
        Type::Date => Value::Date([0, 1, -1, 19_700, 20_000][pick(rng, 5)]),
        Type::Json => Value::Json([serde_json::json!({"a": 1}), serde_json::json!([1, 2]), serde_json::json!("x"), serde_json::json!(7), serde_json::json!({"b": [true]})][pick(rng, 5)].clone()),
        Type::List(_) => Value::List((0..rng.below(4)).map(|_| Value::Text(["x", "y", "z"][pick(rng, 3)].to_string())).collect()),
    }
}

fn main_columns() -> Vec<(&'static str, Type)> {
    vec![
        ("i", Type::Int),
        ("j", Type::Int),
        ("f", Type::Float),
        ("d", Type::Decimal(None)),
        ("m", Type::decimal(12, 2)),
        ("s", Type::Text),
        ("x", Type::Text),
        ("b", Type::Bytes),
        ("u", Type::Uuid),
        ("ts", Type::Timestamp),
        ("day", Type::Date),
        ("ok", Type::Bool),
        ("doc", Type::Json),
        ("tags", Type::list(Type::Text)),
    ]
}

/// The conditions a partial index may carry, and a filter may repeat.
fn partial(n: u64) -> Expr {
    match n % 5 {
        0 => col("ok").eq(true),
        1 => col("j").gt(3),
        2 => col("s").is_not_null(),
        3 => col("i").lt(20),
        _ => col("x").eq("a"),
    }
}

/// What a generated database is made of.
struct Schema {
    columns: Vec<(&'static str, Type)>,
    /// The ordered indexes, by name: for a query that names one.
    ordered: Vec<String>,
    /// The primary key, then each index over plain columns: the columns
    /// of its key and whether each descends. Statements are aimed at
    /// these, so that there is often something to choose.
    trees: Vec<Vec<(&'static str, bool)>>,
}

fn pick<'a, T>(rng: &mut Rng, of: &'a [T]) -> &'a T {
    &of[rng.below(of.len() as u64) as usize]
}

fn make_schema(rng: &mut Rng, db: &Arc<Database>) -> Schema {
    let columns = main_columns();
    let keys: [&[&str]; 13] = [&["i"], &["s"], &["u"], &["i", "s"], &["s", "i"], &["day", "i"], &["b", "j"], &["ts", "j"], &["ok", "i"], &["m", "i"], &["d", "j"], &["j", "i"], &["i", "j", "s"]];
    let key = *pick(rng, &keys);
    let mut def = TableDef::new("t");
    for (name, ty) in &columns {
        let column = ColumnDef::new(name, ty.clone());
        def = def.column(if !key.contains(name) && !rng.chance(3) { column.nullable() } else { column });
    }
    def = def.primary_key(key.iter().copied());
    let mut ordered = Vec::new();
    let mut trees: Vec<Vec<(&'static str, bool)>> = vec![key.iter().map(|name| (*name, false)).collect()];
    for n in 0..2 + rng.below(5) {
        let name = format!("t_{n}");
        let mut index = IndexDef::new(&name);
        match rng.below(12) {
            0 => index = index.on(["j"]).on_each(col("tags")),
            1 => index = index.on(["j"]).full_text(TextConfig::Simple, [(col("s"), 1.0)]),
            2 => index = index.on_expr(col("s").lower()),
            3 => index = index.on(["j"]).on_expr(col("x").upper()),
            4 => index = index.on_expr(col("s").length()).on(["i"]),
            _ => {
                let mut used: Vec<&str> = Vec::new();
                for _ in 0..1 + rng.below(3) {
                    // Any column but the list: floats and JSON too, which
                    // an index may hold and a choice must leave alone.
                    let (column, _) = columns[rng.below(13) as usize];
                    if used.contains(&column) {
                        continue;
                    }
                    used.push(column);
                    index = if rng.chance(3) { index.on_desc(column) } else { index.on([column]) };
                }
                trees.push(index.keys.iter().zip(&used).map(|(key, name)| (*name, key.desc)).collect());
            }
        }
        let tree = matches!(index.kind, IndexKind::Ordered);
        if tree && !index.keys.iter().any(|key| key.each) && rng.chance(6) {
            index = index.unique();
        }
        if tree && rng.chance(4) {
            index = index.filter(partial(rng.next()));
        }
        if tree {
            ordered.push(name);
        }
        def = def.index(index);
    }
    let other = TableDef::new("u")
        .column(ColumnDef::new("id", Type::Int))
        .column(ColumnDef::new("i", Type::Int).nullable())
        .column(ColumnDef::new("s", Type::Text).nullable())
        .column(ColumnDef::new("ok", Type::Bool))
        .primary_key(["id"])
        .index(IndexDef::new("u_i").on(["i"]));
    let mut tx = db.begin_write(WAIT, false).unwrap();
    tx.ddl(&Ddl::CreateTable { def, if_not_exists: false }, None).unwrap();
    tx.ddl(&Ddl::CreateTable { def: other, if_not_exists: false }, None).unwrap();
    tx.commit().unwrap();
    Schema { columns, ordered, trees }
}

fn fill(rng: &mut Rng, db: &Arc<Database>, schema: &Schema, rows: u64) {
    let names: Vec<&str> = schema.columns.iter().map(|(name, _)| *name).collect();
    let described = db.begin_read().describe();
    let nullable: Vec<bool> = described.iter().find(|t| t.name == "t").unwrap().columns.iter().map(|c| c.nullable).collect();
    let mut tx = db.begin_write(WAIT, false).unwrap();
    let mut left = rows;
    while left > 0 {
        // Rows that repeat a key, or a unique index's, are passed over.
        let mut insert = Insert::into("t").columns(names.iter().copied()).on_conflict_do_nothing();
        for _ in 0..left.min(50) {
            let row: Vec<Value> = schema.columns.iter().zip(&nullable).map(|((_, ty), nullable)| if *nullable && rng.chance(5) { Value::Null } else { value(rng, ty) }).collect();
            insert = insert.values(row);
        }
        tx.insert(&insert, None).unwrap();
        left -= left.min(50);
    }
    let mut others = Insert::into("u").columns(["id", "i", "s", "ok"]);
    for id in 0..rng.below(40) as i64 {
        let i = if rng.chance(6) { Value::Null } else { value(rng, &Type::Int) };
        let s = if rng.chance(4) { Value::Null } else { value(rng, &Type::Text) };
        others = others.values([Value::Int(id), i, s, Value::Bool(rng.chance(2))]);
    }
    if let InsertSource::Values(rows) = &others.source {
        if !rows.is_empty() {
            tx.insert(&others, None).unwrap();
        }
    }
    tx.commit().unwrap();
}

/// What a statement is generated from: the columns, what the statement
/// calls its table, and the values its parameters take.
struct Gen<'a> {
    schema: &'a Schema,
    /// What the query's expressions call `t`.
    alias: &'static str,
    params: Vec<Value>,
}

impl Gen<'_> {
    /// A column of `t`, sometimes by its table's name too.
    fn column(&self, rng: &mut Rng, name: &str) -> Expr {
        if rng.chance(3) {
            col(&format!("{}.{name}", self.alias))
        } else {
            col(name)
        }
    }

    /// A value as a literal or, now and then, as a parameter.
    fn constant(&mut self, rng: &mut Rng, value: Value) -> Expr {
        if rng.chance(5) {
            self.params.push(value);
            param(self.params.len() as u32 - 1)
        } else {
            val(value)
        }
    }

    fn any_column(&self, rng: &mut Rng) -> (&'static str, Type) {
        pick(rng, &self.schema.columns).clone()
    }

    fn compare(&self, rng: &mut Rng, a: Expr, b: Expr) -> Expr {
        match rng.below(16) {
            0..=5 => a.eq(b),
            6 => a.ne(b),
            7 | 8 => a.lt(b),
            9 => a.le(b),
            10 | 11 => a.gt(b),
            12 => a.ge(b),
            13 => a.is_distinct_from(b),
            _ => a.is_not_distinct_from(b),
        }
    }

    /// A text that reads as a value of the type, or does not.
    fn text_for(&self, rng: &mut Rng, ty: &Type) -> Value {
        Value::Text(match (ty, rng.chance(5)) {
            (_, true) => "nope".to_string(),
            (Type::Uuid, _) => uuid_text(rng.below(6)),
            (Type::Timestamp, _) => ["1970-01-01T00:00:00Z", "1970-01-02T00:00:00Z", "2023-11-14T22:13:20Z"][rng.below(3) as usize].to_string(),
            (Type::Date, _) => ["1970-01-01", "1970-01-02", "2023-12-08"][rng.below(3) as usize].to_string(),
            _ => ["1", "1.5", "-2.25", " 10 "][rng.below(4) as usize].to_string(),
        })
    }

    /// One condition.
    fn atom(&mut self, rng: &mut Rng) -> Expr {
        let (name, ty) = self.any_column(rng);
        let column = self.column(rng, name);
        match rng.below(80) {
            // A column beside a value of its own kind, either way round.
            0..=39 => {
                let value = value(rng, &ty);
                let value = self.constant(rng, value);
                if rng.chance(5) {
                    self.compare(rng, value, column)
                } else {
                    self.compare(rng, column, value)
                }
            }
            // Beside NULL.
            40 | 41 => {
                let null = self.constant(rng, Value::Null);
                self.compare(rng, column, null)
            }
            // A joined table's column, where there may be no join.
            42 if rng.chance(3) => col("u.ok").eq(true).or(col("u.ok").is_null()),
            // Beside a value of another kind: some compare, some fail.
            42 | 43 => {
                let (_, other) = self.any_column(rng);
                let value = value(rng, &other);
                let value = self.constant(rng, value);
                self.compare(rng, column, value)
            }
            // Beside a text that may read as its kind.
            44..=46 => {
                let text = self.text_for(rng, &ty);
                let text = self.constant(rng, text);
                self.compare(rng, column, text)
            }
            47..=54 => {
                let mut items: Vec<Value> = (0..rng.below(5)).map(|_| value(rng, &ty)).collect();
                if rng.chance(5) {
                    items.push(Value::Null);
                }
                if rng.chance(5) && !items.is_empty() {
                    items.push(items[0].clone());
                }
                if rng.chance(8) {
                    items.push(self.text_for(rng, &ty));
                }
                let list = match rng.below(3) {
                    0 => self.constant(rng, Value::List(items)),
                    _ => list(items.into_iter().map(|item| self.constant(rng, item)).collect::<Vec<_>>()),
                };
                if rng.chance(5) {
                    column.not_in_list(list)
                } else {
                    column.in_list(list)
                }
            }
            55 => column.is_null(),
            56 => column.is_not_null(),
            // What an expression index is written for.
            57..=60 => {
                let text = Value::Text(TEXTS[rng.below(TEXTS.len() as u64) as usize].to_string());
                let text = self.constant(rng, text);
                match rng.below(5) {
                    0 | 1 => self.column(rng, "s").lower().eq(text),
                    2 => self.column(rng, "x").upper().eq(text),
                    3 => {
                        let n = Value::Int(rng.below(4) as i64);
                        let n = self.constant(rng, n);
                        let length = self.column(rng, "s").length();
                        self.compare(rng, length, n)
                    }
                    _ => self.column(rng, "s").trim().ge(text),
                }
            }
            61 | 62 => self.column(rng, "s").like(["a%", "%", "_b", "k1%"][rng.below(4) as usize]),
            // What can fail on a row: arithmetic, a strict cast.
            63 | 64 => match rng.below(4) {
                0 => self.column(rng, "i").add(1).gt(2),
                1 => self.column(rng, "i").div(self.column(rng, "j")).eq(1),
                2 => self.column(rng, "s").cast(Type::Int).eq(1),
                _ => self.column(rng, "j").mul(4_000_000_000_000_000_000i64).lt(5),
            },
            // Two columns.
            65 | 66 => match rng.below(3) {
                0 => self.column(rng, "i").eq(self.column(rng, "j")),
                1 => self.column(rng, "s").lt(self.column(rng, "x")),
                _ => self.column(rng, "ts").ge(self.column(rng, "day")),
            },
            // The clock, and what is computed from it once.
            67 | 68 => match rng.below(3) {
                0 => self.column(rng, "ts").lt(now()),
                1 => self.column(rng, "ts").gt(now().sub(days(20_000))),
                _ => self.column(rng, "day").lt(now()),
            },
            // What is the same for every row: computed once, or failing.
            69 | 70 => match rng.below(4) {
                0 => self.column(rng, "i").gt(val(2).add(1)),
                1 => self.column(rng, "i").eq(val(1).div(0)),
                2 => self.column(rng, "j").le(val(i64::MAX).add(1)),
                _ => val(1).lt(2),
            },
            71 => self.column(rng, "s").try_cast(Type::Int).eq(1),
            72 | 73 => {
                let ok = self.column(rng, "ok");
                if rng.chance(2) {
                    ok
                } else {
                    ok.not()
                }
            }
            // A subquery that reads the row.
            74 | 75 => match rng.below(3) {
                0 => exists(Query::table("u").filter(col("u.i").eq(col(&format!("{}.i", self.alias))))),
                1 => self.column(rng, "i").in_list(list_of(Query::table("u").columns(["i"]))),
                _ => exists(Query::table("u").filter(col("u.id").eq(col(&format!("{}.j", self.alias))).and(col("u.ok")))),
            },
            // A partial index's own condition, as the index has it or by
            // the table's name.
            76..=78 => {
                let condition = partial(rng.next());
                if self.alias == "t" && rng.chance(3) {
                    qualified(condition, "t")
                } else {
                    condition
                }
            }
            _ => {
                let (low, high) = (value(rng, &ty), value(rng, &ty));
                column.between(self.constant(rng, low), self.constant(rng, high))
            }
        }
    }

    fn type_of(&self, name: &str) -> Type {
        self.schema.columns.iter().find(|(column, _)| *column == name).unwrap().1.clone()
    }

    /// Conditions that bind the leading parts of a tree's key: equality
    /// on some, then perhaps a range on the next.
    fn aimed(&mut self, rng: &mut Rng, tree: &[(&'static str, bool)], bound: usize) -> Vec<Expr> {
        let mut out = Vec::new();
        for (name, _) in &tree[..bound] {
            let ty = self.type_of(name);
            let column = self.column(rng, name);
            out.push(match rng.below(10) {
                0 => column.is_null(),
                1 | 2 => {
                    let items: Vec<Expr> = (0..1 + rng.below(3)).map(|_| val(value(rng, &ty))).collect();
                    column.in_list(list(items))
                }
                _ => {
                    let value = value(rng, &ty);
                    column.eq(self.constant(rng, value))
                }
            });
        }
        if let Some((name, _)) = tree.get(bound).filter(|_| rng.chance(2)) {
            let ty = self.type_of(name);
            for _ in 0..1 + rng.below(2) {
                let (column, value) = (self.column(rng, name), value(rng, &ty));
                let value = self.constant(rng, value);
                out.push(match rng.below(4) {
                    0 => column.lt(value),
                    1 => column.le(value),
                    2 => column.gt(value),
                    _ => column.ge(value),
                });
            }
        }
        out
    }

    fn filter(&mut self, rng: &mut Rng) -> Expr {
        match rng.below(20) {
            0..=10 => all((0..1 + rng.below(4)).map(|_| self.atom(rng)).collect::<Vec<_>>()),
            11..=13 => self.atom(rng),
            14 | 15 => self.atom(rng).or(self.atom(rng)),
            16 | 17 => self.atom(rng).and(self.atom(rng).or(self.atom(rng))),
            18 => self.atom(rng).and(self.atom(rng)).not(),
            _ => self.atom(rng).and(self.atom(rng)).and(self.atom(rng).and(self.atom(rng))),
        }
    }

    /// A join with `u`: a lookup the server may choose for, one that
    /// names its way in, and ones that fail.
    fn join(&mut self, rng: &mut Rng, q: Query) -> Query {
        let t = |name: &str| col(&format!("{}.{name}", self.alias));
        let mut inner = Query::table("u");
        inner = match rng.below(9) {
            0 | 1 => inner.filter(col("u.i").eq(t("i"))),
            2 => inner.filter(col("u.id").eq(t("j")).and(col("u.ok"))),
            3 => inner.key([t("j")]),
            4 => inner.index("u_i", KeyRange::eq([t("i")])),
            // A text beside an integer, in a filter and in a bound.
            5 => inner.filter(col("u.s").eq(t("i"))),
            6 => inner.key([t("s")]),
            7 => inner.filter(col("u.i").add(1).eq(t("i"))),
            _ => inner.filter(col("u.s").eq(t("s"))),
        };
        if rng.chance(4) {
            inner = inner.limit(1);
        }
        if rng.chance(3) {
            q.left_join(inner)
        } else {
            q.join(inner)
        }
    }

    /// The table and how its rows are found: what a query, an update and
    /// a delete all start from.
    fn rows(&mut self, rng: &mut Rng) -> Query {
        let mut q = Query::table("t");
        if self.alias != "t" {
            q = q.alias(self.alias);
        }
        // Now and then a way in is named: both runs then read the same.
        if rng.chance(10) {
            q = match rng.below(4) {
                0 => q.whole(),
                1 => q.reverse(),
                2 if !self.schema.ordered.is_empty() => q.index(pick(rng, &self.schema.ordered), KeyRange::all()),
                _ => q.range(KeyRange::all().nulls()),
            };
        }
        if rng.chance(5) {
            q = self.join(rng, q);
        }
        match rng.below(10) {
            0 => {}
            // Aimed at a key, with or without more.
            1..=4 => {
                let schema = self.schema;
                let tree = pick(rng, &schema.trees);
                let bound = 1 + rng.below(tree.len() as u64) as usize;
                let mut conditions = self.aimed(rng, tree, bound);
                for _ in 0..rng.below(3) {
                    conditions.push(self.atom(rng));
                }
                // In any order: which condition comes first decides
                // nothing.
                for n in (1..conditions.len()).rev() {
                    conditions.swap(n, rng.below(n as u64 + 1) as usize);
                }
                q = q.filter(all(conditions));
            }
            _ => q = q.filter(self.filter(rng)),
        }
        q
    }

    /// A page in the order of a tree's key: some leading parts bound by
    /// equality, the next ones sorted by, each as the key has it or all
    /// the other way round, and now and then not quite either.
    fn page(&mut self, rng: &mut Rng) -> Query {
        let schema = self.schema;
        let tree = pick(rng, &schema.trees);
        let bound = rng.below(tree.len() as u64) as usize;
        let mut q = Query::table("t");
        if self.alias != "t" {
            q = q.alias(self.alias);
        }
        let mut conditions = self.aimed(rng, tree, bound);
        for _ in 0..rng.below(4).saturating_sub(1) {
            conditions.push(self.atom(rng));
        }
        if !conditions.is_empty() {
            q = q.filter(all(conditions));
        }
        let backwards = rng.chance(3);
        let sorted = 1 + rng.below((tree.len() - bound) as u64) as usize;
        for (name, desc) in &tree[bound..bound + sorted] {
            let column = self.column(rng, name);
            let mut order = if *desc != backwards { column.desc() } else { column.asc() };
            if rng.chance(12) {
                order = if rng.chance(2) { order.nulls_first() } else { order.nulls_last() };
            }
            if rng.chance(15) {
                order.desc = !order.desc;
            }
            q = q.order_by(order);
        }
        q
    }

    fn order(&self, rng: &mut Rng, mut q: Query) -> Query {
        for _ in 0..1 + rng.below(3) {
            let (name, _) = self.schema.columns[rng.below(13) as usize];
            let column = self.column(rng, name);
            let mut order = if rng.chance(2) { column.desc() } else { column.asc() };
            if rng.chance(6) {
                order = if rng.chance(2) { order.nulls_first() } else { order.nulls_last() };
            }
            q = q.order_by(order);
        }
        q
    }

    fn query(&mut self, rng: &mut Rng) -> Query {
        let mut q = self.rows(rng);
        match rng.below(10) {
            // A sorted page: where an index's own order is read.
            0..=3 => {
                q = if rng.chance(4) { self.order(rng, q) } else { self.page(rng) }.limit(rng.below(12));
                if rng.chance(4) {
                    q = q.offset(rng.below(5));
                }
                q = match rng.below(8) {
                    0 => q.columns(["i", "s", "ts"]),
                    1 => q.select("n", col("i").add(1)),
                    // A name that is a column's, for something else.
                    2 => q.select("i", col("j")).select("s", col("s")),
                    3 => q.select("i", col("i")).select("lower", col("s").lower()).select("start", col("x").left(rng.below(4) as i64 - 1)),
                    4 => q.with_total(),
                    5 => q.distinct().columns(["j", "ok"]),
                    _ => q,
                };
            }
            // Rows grouped.
            4 | 5 => {
                q = q.select("n", count()).select("total", sum(col("j"))).select("all", Expr::Agg { func: AggFunc::Collect, arg: Some(Box::new(col("s"))), filter: None, distinct: false }).select("first", col("f"));
                if rng.chance(2) {
                    q = q.group_by([col("ok")]).select("ok", col("ok"));
                }
            }
            // Rows as they come, some of them.
            _ => {
                if rng.chance(25) {
                    q = q.each("tag", col("tags"));
                }
                if rng.chance(4) {
                    q = self.order(rng, q);
                }
                if rng.chance(3) {
                    q = q.limit(rng.below(20));
                }
                if rng.chance(6) {
                    q = q.offset(rng.below(4));
                }
                if rng.chance(8) {
                    q = q.with_total();
                }
                q = match rng.below(8) {
                    0 => q.columns(["i", "j", "s"]),
                    1 => q.select("n", col("i").add(col("j"))),
                    2 => q.distinct().columns(["ok", "x"]),
                    _ => q,
                };
            }
        }
        q
    }
}

/// An expression with every bare column given a table's name.
fn qualified(expr: Expr, table: &str) -> Expr {
    let name = |column: String| Expr::Column(Some(table.to_string()), column);
    match expr {
        Expr::Column(None, column) => name(column),
        Expr::Cmp(op, a, b) => Expr::Cmp(op, Box::new(qualified(*a, table)), Box::new(qualified(*b, table))),
        Expr::IsNull(e, negated) => Expr::IsNull(Box::new(qualified(*e, table)), negated),
        other => other,
    }
}

// -------------------------------------------------------------- comparing

/// What a statement answered, in a form two runs can be compared by: a
/// NaN equals itself here, and an error is its code and its message.
fn shown(answer: &Result<RowSet, Error>) -> String {
    format!("{answer:?}")
}

/// Runs a query as chosen for and as read from the whole table, and
/// returns the first with what it read, having checked that the two
/// answer the same.
fn both_ways(db: &Arc<Database>, q: &Query, params: &[Value]) -> (Result<RowSet, Error>, Read) {
    let mut txn = db.begin_read();
    txn.plan = true;
    let chosen = txn.query_with(q, params, None);
    let read = txn.take_read();
    txn.plan = false;
    let whole = txn.query_with(q, params, None);
    assert_eq!(shown(&chosen), shown(&whole), "\nthe query: {q:?}\nits values: {params:?}\nchosen: {:?}", read.chosen);
    (chosen, read)
}

/// The two tables, read whole in the transaction that changed them.
fn tables(tx: &mut database_engine::db::WriteTxn) -> String {
    let t = tx.query(&Query::table("t").whole(), None);
    let u = tx.query(&Query::table("u").whole(), None);
    format!("{}\n{}", shown(&t), shown(&u))
}

enum Write {
    Update(Update),
    Delete(Delete),
}

/// Runs an update or a delete with the choice made and without, each in
/// a transaction that is then rolled back, and checks that it returned
/// the same and left the same tables.
fn write_both_ways(db: &Arc<Database>, write: &Write, params: &[Value]) -> Read {
    let run = |plan: bool| {
        let mut tx = db.begin_write(WAIT, false).unwrap();
        tx.set_now(1_700_000_000_000_000);
        tx.raw().unwrap().plan = plan;
        tx.bind(params.to_vec());
        let answer = match write {
            Write::Update(update) => tx.update(update, None),
            Write::Delete(delete) => tx.delete(delete, None),
        };
        let read = tx.take_read();
        // A statement that failed left nothing: the tables are as they
        // were, and reading them says so.
        let after = if answer.is_ok() {
            tx.raw().unwrap().plan = false;
            tables(&mut tx)
        } else {
            String::new()
        };
        (shown(&answer), after, read)
    };
    let (chosen, after_chosen, read) = run(true);
    let (whole, after_whole, _) = run(false);
    let what = match write {
        Write::Update(update) => format!("{update:?}"),
        Write::Delete(delete) => format!("{delete:?}"),
    };
    assert_eq!(chosen, whole, "\nwhat the statement returned\nthe statement: {what}\nits values: {params:?}\nchosen: {:?}", read.chosen);
    assert_eq!(after_chosen, after_whole, "\nwhat the statement left\nthe statement: {what}\nits values: {params:?}\nchosen: {:?}", read.chosen);
    read
}

fn new_database(vfs: &MemVfs) -> (database_engine::engine::Engine, Arc<Database>) {
    let engine = open(vfs);
    let db = engine.create_database("d").unwrap();
    (engine, db)
}

#[test]
fn a_chosen_way_in_answers_as_the_whole_table_does() {
    // R-Q23, R-Q24, R-W14
    // PLAN_SEEDS=2000 for a longer hunt.
    let seeds: u64 = std::env::var("PLAN_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let (mut cases, mut failed) = (0u64, 0u64);
    let mut ways: BTreeMap<&str, u64> = BTreeMap::new();
    let mut note = |read: &Read| {
        for chosen in &read.chosen {
            if chosen.table != "t" {
                continue;
            }
            *ways
                .entry(match &chosen.way {
                    Way::Whole => "the whole table",
                    Way::Primary => "the primary key",
                    Way::Index(_) => "an index",
                    Way::InOrder(_) => "a tree in its order",
                })
                .or_default() += 1;
        }
    };
    for seed in 0..seeds {
        let vfs = MemVfs::new();
        let mut rng = Rng::new(seed);
        let (_engine, db) = new_database(&vfs);
        let schema = make_schema(&mut rng, &db);
        let rows = match rng.below(12) {
            0 => 0,
            1 => 1 + rng.below(4),
            // Enough for an index's range to be given up as too wide.
            2 => 6_000,
            _ => 30 + rng.below(200),
        };
        fill(&mut rng, &db, &schema, rows);
        for _ in 0..150 {
            let mut gen = Gen { schema: &schema, alias: if rng.chance(3) { "a" } else { "t" }, params: Vec::new() };
            let q = gen.query(&mut rng);
            let (answer, read) = both_ways(&db, &q, &gen.params);
            cases += 1;
            failed += u64::from(answer.is_err());
            note(&read);
        }
        for _ in 0..30 {
            let mut gen = Gen { schema: &schema, alias: if rng.chance(3) { "a" } else { "t" }, params: Vec::new() };
            let mut rows = gen.rows(&mut rng);
            // The first few in some order: a batch leased from a queue.
            if rng.chance(4) {
                rows = gen.order(&mut rng, rows).limit(1 + rng.below(4));
            }
            let write = if rng.chance(3) {
                Write::Delete(Delete::rows(rows).returning_all())
            } else {
                let update = Update::rows(rows);
                let update = match rng.below(6) {
                    0 => update.set("j", col("j").add(1)),
                    1 => update.set("s", col("s").lower()).set("ok", col("ok").not()),
                    2 => update.set("i", col("i").add(1_000)),
                    3 => {
                        let value = value(&mut rng, &Type::Int);
                        update.set("j", gen.constant(&mut rng, value))
                    }
                    4 => update.set("x", "a").set("ts", now()),
                    _ => update.set("ok", true),
                };
                Write::Update(update.returning_all())
            };
            let read = write_both_ways(&db, &write, &gen.params);
            cases += 1;
            note(&read);
        }
    }
    println!("{cases} statements, {failed} of the queries answering with an error; ways into the table chosen: {ways:?}");
    // The comparison proves nothing where nothing was chosen: each kind
    // of choice must have been made, and often.
    for way in ["the primary key", "an index", "a tree in its order", "the whole table"] {
        let times = ways.get(way).copied().unwrap_or(0);
        assert!(times * 200 >= cases, "{way} was chosen {times} times in {cases} statements");
    }
    assert!(failed * 100 >= cases, "{failed} of {cases} statements failed: errors are compared too");
}

// ------------------------------------------------- the rules, one by one

/// A database with these tables and rows.
fn database(tables: Vec<TableDef>, rows: Vec<Insert>) -> (database_engine::engine::Engine, Arc<Database>) {
    let vfs = MemVfs::new();
    let (engine, db) = new_database(&vfs);
    let mut tx = db.begin_write(WAIT, false).unwrap();
    for def in tables {
        tx.ddl(&Ddl::CreateTable { def, if_not_exists: false }, None).unwrap();
    }
    for insert in rows {
        tx.insert(&insert, None).unwrap();
    }
    tx.commit().unwrap();
    (engine, db)
}

/// 1,000 notes of 20 owners, 50 each, a day for every 20 notes, a
/// category that is NULL for every fourth; and the 20 owners.
fn notes() -> (database_engine::engine::Engine, Arc<Database>) {
    let notes = TableDef::new("notes")
        .column(ColumnDef::new("id", Type::Int))
        .column(ColumnDef::new("owner", Type::Int))
        .column(ColumnDef::new("day", Type::Int))
        .column(ColumnDef::new("category", Type::Int).nullable())
        .column(ColumnDef::new("title", Type::Text))
        .column(ColumnDef::new("score", Type::Float))
        .column(ColumnDef::new("tags", Type::list(Type::Text)))
        .primary_key(["id"])
        .index(IndexDef::new("notes_owner_day").on(["owner", "day"]))
        .index(IndexDef::new("notes_category").on(["category"]))
        .index(IndexDef::new("notes_category_down").on_desc("category").on(["day"]))
        .index(IndexDef::new("notes_title").on_expr(col("title").lower()))
        .index(IndexDef::new("notes_early").on(["day"]).filter(col("day").lt(5)))
        .index(IndexDef::new("notes_score").on(["score"]))
        .index(IndexDef::new("notes_tags").on_each(col("tags")))
        .index(IndexDef::new("notes_words").on(["owner"]).full_text(TextConfig::Simple, [(col("title"), 1.0)]));
    let owners = TableDef::new("owners").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("name", Type::Text)).primary_key(["id"]);
    let mut rows = Vec::new();
    let mut insert = Insert::into("owners").columns(["id", "name"]);
    for id in 0..20 {
        insert = insert.values([val(id), val(format!("owner {id}"))]);
    }
    rows.push(insert);
    for chunk in 0..10 {
        let mut insert = Insert::into("notes").columns(["id", "owner", "day", "category", "title", "score", "tags"]);
        for id in chunk * 100..(chunk + 1) * 100 {
            let category = if id % 4 == 0 { Value::Null } else { Value::Int(id % 10) };
            insert = insert.values([val(id), val(id % 20), val(id / 20), Expr::Value(category), val(format!("Note {id}")), val(id as f64 / 2.0), val(vec![format!("t{}", id % 3)])]);
        }
        rows.push(insert);
    }
    database(vec![notes, owners], rows)
}

/// A query's answer, checked against the whole table's; what it read;
/// and the ways in it was given.
fn asked(db: &Arc<Database>, q: Query, params: &[Value]) -> (Result<RowSet, Error>, (u64, u64), Vec<String>) {
    let (answer, read) = both_ways(db, &q, params);
    (answer, (read.entries, read.rows), read.chosen.iter().map(|chosen| chosen.to_string()).collect())
}

/// What a query read, entries and rows, and how many rows it returned.
fn read(db: &Arc<Database>, q: Query) -> (u64, u64, usize) {
    let (answer, (entries, rows), _) = asked(db, q, &[]);
    (entries, rows, answer.expect("the query answers").rows.len())
}

/// The way in a query was given for its own table.
fn way(db: &Arc<Database>, q: Query) -> String {
    let (_, _, ways) = asked(db, q, &[]);
    ways.into_iter().next().unwrap_or_else(|| "as named".to_string())
}

const BY_KEY: &str = r#""notes" by its primary key"#;
const WHOLE: &str = r#""notes" by the whole table"#;

fn by(index: &str) -> String {
    format!(r#""notes" by index "{index}""#)
}

#[test]
fn equality_on_the_leading_parts_of_a_key_reads_that_range() {
    // R-Q23
    let (_engine, db) = notes();
    let all = || Query::table("notes");
    // The primary key: one row, a few rows, a range of rows.
    assert_eq!(read(&db, all().filter(col("id").eq(7))), (0, 1, 1));
    assert_eq!(read(&db, all().filter(val(7).eq(col("id")))), (0, 1, 1));
    assert_eq!(read(&db, all().filter(col("notes.id").eq(7_000))), (0, 0, 0));
    assert_eq!(read(&db, all().filter(col("id").in_list(vec![3i64, 900, 5, 3]))), (0, 3, 3));
    assert_eq!(read(&db, all().filter(col("id").ge(10).and(col("id").lt(15)))), (0, 5, 5));
    assert_eq!(read(&db, all().filter(col("id").between(10, 14))), (0, 5, 5));
    assert_eq!(read(&db, all().filter(col("id").gt(990))), (0, 9, 9));
    assert_eq!(way(&db, all().filter(col("id").eq(7))), BY_KEY);
    // An index: its first part, both parts, the first and a range of the
    // second; the rest of the filter is applied to what the range gives.
    assert_eq!(read(&db, all().filter(col("owner").eq(3))), (50, 50, 50));
    assert_eq!(read(&db, all().filter(col("owner").eq(3).and(col("day").eq(10)))), (1, 1, 1));
    assert_eq!(read(&db, all().filter(col("day").eq(10).and(col("owner").eq(3)))), (1, 1, 1));
    assert_eq!(read(&db, all().filter(col("owner").eq(3).and(col("day").ge(45)))), (5, 5, 5));
    assert_eq!(read(&db, all().filter(col("owner").eq(3).and(col("day").gt(10)).and(col("day").le(12)).and(col("day").lt(40)))), (2, 2, 2));
    assert_eq!(read(&db, all().filter(col("owner").eq(3).and(col("title").like("%7%")))), (50, 50, 5));
    assert_eq!(read(&db, all().filter(col("owner").in_list(vec![3i64, 4]).and(col("day").eq(10)))), (2, 2, 2));
    assert_eq!(way(&db, all().filter(col("owner").eq(3))), by("notes_owner_day"));
    // The second part alone is no range of this index.
    assert_eq!(way(&db, all().filter(col("day").eq(10))), WHOLE);
    // The rows come as the whole table gives them, in primary key order,
    // not in the index's.
    let (answer, ..) = asked(&db, all().filter(col("owner").in_list(vec![4i64, 3])).columns(["id"]).limit(4), &[]);
    assert_eq!(answer.unwrap().rows, vec![vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(23)], vec![Value::Int(24)]]);
    // An aggregate counts what the range gives.
    assert_eq!(read(&db, all().filter(col("owner").eq(3)).select("n", count())), (50, 50, 1));
}

#[test]
fn a_range_of_a_key_never_takes_in_the_keys_that_are_null() {
    // R-Q23
    let (_engine, db) = notes();
    let all = || Query::table("notes");
    // 250 notes have no category. Of the rest, 100 have each odd one
    // from 1 to 9 and 50 each even one from 0 to 8.
    assert_eq!(read(&db, all().filter(col("category").eq(7))), (100, 100, 100));
    assert_eq!(read(&db, all().filter(col("category").gt(7))), (150, 150, 150));
    assert_eq!(read(&db, all().filter(col("category").ge(7))), (250, 250, 250));
    assert_eq!(read(&db, all().filter(col("category").lt(2))), (150, 150, 150));
    assert_eq!(read(&db, all().filter(col("category").gt(1).and(col("category").le(3)))), (150, 150, 150));
    // A key that is NULL is asked for by name, and only so.
    assert_eq!(read(&db, all().filter(col("category").is_null())), (250, 250, 250));
    assert_eq!(read(&db, all().filter(col("category").is_not_distinct_from(null()))), (250, 250, 250));
    assert_eq!(read(&db, all().filter(col("category").is_not_distinct_from(7))), (100, 100, 100));
    assert_eq!(way(&db, all().filter(col("category").gt(7))), by("notes_category"));
    // The same on a key that descends, where NULLs come first: read in
    // its own order, the index gives up exactly the entries of the range.
    let down = |condition: Expr| {
        let (answer, cost, ways) = asked(&db, all().filter(condition).order_by(col("category").desc()).order_by(col("day")).limit(1_000), &[]);
        assert_eq!(ways, vec![format!("{}, in its order", by("notes_category_down"))]);
        (cost, answer.unwrap().rows.len())
    };
    assert_eq!(down(col("category").gt(7)), ((150, 150), 150));
    assert_eq!(down(col("category").ge(9)), ((100, 100), 100));
    assert_eq!(down(col("category").lt(2)), ((150, 150), 150));
    assert_eq!(down(col("category").le(0)), ((50, 50), 50));
    assert_eq!(down(col("category").gt(2).and(col("category").lt(5))), ((150, 150), 150));
}

#[test]
fn a_bound_that_is_null_holds_for_no_row_and_nothing_is_read() {
    // R-Q23
    let (_engine, db) = notes();
    let all = || Query::table("notes");
    for condition in [col("owner").eq(null()), col("id").lt(null()), col("title").ne(null()), col("id").in_list(list(Vec::<Expr>::new())), col("id").in_list(list([null()])), val(1).eq(2)] {
        assert_eq!(read(&db, all().filter(condition.clone().and(col("day").eq(1)))), (0, 0, 0), "{condition:?}");
    }
    // A parameter that is NULL when the statement runs, likewise; given
    // a value, the same statement reads that value's range.
    let by_owner = all().filter(col("owner").eq(param(0)).and(col("day").lt(param(1))));
    let (answer, cost, _) = asked(&db, by_owner.clone(), &[Value::Null, Value::Int(3)]);
    assert_eq!((answer.unwrap().rows.len(), cost), (0, (0, 0)));
    let (answer, cost, _) = asked(&db, by_owner.clone(), &[Value::Int(3), Value::Null]);
    assert_eq!((answer.unwrap().rows.len(), cost), (0, (0, 0)));
    let (answer, cost, ways) = asked(&db, by_owner.clone(), &[Value::Int(3), Value::Int(10)]);
    assert_eq!((answer.unwrap().rows.len(), cost, ways), (10, (10, 10), vec![by("notes_owner_day")]));
    // A statement run without the value it reads fails as it did.
    let (answer, cost, ways) = asked(&db, by_owner, &[Value::Int(3)]);
    assert!(answer.is_err());
    // (On the fourth row: the first three are not owner 3's, and the
    // second condition is not reached for them.)
    assert_eq!((cost, ways), ((0, 4), vec![WHOLE.to_string()]));
}

#[test]
fn a_bound_of_another_kind_is_not_made_into_a_key() {
    // R-Q23, R-Q22
    let uuid_of = |n: u8| [n; 16];
    let events = TableDef::new("notes")
        .column(ColumnDef::new("id", Type::Uuid))
        .column(ColumnDef::new("n", Type::Int))
        .column(ColumnDef::new("price", Type::decimal(12, 2)))
        .column(ColumnDef::new("at", Type::Timestamp))
        .column(ColumnDef::new("on", Type::Date))
        .primary_key(["id"])
        .index(IndexDef::new("notes_n").on(["n"]))
        .index(IndexDef::new("notes_price").on(["price"]))
        .index(IndexDef::new("notes_at").on(["at"]))
        .index(IndexDef::new("notes_on").on(["on"]));
    let mut insert = Insert::into("notes").columns(["id", "n", "price", "at", "on"]);
    for n in 0..40u8 {
        insert = insert.values([val(uuid_of(n)), val(i64::from(n)), val(format!("{n}.50")), Expr::Value(Value::Timestamp(i64::from(n) * 86_400_000_000)), Expr::Value(Value::Date(i32::from(n)))]);
    }
    let (_engine, db) = database(vec![events], vec![insert]);
    let all = || Query::table("notes");
    // A text beside a uuid, a timestamp, a date or a decimal is read as
    // one by the filter, and so it is as a bound.
    assert_eq!(read(&db, all().filter(col("id").eq(database_protocol::value::format_uuid(&uuid_of(7))))), (0, 1, 1));
    assert_eq!(read(&db, all().filter(col("price").eq("7.5"))), (1, 1, 1));
    assert_eq!(read(&db, all().filter(col("price").eq(" 7.50 "))), (1, 1, 1));
    assert_eq!(read(&db, all().filter(col("at").lt("1970-01-03T00:00:00Z"))), (2, 2, 2));
    assert_eq!(read(&db, all().filter(col("on").ge("1970-02-08"))), (2, 2, 2));
    // A decimal equals however it is written.
    assert_eq!(read(&db, all().filter(col("price").eq(Expr::Value(Value::Decimal("7.500".parse().unwrap()))))), (1, 1, 1));
    // Numbers of two kinds compare, and a date compares with a
    // timestamp, but neither is the key's own kind: the whole table.
    assert_eq!(read(&db, all().filter(col("price").eq(7))), (0, 40, 0));
    assert_eq!(read(&db, all().filter(col("n").eq(Expr::Value(Value::Decimal("7.0".parse().unwrap()))))), (0, 40, 1));
    assert_eq!(read(&db, all().filter(col("n").lt(2.5))), (0, 40, 3));
    assert_eq!(read(&db, all().filter(col("at").lt(Expr::Value(Value::Date(2))))), (0, 40, 2));
    assert_eq!(read(&db, all().filter(col("on").eq(Expr::Value(Value::Timestamp(86_400_000_000))))), (0, 40, 1));
    // What does not compare fails, as a wrong-typed comparison always
    // has, on the first row: it is not an empty answer.
    for condition in [col("n").eq("seven"), col("id").eq("not a uuid"), col("id").eq(7), col("on").lt("1970-02-30"), col("price").gt("much"), col("n").in_list(vec!["a", "b"])] {
        let (answer, cost, ways) = asked(&db, all().filter(condition.clone()), &[]);
        let Err(Error::Db(e)) = answer else { panic!("{condition:?} answered") };
        assert_eq!(e.code, database_protocol::error::ErrorCode::TypeMismatch, "{condition:?}");
        assert_eq!((cost, ways), ((0, 1), vec![WHOLE.to_string()]), "{condition:?}");
    }
    // And so does any other condition beside one that would have: the
    // range the first gives is not read, since the whole table read
    // would have failed on a row outside it.
    let (answer, _, ways) = asked(&db, all().filter(col("n").eq(7).and(col("price").gt("much"))), &[]);
    assert!(answer.is_err());
    assert_eq!(ways, vec![WHOLE.to_string()]);
}

#[test]
fn a_partial_index_is_read_only_where_the_filter_demands_its_condition() {
    // R-Q23
    let (_engine, db) = notes();
    let all = || Query::table("notes");
    // `notes_early` holds the notes of days before the fifth.
    assert_eq!(way(&db, all().filter(col("day").lt(5).and(col("day").eq(3)))), by("notes_early"));
    assert_eq!(read(&db, all().filter(col("day").lt(5).and(col("day").eq(3)))), (20, 20, 20));
    assert_eq!(read(&db, all().filter(col("day").ge(2).and(col("notes.day").lt(5)))), (60, 60, 60));
    // Nothing is deduced: a condition that implies the index's is not
    // the index's.
    assert_eq!(way(&db, all().filter(col("day").eq(3))), WHOLE);
    assert_eq!(way(&db, all().filter(col("day").lt(4).and(col("day").eq(3)))), WHOLE);
    assert_eq!(way(&db, all().filter(val(5).gt(col("day")).and(col("day").eq(3)))), WHOLE);
    // Under an `or` the condition is not demanded.
    assert_eq!(way(&db, all().filter(col("day").lt(5).or(col("id").eq(1)).and(col("day").eq(3)))), WHOLE);
}

#[test]
fn an_index_on_an_expression_is_read_where_the_filter_writes_the_expression() {
    // R-Q23, R-Q3
    let (_engine, db) = notes();
    let all = || Query::table("notes");
    assert_eq!(read(&db, all().filter(col("title").lower().eq("note 7"))), (1, 1, 1));
    assert_eq!(read(&db, all().filter(col("notes.title").lower().eq("note 7"))), (1, 1, 1));
    assert_eq!(read(&db, all().alias("n").filter(col("n.title").lower().ge("note 995"))), (5, 5, 5));
    assert_eq!(way(&db, all().filter(col("title").lower().eq("note 7"))), by("notes_title"));
    // Another expression of the same column is not the index's.
    assert_eq!(way(&db, all().filter(col("title").upper().eq("NOTE 7"))), WHOLE);
    assert_eq!(way(&db, all().filter(col("title").eq("Note 7"))), WHOLE);
}

#[test]
fn what_no_rule_covers_reads_the_whole_table() {
    // R-Q23
    let (_engine, db) = notes();
    let all = || Query::table("notes");
    for (what, q) in [
        ("an or", all().filter(col("owner").eq(3).or(col("owner").eq(4)))),
        ("a float's key", all().filter(col("score").eq(3.5))),
        ("an index over a list's items", all().filter(val("t1").in_list(col("tags")))),
        ("arithmetic on a column", all().filter(col("owner").eq(3).and(col("day").add(1).gt(2)))),
        ("a strict cast", all().filter(col("owner").eq(3).and(col("title").cast(Type::Int).eq(1)))),
        ("a subquery", all().filter(col("owner").eq(3).and(exists(Query::table("owners").key([col("notes.owner")]))))),
        ("each", all().each("tag", col("tags")).filter(col("owner").eq(3))),
        ("a column compared with a column", all().filter(col("owner").eq(col("day")))),
        ("not equal", all().filter(col("owner").ne(3))),
        ("no filter", all()),
    ] {
        assert_eq!(way(&db, q), WHOLE, "{what}");
    }
    // The rest of such a filter is still what it was: the row counts say
    // every row was read.
    assert_eq!(read(&db, all().filter(col("owner").eq(3).and(col("day").add(1).gt(2)))), (0, 1_000, 48));
    // What is computed once is not arithmetic on a column, and the clock
    // is a value like any other.
    assert_eq!(read(&db, all().filter(col("owner").eq(val(1).add(2)))), (50, 50, 50));
    assert_eq!(read(&db, all().filter(col("id").lt(now().sub(now())))), (0, 0, 0));
    // A cast that gives NULL in place of failing cannot fail.
    assert_eq!(read(&db, all().filter(col("owner").eq(3).and(col("title").try_cast(Type::Int).is_null()))), (50, 50, 50));
}

#[test]
fn between_two_trees_that_fit_the_choice_is_fixed() {
    // R-Q23
    let table = TableDef::new("notes")
        .column(ColumnDef::new("id", Type::Int))
        .column(ColumnDef::new("a", Type::Int))
        .column(ColumnDef::new("b", Type::Int))
        .column(ColumnDef::new("c", Type::Int))
        .column(ColumnDef::new("code", Type::Text).nullable())
        .primary_key(["id"])
        .index(IndexDef::new("by_a").on(["a"]))
        .index(IndexDef::new("by_a_b").on(["a", "b"]))
        .index(IndexDef::new("by_b").on(["b"]))
        .index(IndexDef::new("by_a_again").on(["a"]))
        .index(IndexDef::new("by_c_a").on(["c", "a"]))
        .index(IndexDef::new("by_code").on(["code"]).unique());
    let mut insert = Insert::into("notes").columns(["id", "a", "b", "c", "code"]);
    for id in 0..200i64 {
        insert = insert.values([val(id), val(id % 10), val(id % 7), val(id % 3), val(format!("code {id}"))]);
    }
    let (_engine, db) = database(vec![table], vec![insert]);
    let all = || Query::table("notes");
    // Of two that bind the same, the one defined first.
    assert_eq!(way(&db, all().filter(col("a").eq(1))), by("by_a"));
    // More parts bound by equality, wherever the conditions stand.
    assert_eq!(way(&db, all().filter(col("a").eq(1).and(col("b").eq(2)))), by("by_a_b"));
    assert_eq!(way(&db, all().filter(col("b").eq(2).and(col("a").eq(1)))), by("by_a_b"));
    assert_eq!(way(&db, all().filter(col("c").eq(2).and(col("a").eq(1)))), by("by_c_a"));
    // Equality before a range, and a range from both sides before one.
    assert_eq!(way(&db, all().filter(col("a").gt(1).and(col("b").eq(2)))), by("by_b"));
    assert_eq!(way(&db, all().filter(col("a").eq(1).and(col("b").gt(2)))), by("by_a_b"));
    assert_eq!(way(&db, all().filter(col("a").gt(1).and(col("b").gt(2)).and(col("b").lt(5)))), by("by_b"));
    // The primary key before an index that binds the same.
    assert_eq!(way(&db, all().filter(col("a").eq(1).and(col("id").eq(11)))), BY_KEY);
    assert_eq!(way(&db, all().filter(col("a").gt(1).and(col("id").gt(11)))), BY_KEY);
    // A unique key bound whole before more parts of another.
    assert_eq!(way(&db, all().filter(col("a").eq(1).and(col("b").eq(4)).and(col("code").eq("code 11")))), by("by_code"));
    // But not where the value bound is NULL: a unique index holds many.
    assert_eq!(way(&db, all().filter(col("a").eq(1).and(col("b").eq(4)).and(col("code").is_null()))), by("by_a_b"));
    // Run again, the same statement takes the same way.
    for _ in 0..3 {
        assert_eq!(way(&db, all().filter(col("b").eq(2).and(col("a").eq(1)).and(col("c").eq(0)))), by("by_a_b"));
    }
}

#[test]
fn a_page_in_a_trees_own_order_is_read_until_it_is_full() {
    // R-Q24
    let (_engine, db) = notes();
    let all = || Query::table("notes");
    let in_order = |index: &str| format!("{}, in its order", by(index));
    // The untuned page: one owner's notes by day, the first ten. Ten
    // entries and ten rows are read, where the whole table read took a
    // thousand rows and a sort.
    let page = all().filter(col("owner").eq(3)).order_by(col("day")).limit(10);
    assert_eq!(read(&db, page.clone()), (10, 10, 10));
    assert_eq!(way(&db, page.clone()), in_order("notes_owner_day"));
    assert_eq!(read(&db, page.clone().offset(5)), (15, 15, 10));
    // The newest ten: the same index, read backwards. No two of one
    // owner's notes tie here only if the key says so, and it does not,
    // so this page is sorted from the owner's range instead.
    let newest = all().filter(col("owner").eq(3)).order_by(col("day").desc()).limit(10);
    assert_eq!(read(&db, newest.clone()), (50, 50, 10));
    assert_eq!(way(&db, newest), by("notes_owner_day"));
    // With the primary key among the sort keys nothing ties, and it is
    // read backwards and stops.
    let newest = all().filter(col("owner").eq(3)).order_by(col("day").desc()).order_by(col("id").desc()).limit(10);
    assert_eq!(read(&db, newest.clone()), (10, 10, 10));
    assert_eq!(way(&db, newest), in_order("notes_owner_day"));
    // The primary key's own order, either way, with or without a range.
    assert_eq!(read(&db, all().order_by(col("id").desc()).limit(3)), (0, 3, 3));
    assert_eq!(read(&db, all().filter(col("id").lt(500)).order_by(col("id").desc()).limit(3)), (0, 3, 3));
    assert_eq!(way(&db, all().order_by(col("id").desc()).limit(3)), r#""notes" by its primary key, in its order"#);
    // A filter the index cannot serve is applied on the way, and the
    // read goes on until the page is full: the owner's first two notes
    // with a 7 in the title are his 36th and 37th.
    assert_eq!(read(&db, all().filter(col("owner").eq(3).and(col("title").like("%7%"))).order_by(col("day")).limit(2)), (37, 37, 2));
    // A range on the first sort key bounds the read as well.
    assert_eq!(read(&db, all().filter(col("owner").eq(3).and(col("day").ge(40))).order_by(col("day")).limit(3)), (3, 3, 3));
    // NULLs where the key has them: last going up. Asked for first, the
    // order is not the key's, and the page is sorted.
    assert_eq!(way(&db, all().order_by(col("category")).limit(5)), in_order("notes_category"));
    assert_eq!(way(&db, all().order_by(col("category").asc().nulls_first()).limit(5)), WHOLE);
    assert_eq!(way(&db, all().order_by(col("category").desc()).order_by(col("day")).limit(5)), in_order("notes_category_down"));
    // A narrower range is not given up for an order: the index by owner
    // and day binds a part by equality, the one by category none.
    assert_eq!(way(&db, all().filter(col("owner").eq(3)).order_by(col("category")).limit(5)), by("notes_owner_day"));
    // Not in order: no limit; a total asked for; distinct; a join; what
    // is selected could fail on a row past the page; a sort key that
    // names a selected item which is not the column.
    let owner = || all().filter(col("owner").eq(3)).order_by(col("day"));
    assert_eq!(way(&db, owner()), by("notes_owner_day"));
    assert_eq!(way(&db, owner().limit(10).with_total()), by("notes_owner_day"));
    assert_eq!(way(&db, owner().limit(10).distinct()), by("notes_owner_day"));
    assert_eq!(way(&db, owner().limit(10).join(Query::table("owners").key([col("notes.owner")]))), by("notes_owner_day"));
    assert_eq!(way(&db, owner().limit(10).select("next", col("day").add(1))), by("notes_owner_day"));
    assert_eq!(way(&db, owner().limit(10).select("day", col("id")).select("id", col("day"))), by("notes_owner_day"));
    assert_eq!(way(&db, owner().limit(10).select("day", col("day")).select("title", col("title").lower())), in_order("notes_owner_day"));
}

#[test]
fn rows_that_tie_in_a_page_come_in_primary_key_order() {
    // R-Q24
    let table = TableDef::new("mails")
        .column(ColumnDef::new("box", Type::Int))
        .column(ColumnDef::new("id", Type::Int))
        .column(ColumnDef::new("day", Type::Int))
        .column(ColumnDef::new("size", Type::Int))
        .primary_key(["box", "id"])
        .index(IndexDef::new("by_day").on(["box"]).on_desc("day"))
        .index(IndexDef::new("by_size_day").on(["size", "day"]));
    let mut insert = Insert::into("mails").columns(["box", "id", "day", "size"]);
    for id in 0..60i64 {
        insert = insert.values([val(id % 2), val(59 - id), val(id % 3), val(id % 4)]);
    }
    let (_engine, db) = database(vec![table], vec![insert]);
    // Twenty mails of a box share each day. The stable sort leaves them
    // by primary key, and so does the index, whose entries end with it.
    let page = Query::table("mails").filter(col("box").eq(1)).order_by(col("day").desc()).limit(25).columns(["id", "day"]);
    let (answer, cost, ways) = asked(&db, page, &[]);
    let rows = answer.unwrap().rows;
    assert_eq!((cost, ways), ((25, 25), vec![r#""mails" by index "by_day", in its order"#.to_string()]));
    let ids: Vec<i64> = rows.iter().map(|row| if let Value::Int(id) = row[0] { id } else { panic!() }).collect();
    assert!(ids[..10].windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
    // An index whose entries tie in another order than the primary
    // key's is not read in order: within a size and a day, `by_size_day`
    // has its mails by box and id, which is the primary key's order, but
    // within a size alone it has them by day first.
    let by_size = |q: Query| asked(&db, q, &[]).2;
    assert_eq!(by_size(Query::table("mails").filter(col("size").eq(1)).order_by(col("day")).limit(5)), vec![r#""mails" by index "by_size_day", in its order"#.to_string()]);
    assert_eq!(by_size(Query::table("mails").order_by(col("size")).limit(5)), vec![r#""mails" by the whole table"#.to_string()]);
}

#[test]
fn a_join_and_a_subquery_are_each_looked_up_by_their_own_filter() {
    // R-Q23
    let (_engine, db) = notes();
    // The inner side names no way in: it is a lookup by primary key for
    // each row so far, where it was the whole of `owners` for each.
    let joined = Query::table("notes").alias("n").filter(col("n.owner").eq(3)).join(Query::table("owners").alias("o").filter(col("o.id").eq(col("n.owner")))).select("name", col("o.name"));
    let (answer, cost, ways) = asked(&db, joined, &[]);
    assert_eq!(answer.unwrap().rows.len(), 50);
    assert_eq!((cost, ways), ((50, 100), vec![by("notes_owner_day"), r#""owners" by its primary key"#.to_string()]));
    // The outer side named, the inner chosen; and the other way round.
    let joined = Query::table("owners").alias("o").key([3]).join(Query::table("notes").alias("n").filter(col("n.owner").eq(col("o.id")).and(col("n.day").lt(5))));
    let (answer, cost, ways) = asked(&db, joined, &[]);
    assert_eq!(answer.unwrap().rows.len(), 5);
    assert_eq!((cost, ways), ((5, 6), vec![by("notes_owner_day")]));
    // A subquery that reads the outer row.
    let with_notes = Query::table("owners").filter(exists(Query::table("notes").filter(col("notes.owner").eq(col("owners.id")).and(col("notes.day").eq(0)))));
    let (answer, cost, ways) = asked(&db, with_notes, &[]);
    assert_eq!(answer.unwrap().rows.len(), 20);
    assert_eq!((cost, ways), ((20, 40), vec![r#""owners" by the whole table"#.to_string(), by("notes_owner_day")]));
    // An outer row whose key is NULL matches nothing, and reads nothing.
    let orphans = Query::table("notes").alias("n").filter(col("n.id").lt(8)).left_join(Query::table("owners").alias("o").filter(col("o.id").eq(col("n.category")))).select("name", col("o.name"));
    let (answer, cost, _) = asked(&db, orphans, &[]);
    let rows = answer.unwrap().rows;
    assert_eq!((rows.len(), rows[0][0].clone(), cost), (8, Value::Null, (0, 8 + 6)));
    // A join that could fail on a row keeps the outer query on the whole
    // table, where it fails or not as it always did: here the inner
    // filter adds to a column.
    let failing = Query::table("notes").alias("n").filter(col("n.owner").eq(3)).join(Query::table("owners").alias("o").filter(col("o.id").add(0).eq(col("n.owner"))));
    let (_, _, ways) = asked(&db, failing, &[]);
    assert_eq!(ways[0], WHOLE);
}

#[test]
fn an_update_and_a_delete_find_their_rows_as_a_query_does() {
    // R-W14
    let (_engine, db) = notes();
    let update = Update::table("notes").filter(col("owner").eq(3).and(col("day").lt(10))).set("title", "seen").returning(["id"]);
    let read = write_both_ways(&db, &Write::Update(update), &[]);
    assert_eq!((read.entries, read.rows), (10, 20), "ten rows found through the index, and each read again to be changed");
    assert_eq!(read.chosen, vec![Chosen { table: "notes".into(), way: Way::Index("notes_owner_day".into()) }]);
    let delete = Delete::from("notes").filter(col("id").ge(990)).returning_all();
    let read = write_both_ways(&db, &Write::Delete(delete), &[]);
    assert_eq!(read.chosen, vec![Chosen { table: "notes".into(), way: Way::Primary }]);
    assert!(read.rows < 100, "{read:?}");
    // A batch leased in some order is sorted by the update itself: its
    // rows are found by range, and the index's own order is not read.
    let lease = Update::rows(Query::table("notes").filter(col("owner").eq(3)).order_by(col("day")).limit(5)).set("title", "leased").returning(["id"]);
    let read = write_both_ways(&db, &Write::Update(lease), &[]);
    assert_eq!(read.chosen, vec![Chosen { table: "notes".into(), way: Way::Index("notes_owner_day".into()) }]);
    assert_eq!(read.entries, 50);
}

#[test]
fn an_index_range_wider_than_a_quarter_of_the_table_is_given_up() {
    // R-Q23
    let table = TableDef::new("notes").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("kind", Type::Int)).primary_key(["id"]).index(IndexDef::new("notes_kind").on(["kind"]));
    let mut rows = Vec::new();
    for chunk in 0..12i64 {
        let mut insert = Insert::into("notes").columns(["id", "kind"]);
        for id in chunk * 500..(chunk + 1) * 500 {
            // Half the notes are of kind 0, a twelfth each of 1 to 6.
            insert = insert.values([val(id), val(if id % 2 == 0 { 0 } else { 1 + (id / 2) % 6 })]);
        }
        rows.push(insert);
    }
    let (_engine, db) = database(vec![table], rows);
    assert_eq!(read(&db, Query::table("notes").filter(col("kind").eq(3))), (500, 500, 500));
    assert_eq!(way(&db, Query::table("notes").filter(col("kind").eq(3))), by("notes_kind"));
    // 3,000 of 6,000: the entries are read up to a quarter of the table
    // and one more, then every row.
    assert_eq!(read(&db, Query::table("notes").filter(col("kind").eq(0))), (1_501, 6_000, 3_000));
    assert_eq!(way(&db, Query::table("notes").filter(col("kind").eq(0))), WHOLE);
}

#[test]
fn a_way_in_that_is_named_is_read_as_named() {
    // R-Q23
    let (_engine, db) = notes();
    let ask = |q: &Query, plan: bool| {
        let mut txn = db.begin_read();
        txn.plan = plan;
        let rows = txn.query(q, None).unwrap().rows.len();
        let read = txn.take_read();
        (read.entries, read.rows, rows, read.chosen)
    };
    // The filter would give one row by primary key. The index named is
    // read all the same, and nothing is noted as chosen.
    let named = Query::table("notes").index("notes_owner_day", KeyRange::eq([3])).filter(col("id").eq(23));
    assert_eq!(ask(&named, true), (50, 50, 1, vec![]));
    let named = Query::table("notes").range(KeyRange::new(Bound::included([0]), Bound::Unbounded)).filter(col("owner").eq(3));
    assert_eq!(ask(&named, true), (0, 1_000, 50, vec![]));
    let named = Query::table("notes").reverse().filter(col("owner").eq(3));
    assert_eq!(ask(&named, true), (0, 1_000, 50, vec![]));
    // The whole table, asked for by name.
    let whole = Query::table("notes").whole().filter(col("owner").eq(3));
    assert_eq!(ask(&whole, true), (0, 1_000, 50, vec![]));
    assert_eq!(ask(&whole.clone().order_by(col("day")).limit(10), true), (0, 1_000, 10, vec![]));
    // And for every query, where choosing is turned off.
    let untuned = Query::table("notes").filter(col("owner").eq(3));
    assert_eq!(ask(&untuned, false), (0, 1_000, 50, vec![]));
    assert_eq!(ask(&untuned, true), (50, 50, 50, vec![Chosen { table: "notes".into(), way: Way::Index("notes_owner_day".into()) }]));
}

#[test]
fn the_choice_is_made_from_the_tables_as_they_are_when_the_statement_runs() {
    // R-Q23
    let (_engine, db) = notes();
    let by_title = Query::table("notes").filter(col("title").eq(param(0)));
    let value = [Value::Text("Note 7".into())];
    assert_eq!(asked(&db, by_title.clone(), &value).1, (0, 1_000));
    let ddl = |ddl: Ddl| {
        let mut tx = db.begin_write(WAIT, false).unwrap();
        tx.ddl(&ddl, None).unwrap();
        tx.commit().unwrap();
    };
    // An index made after the statement was written serves it; dropped,
    // it is the whole table again; and a partial one in its place serves
    // only the statement that demands its condition.
    ddl(Ddl::CreateIndex { table: "notes".into(), index: IndexDef::new("notes_by_title").on(["title"]), if_not_exists: false });
    assert_eq!(asked(&db, by_title.clone(), &value).1, (1, 1));
    ddl(Ddl::DropIndex { table: "notes".into(), index: "notes_by_title".into(), if_exists: false });
    assert_eq!(asked(&db, by_title.clone(), &value).1, (0, 1_000));
    ddl(Ddl::CreateIndex { table: "notes".into(), index: IndexDef::new("notes_by_title").on(["title"]).filter(col("score").ge(0.0)), if_not_exists: false });
    assert_eq!(asked(&db, by_title.clone(), &value).1, (0, 1_000));
    assert_eq!(asked(&db, by_title.filter(col("score").ge(0.0)), &value).1, (1, 1));
}
