//! Expressions: what a filter tests, what an update assigns, what a query
//! returns. The client builds them, the server evaluates them against rows.
//!
//! NULL behaves as in SQL: a comparison with NULL is NULL, `and`/`or` use
//! three-valued logic, and a filter keeps a row only when it is true.

use crate::query::Query;
use crate::value::{Type, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Value(Value),
    /// A column, optionally of one named table in the query. Without a
    /// table name the innermost table that has the column is meant, so a
    /// subquery reads its own table first and the enclosing row after.
    Column(Option<String>, String),
    /// In an upsert's update: the row that could not be inserted.
    Excluded(String),
    /// In an update's `returning`: the row as it was before.
    Old(String),
    /// In a tree walk: how many steps from a start row.
    Depth,
    /// In a full-text search: how well the row matches.
    Rank,
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    /// `IS NULL`, or `IS NOT NULL` when the flag is set.
    IsNull(Box<Expr>, bool),
    /// Whether the value equals some item of the list; negated by the flag.
    In(Box<Expr>, Box<Expr>, bool),
    Like { expr: Box<Expr>, pattern: Box<Expr>, case_insensitive: bool, negated: bool },
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Call(Func, Vec<Expr>),
    /// The first branch whose condition is true, else the last expression,
    /// else NULL.
    Case(Vec<(Expr, Expr)>, Option<Box<Expr>>),
    /// Converts to a type. With the flag set, a value that cannot be
    /// converted gives NULL instead of an error.
    Cast(Box<Expr>, Type, bool),
    /// A list of the items' values. Lists compare item by item, so two of
    /// them compare as SQL row values do.
    List(Vec<Expr>),
    Agg { func: AggFunc, arg: Option<Box<Expr>>, filter: Option<Box<Expr>>, distinct: bool },
    Subquery(Box<Query>, SubqueryKind),
    /// In a prepared statement: the value given for this place each time
    /// the statement is run, counting from 0. See `Request::Prepared`.
    Param(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CmpOp {
    Eq = 0,
    Ne = 1,
    Lt = 2,
    Le = 3,
    Gt = 4,
    Ge = 5,
    /// Not equal, where NULL counts as a value: never NULL itself.
    IsDistinctFrom = 6,
    IsNotDistinctFrom = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ArithOp {
    Add = 0,
    Sub = 1,
    Mul = 2,
    Div = 3,
    Mod = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AggFunc {
    /// Rows, or non-NULL values when given an argument.
    Count = 0,
    Sum = 1,
    Min = 2,
    Max = 3,
    Avg = 4,
    /// The values as a list, in the order the rows were read.
    Collect = 5,
    /// True if any value is true.
    BoolOr = 6,
    BoolAnd = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SubqueryKind {
    /// The first column of the first row, or NULL if there is none.
    Scalar = 0,
    /// Whether there is any row.
    Exists = 1,
    /// The first column of every row, as a list.
    List = 2,
}

/// The functions an expression can call. Unless said otherwise a NULL
/// argument gives NULL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Func {
    // Text.
    Lower = 1,
    Upper = 2,
    /// Length in characters.
    Length = 3,
    /// Length in bytes, of text or bytes. Does not load a large value.
    OctetLength = 4,
    /// Without leading and trailing spaces.
    Trim = 5,
    /// `left(text, n)`: the first n characters.
    Left = 6,
    /// `substr(text, start, length)`, counting characters from 1.
    Substr = 7,
    /// `substr_bytes(value, start, length)`, counting bytes from 1; gives
    /// bytes. Reads only the pages of a large value that the range covers.
    SubstrBytes = 8,
    /// `strpos(text, needle)`: where the needle first occurs, counting
    /// characters from 1, or 0.
    StrPos = 9,
    /// All arguments joined. NULL if any is NULL.
    Concat = 10,
    /// `replace(text, from, to)`.
    Replace = 11,
    /// Bytes as lower-case hex.
    Hex = 12,
    /// `similarity(a, b)`: how alike two texts are, from 0 to 1, by the
    /// three-letter runs they share. Postgres' `pg_trgm` measure.
    Similarity = 13,

    // Numbers.
    Abs = 20,
    Ceil = 21,
    Floor = 22,
    /// `round(x)` or `round(x, digits)`.
    Round = 23,
    Power = 24,
    /// The smallest argument, skipping NULLs.
    Least = 25,
    Greatest = 26,
    /// -1, 0 or 1.
    Sign = 27,

    // NULL handling.
    /// The first argument that is not NULL.
    Coalesce = 30,
    /// NULL if the two arguments are equal, else the first.
    NullIf = 31,

    // Time.
    /// The server's clock when the transaction began, or for a statement
    /// outside one, when the statement began. The same value however often
    /// it is read within one of those.
    Now = 40,
    /// `parse_timestamp(text[, offset_seconds])`: NULL if the text is not
    /// a point in time. See `time::parse_timestamp`.
    ParseTimestamp = 41,
    /// A fresh random uuid (version 4).
    RandomUuid = 42,
    /// `date_part(part, value)` of a timestamp (in UTC) or a date: `year`,
    /// `month`, `day`, `hour`, `minute`, `second`, `dow` (Sunday is 0) or
    /// `epoch` (seconds, with a fraction).
    DatePart = 43,

    // JSON. A path is given as further arguments: text for an object key,
    // an integer for an array index, negative ones counting from the end.
    /// `json_get(json, path...)`: the member, as JSON. NULL if absent.
    JsonGet = 50,
    /// The member as text: a string without its quotes, anything else as
    /// its JSON text. NULL if absent or JSON null.
    JsonGetText = 51,
    /// `object`, `array`, `string`, `number`, `boolean` or `null`.
    JsonTypeof = 52,
    /// Items in an array. NULL for anything else.
    JsonLen = 53,
    /// `json_contains(a, b)`: Postgres' `a @> b`.
    JsonContains = 54,
    /// Whether the text is a key of the object, or an item of the array.
    JsonHasKey = 55,
    /// The same for any of a list of texts.
    JsonHasAny = 56,
    JsonHasAll = 57,
    /// `a || b`: two objects merged, the second winning; two arrays joined.
    JsonMerge = 58,
    /// Without object members whose value is null, at every depth.
    JsonStripNulls = 59,
    /// `json_remove(json, key)`: the object without that key.
    JsonRemove = 60,
    /// An array's items as a list of JSON values. An empty list for NULL
    /// or anything that is not an array.
    JsonElems = 61,
    /// `json_pluck(array, key)`: the text of that key in each object of
    /// the array, as a list, skipping objects without it. An empty list
    /// for NULL.
    JsonPluck = 62,
    /// A JSON array of the arguments.
    JsonArray = 63,
    /// A JSON object from alternating keys and values.
    JsonObject = 64,
    /// Any value as JSON.
    ToJson = 65,
    /// An array's items as a list of text: a string as it is, a null as
    /// NULL, anything else as its JSON text. An empty list for NULL or
    /// anything that is not an array.
    JsonTexts = 66,

    // Lists.
    ListLen = 80,
    /// Whether two lists share an item.
    ListOverlaps = 81,
    /// One list after the other. A NULL argument counts as empty.
    ListConcat = 82,
    /// Each distinct item once, in order of first appearance.
    ListDistinct = 83,
    /// `list_position(list, item)`: counting from 1, or NULL.
    ListPosition = 84,

    // Full-text search without an index, on any text.
    /// `matches(text, query, config, prefix)`: whether the text holds every
    /// word of the query. `config` is `simple` or `english`; with `prefix`
    /// true each query word matches words that start with it.
    Matches = 90,
    /// `match_rank(text, query, config, prefix)`: how often.
    MatchRank = 91,
    /// `headline(text, query, config, start, stop, max_words, min_words)`:
    /// an excerpt around the matches, each wrapped in `start` and `stop`.
    Headline = 92,
}

impl Func {
    pub fn from_u16(v: u16) -> Option<Func> {
        use Func::*;
        Some(match v {
            1 => Lower,
            2 => Upper,
            3 => Length,
            4 => OctetLength,
            5 => Trim,
            6 => Left,
            7 => Substr,
            8 => SubstrBytes,
            9 => StrPos,
            10 => Concat,
            11 => Replace,
            12 => Hex,
            13 => Similarity,
            20 => Abs,
            21 => Ceil,
            22 => Floor,
            23 => Round,
            24 => Power,
            25 => Least,
            26 => Greatest,
            27 => Sign,
            30 => Coalesce,
            31 => NullIf,
            40 => Now,
            41 => ParseTimestamp,
            42 => RandomUuid,
            43 => DatePart,
            50 => JsonGet,
            51 => JsonGetText,
            52 => JsonTypeof,
            53 => JsonLen,
            54 => JsonContains,
            55 => JsonHasKey,
            56 => JsonHasAny,
            57 => JsonHasAll,
            58 => JsonMerge,
            59 => JsonStripNulls,
            60 => JsonRemove,
            61 => JsonElems,
            62 => JsonPluck,
            63 => JsonArray,
            64 => JsonObject,
            65 => ToJson,
            66 => JsonTexts,
            80 => ListLen,
            81 => ListOverlaps,
            82 => ListConcat,
            83 => ListDistinct,
            84 => ListPosition,
            90 => Matches,
            91 => MatchRank,
            92 => Headline,
            _ => return None,
        })
    }
}

impl CmpOp {
    pub fn from_u8(v: u8) -> Option<CmpOp> {
        use CmpOp::*;
        Some(match v {
            0 => Eq,
            1 => Ne,
            2 => Lt,
            3 => Le,
            4 => Gt,
            5 => Ge,
            6 => IsDistinctFrom,
            7 => IsNotDistinctFrom,
            _ => return None,
        })
    }
}

impl ArithOp {
    pub fn from_u8(v: u8) -> Option<ArithOp> {
        use ArithOp::*;
        Some(match v {
            0 => Add,
            1 => Sub,
            2 => Mul,
            3 => Div,
            4 => Mod,
            _ => return None,
        })
    }
}

impl AggFunc {
    pub fn from_u8(v: u8) -> Option<AggFunc> {
        use AggFunc::*;
        Some(match v {
            0 => Count,
            1 => Sum,
            2 => Min,
            3 => Max,
            4 => Avg,
            5 => Collect,
            6 => BoolOr,
            7 => BoolAnd,
            _ => return None,
        })
    }
}

impl SubqueryKind {
    pub fn from_u8(v: u8) -> Option<SubqueryKind> {
        Some(match v {
            0 => SubqueryKind::Scalar,
            1 => SubqueryKind::Exists,
            2 => SubqueryKind::List,
            _ => return None,
        })
    }
}

// --------------------------------------------------------------- building

impl<T: Into<Value>> From<T> for Expr {
    fn from(v: T) -> Self {
        Expr::Value(v.into())
    }
}

/// A column: `col("name")`, or `col("m.name")` for the table named `m`.
pub fn col(name: &str) -> Expr {
    match name.split_once('.') {
        Some((table, column)) => Expr::Column(Some(table.to_string()), column.to_string()),
        None => Expr::Column(None, name.to_string()),
    }
}

/// A constant.
pub fn val(v: impl Into<Value>) -> Expr {
    Expr::Value(v.into())
}

pub fn null() -> Expr {
    Expr::Value(Value::Null)
}

/// In a prepared statement: the `n`th value it is run with, counting from
/// 0. The statement is then sent once and run many times with different
/// values, as SQL's `$1` is.
pub fn param(n: u32) -> Expr {
    Expr::Param(n)
}

/// The server's clock. See `Func::Now`.
pub fn now() -> Expr {
    Expr::Call(Func::Now, Vec::new())
}

/// In an upsert's update: the value the insert proposed for a column.
pub fn excluded(column: &str) -> Expr {
    Expr::Excluded(column.to_string())
}

/// In an update's `returning`: a column as it was before the update.
pub fn old(column: &str) -> Expr {
    Expr::Old(column.to_string())
}

pub fn call(func: Func, args: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::Call(func, args.into_iter().collect())
}

/// True when every part is. An empty list is true.
pub fn all(parts: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::And(parts.into_iter().collect())
}

/// True when any part is. An empty list is false.
pub fn any(parts: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::Or(parts.into_iter().collect())
}

pub fn coalesce(args: impl IntoIterator<Item = Expr>) -> Expr {
    call(Func::Coalesce, args)
}

pub fn least(args: impl IntoIterator<Item = Expr>) -> Expr {
    call(Func::Least, args)
}

pub fn greatest(args: impl IntoIterator<Item = Expr>) -> Expr {
    call(Func::Greatest, args)
}

pub fn concat(args: impl IntoIterator<Item = Expr>) -> Expr {
    call(Func::Concat, args)
}

/// A list of values: `list([col("date"), col("id")]).lt(list([d, id]))` is
/// SQL's `(date, id) < ($1, $2)`.
pub fn list(items: impl IntoIterator<Item = Expr>) -> Expr {
    Expr::List(items.into_iter().collect())
}

/// `CASE WHEN condition THEN value ... ELSE otherwise END`.
pub fn case(branches: impl IntoIterator<Item = (Expr, Expr)>, otherwise: impl Into<Expr>) -> Expr {
    Expr::Case(branches.into_iter().collect(), Some(Box::new(otherwise.into())))
}

/// `when(c, a, b)`: a if c is true, else b.
pub fn when(condition: Expr, then: impl Into<Expr>, otherwise: impl Into<Expr>) -> Expr {
    Expr::Case(vec![(condition, then.into())], Some(Box::new(otherwise.into())))
}

/// The count of rows.
pub fn count() -> Expr {
    Expr::Agg { func: AggFunc::Count, arg: None, filter: None, distinct: false }
}

fn agg(func: AggFunc, arg: Expr) -> Expr {
    Expr::Agg { func, arg: Some(Box::new(arg)), filter: None, distinct: false }
}

pub fn sum(arg: Expr) -> Expr {
    agg(AggFunc::Sum, arg)
}

pub fn min(arg: Expr) -> Expr {
    agg(AggFunc::Min, arg)
}

pub fn max(arg: Expr) -> Expr {
    agg(AggFunc::Max, arg)
}

pub fn avg(arg: Expr) -> Expr {
    agg(AggFunc::Avg, arg)
}

pub fn collect(arg: Expr) -> Expr {
    agg(AggFunc::Collect, arg)
}

/// The first column of a query's first row, or NULL.
pub fn scalar(query: Query) -> Expr {
    Expr::Subquery(Box::new(query), SubqueryKind::Scalar)
}

/// Whether a query has any row.
pub fn exists(query: Query) -> Expr {
    Expr::Subquery(Box::new(query), SubqueryKind::Exists)
}

/// The first column of each of a query's rows, as a list.
pub fn list_of(query: Query) -> Expr {
    Expr::Subquery(Box::new(query), SubqueryKind::List)
}

/// A span of time in microseconds, to add to a timestamp:
/// `now() + seconds(60)`.
pub fn seconds(n: i64) -> Expr {
    Expr::Value(Value::Int(n * 1_000_000))
}

pub fn minutes(n: i64) -> Expr {
    seconds(n * 60)
}

pub fn hours(n: i64) -> Expr {
    seconds(n * 3600)
}

pub fn days(n: i64) -> Expr {
    seconds(n * 86_400)
}

/// A span given in fractional seconds.
pub fn seconds_f64(n: f64) -> Expr {
    Expr::Value(Value::Int((n * 1_000_000.0).round() as i64))
}

/// Escapes `%`, `_` and `\` so the text matches only itself in a `like`.
pub fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[allow(clippy::should_implement_trait)]
impl Expr {
    fn cmp(self, op: CmpOp, other: impl Into<Expr>) -> Expr {
        Expr::Cmp(op, Box::new(self), Box::new(other.into()))
    }
    pub fn eq(self, other: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Eq, other)
    }
    pub fn ne(self, other: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Ne, other)
    }
    pub fn lt(self, other: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Lt, other)
    }
    pub fn le(self, other: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Le, other)
    }
    pub fn gt(self, other: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Gt, other)
    }
    pub fn ge(self, other: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Ge, other)
    }
    /// Not equal, counting NULL as a value. Never NULL.
    pub fn is_distinct_from(self, other: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::IsDistinctFrom, other)
    }
    /// Equal, counting NULL as a value. Never NULL.
    pub fn is_not_distinct_from(self, other: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::IsNotDistinctFrom, other)
    }
    pub fn is_null(self) -> Expr {
        Expr::IsNull(Box::new(self), false)
    }
    pub fn is_not_null(self) -> Expr {
        Expr::IsNull(Box::new(self), true)
    }
    /// True unless the value is true: NULL counts as false.
    pub fn is_not_true(self) -> Expr {
        self.is_distinct_from(true)
    }
    pub fn and(self, other: impl Into<Expr>) -> Expr {
        match self {
            Expr::And(mut parts) => {
                parts.push(other.into());
                Expr::And(parts)
            }
            first => Expr::And(vec![first, other.into()]),
        }
    }
    pub fn or(self, other: impl Into<Expr>) -> Expr {
        match self {
            Expr::Or(mut parts) => {
                parts.push(other.into());
                Expr::Or(parts)
            }
            first => Expr::Or(vec![first, other.into()]),
        }
    }
    pub fn not(self) -> Expr {
        Expr::Not(Box::new(self))
    }
    /// Whether the value equals an item of the list: a `Value::List`, a
    /// list column, or `list_of(query)`.
    /// `low <= self AND self <= high`.
    pub fn between(self, low: impl Into<Expr>, high: impl Into<Expr>) -> Expr {
        Expr::And(vec![self.clone().ge(low), self.le(high)])
    }
    pub fn in_list(self, list: impl Into<Expr>) -> Expr {
        Expr::In(Box::new(self), Box::new(list.into()), false)
    }
    pub fn not_in_list(self, list: impl Into<Expr>) -> Expr {
        Expr::In(Box::new(self), Box::new(list.into()), true)
    }
    /// SQL `LIKE`: `%` matches any run of characters, `_` any one, and a
    /// backslash makes the next character literal.
    pub fn like(self, pattern: impl Into<Expr>) -> Expr {
        Expr::Like { expr: Box::new(self), pattern: Box::new(pattern.into()), case_insensitive: false, negated: false }
    }
    pub fn not_like(self, pattern: impl Into<Expr>) -> Expr {
        Expr::Like { expr: Box::new(self), pattern: Box::new(pattern.into()), case_insensitive: false, negated: true }
    }
    /// `LIKE` without regard to case.
    pub fn ilike(self, pattern: impl Into<Expr>) -> Expr {
        Expr::Like { expr: Box::new(self), pattern: Box::new(pattern.into()), case_insensitive: true, negated: false }
    }
    pub fn add(self, other: impl Into<Expr>) -> Expr {
        Expr::Arith(ArithOp::Add, Box::new(self), Box::new(other.into()))
    }
    pub fn sub(self, other: impl Into<Expr>) -> Expr {
        Expr::Arith(ArithOp::Sub, Box::new(self), Box::new(other.into()))
    }
    pub fn mul(self, other: impl Into<Expr>) -> Expr {
        Expr::Arith(ArithOp::Mul, Box::new(self), Box::new(other.into()))
    }
    pub fn div(self, other: impl Into<Expr>) -> Expr {
        Expr::Arith(ArithOp::Div, Box::new(self), Box::new(other.into()))
    }
    pub fn cast(self, to: Type) -> Expr {
        Expr::Cast(Box::new(self), to, false)
    }
    /// Converts, giving NULL where `cast` would fail.
    pub fn try_cast(self, to: Type) -> Expr {
        Expr::Cast(Box::new(self), to, true)
    }
    fn call1(self, func: Func) -> Expr {
        Expr::Call(func, vec![self])
    }
    fn call2(self, func: Func, other: impl Into<Expr>) -> Expr {
        Expr::Call(func, vec![self, other.into()])
    }
    pub fn lower(self) -> Expr {
        self.call1(Func::Lower)
    }
    pub fn upper(self) -> Expr {
        self.call1(Func::Upper)
    }
    pub fn length(self) -> Expr {
        self.call1(Func::Length)
    }
    pub fn octet_length(self) -> Expr {
        self.call1(Func::OctetLength)
    }
    pub fn trim(self) -> Expr {
        self.call1(Func::Trim)
    }
    pub fn left(self, chars: i64) -> Expr {
        self.call2(Func::Left, chars)
    }
    pub fn substr(self, start: impl Into<Expr>, len: impl Into<Expr>) -> Expr {
        Expr::Call(Func::Substr, vec![self, start.into(), len.into()])
    }
    pub fn substr_bytes(self, start: impl Into<Expr>, len: impl Into<Expr>) -> Expr {
        Expr::Call(Func::SubstrBytes, vec![self, start.into(), len.into()])
    }
    pub fn strpos(self, needle: impl Into<Expr>) -> Expr {
        self.call2(Func::StrPos, needle)
    }
    pub fn or_else(self, fallback: impl Into<Expr>) -> Expr {
        Expr::Call(Func::Coalesce, vec![self, fallback.into()])
    }
    /// A member of a JSON value: `col("detail").json(["device", "name"])`.
    pub fn json<P: Into<Expr>>(self, path: impl IntoIterator<Item = P>) -> Expr {
        let mut args = vec![self];
        args.extend(path.into_iter().map(Into::into));
        Expr::Call(Func::JsonGet, args)
    }
    /// The same member as text.
    pub fn json_text<P: Into<Expr>>(self, path: impl IntoIterator<Item = P>) -> Expr {
        let mut args = vec![self];
        args.extend(path.into_iter().map(Into::into));
        Expr::Call(Func::JsonGetText, args)
    }
    pub fn json_contains(self, other: impl Into<Expr>) -> Expr {
        self.call2(Func::JsonContains, other)
    }
    pub fn json_has_key(self, key: impl Into<Expr>) -> Expr {
        self.call2(Func::JsonHasKey, key)
    }
    pub fn json_has_any(self, keys: impl Into<Expr>) -> Expr {
        self.call2(Func::JsonHasAny, keys)
    }
    pub fn json_has_all(self, keys: impl Into<Expr>) -> Expr {
        self.call2(Func::JsonHasAll, keys)
    }
    pub fn json_merge(self, other: impl Into<Expr>) -> Expr {
        self.call2(Func::JsonMerge, other)
    }
    pub fn json_len(self) -> Expr {
        self.call1(Func::JsonLen)
    }
    pub fn json_pluck(self, key: &str) -> Expr {
        self.call2(Func::JsonPluck, key)
    }
    /// A JSON array's items as a list of text.
    pub fn json_texts(self) -> Expr {
        self.call1(Func::JsonTexts)
    }
    pub fn list_overlaps(self, other: impl Into<Expr>) -> Expr {
        self.call2(Func::ListOverlaps, other)
    }
    pub fn list_concat(self, other: impl Into<Expr>) -> Expr {
        self.call2(Func::ListConcat, other)
    }
    /// An aggregate over only the rows where the condition is true.
    pub fn filter(self, condition: Expr) -> Expr {
        match self {
            Expr::Agg { func, arg, distinct, .. } => Expr::Agg { func, arg, filter: Some(Box::new(condition)), distinct },
            other => other,
        }
    }
    /// An aggregate over each distinct value once.
    pub fn distinct(self) -> Expr {
        match self {
            Expr::Agg { func, arg, filter, .. } => Expr::Agg { func, arg, filter, distinct: true },
            other => other,
        }
    }
    /// For `order_by`: this expression, descending.
    pub fn desc(self) -> crate::query::Order {
        crate::query::Order { expr: self, desc: true, nulls_last: None }
    }
    pub fn asc(self) -> crate::query::Order {
        crate::query::Order { expr: self, desc: false, nulls_last: None }
    }

    /// Whether the expression holds an aggregate, outside any subquery.
    pub fn has_aggregate(&self) -> bool {
        let mut found = false;
        self.visit(&mut |e| found |= matches!(e, Expr::Agg { .. }));
        found
    }

    /// Calls `f` with this expression and each one inside it, not entering
    /// subqueries.
    pub fn visit(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Not(e) | Expr::IsNull(e, _) | Expr::Neg(e) | Expr::Cast(e, _, _) => e.visit(f),
            Expr::And(parts) | Expr::Or(parts) | Expr::List(parts) | Expr::Call(_, parts) => parts.iter().for_each(|e| e.visit(f)),
            Expr::Cmp(_, a, b) | Expr::Arith(_, a, b) | Expr::In(a, b, _) => {
                a.visit(f);
                b.visit(f);
            }
            Expr::Like { expr, pattern, .. } => {
                expr.visit(f);
                pattern.visit(f);
            }
            Expr::Case(branches, otherwise) => {
                for (c, v) in branches {
                    c.visit(f);
                    v.visit(f);
                }
                if let Some(e) = otherwise {
                    e.visit(f);
                }
            }
            Expr::Agg { arg, filter, .. } => {
                if let Some(e) = arg {
                    e.visit(f);
                }
                if let Some(e) = filter {
                    e.visit(f);
                }
            }
            Expr::Value(_) | Expr::Column(..) | Expr::Excluded(_) | Expr::Old(_) | Expr::Depth | Expr::Rank | Expr::Subquery(..) | Expr::Param(_) => {}
        }
    }
}

macro_rules! operator {
    ($trait:ident, $method:ident, $op:expr) => {
        impl<T: Into<Expr>> std::ops::$trait<T> for Expr {
            type Output = Expr;
            fn $method(self, other: T) -> Expr {
                Expr::Arith($op, Box::new(self), Box::new(other.into()))
            }
        }
    };
}

operator!(Add, add, ArithOp::Add);
operator!(Sub, sub, ArithOp::Sub);
operator!(Mul, mul, ArithOp::Mul);
operator!(Div, div, ArithOp::Div);
operator!(Rem, rem, ArithOp::Mod);

impl std::ops::Neg for Expr {
    type Output = Expr;
    fn neg(self) -> Expr {
        Expr::Neg(Box::new(self))
    }
}

impl std::ops::Not for Expr {
    type Output = Expr;
    fn not(self) -> Expr {
        Expr::Not(Box::new(self))
    }
}
