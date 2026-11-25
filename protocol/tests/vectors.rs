//! Golden vectors: named messages, each described in JSON and given as
//! the bytes this crate encodes it to.
//!
//! `protocol/fixtures/vectors.json` is what `docs/PROTOCOL.md` promises,
//! in a form a program can check. This test builds every message with the
//! crate's own builders, encodes it, and requires the file to say the
//! same; the clients in other languages (`clients/`) read the file, build
//! each message from its description, and must arrive at the same bytes.
//! A change to the wire format therefore fails here first, and cannot be
//! made without the file, the specification and the other clients being
//! looked at.
//!
//! The vectors are of one protocol version (`VECTORS_VERSION`). A later
//! version of the crate may add requests, expressions and error codes;
//! those are not described here, and their presence does not fail these
//! tests. What must not change is how a message of this version is
//! written and read.
//!
//! To write the file again after a deliberate change:
//!
//! ```sh
//! PROTOCOL_VECTORS=write cargo test -p database-protocol --test vectors
//! ```

use database_protocol::codec::RESERVE;
use database_protocol::decimal::{Decimal, MAX_SCALE};
use database_protocol::error::{DbError, ErrorCode};
use database_protocol::expr::*;
use database_protocol::message::*;
use database_protocol::query::*;
use database_protocol::schema::*;
use database_protocol::value::{Type, Value, MAX_JSON_DEPTH};
use serde_json::{json, Map, Value as Json};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/vectors.json");

/// The protocol version these vectors are of: the one `docs/PROTOCOL.md`
/// describes and the clients in `clients/` speak. It is written here and
/// not taken from the crate's `PROTOCOL_VERSION`, which moves on when a
/// later version adds requests: a server of a later version still reads
/// every message of this one, and that is what the vectors hold it to.
const VECTORS_VERSION: u32 = 2;

// The crate is at that version, or at a later one that still reads it.
const _: () = assert!(PROTOCOL_VERSION >= VECTORS_VERSION);

/// The highest error code of that version. Codes above it belong to later
/// versions and are not described here.
const LAST_ERROR_CODE: u8 = 22;

// ------------------------------------------------------------ describing
//
// A message as JSON. Every node of an enum is an object with a `kind`;
// integers that may pass 2^53 are strings; bytes are hex; a float is the
// 16 hex digits of its bits, so that infinities and NaN can be written.

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

fn value(v: &Value) -> Json {
    match v {
        Value::Null => json!({"kind": "null"}),
        Value::Bool(b) => json!({"kind": "bool", "value": b}),
        Value::Int(n) => json!({"kind": "int", "value": n.to_string()}),
        Value::Float(f) => json!({"kind": "float", "bits": format!("{:016x}", f.to_bits())}),
        Value::Decimal(d) => json!({"kind": "decimal", "mantissa": d.mantissa().to_string(), "scale": d.scale()}),
        Value::Text(s) => json!({"kind": "text", "value": s}),
        Value::Bytes(b) => json!({"kind": "bytes", "hex": hex(b)}),
        Value::Uuid(u) => json!({"kind": "uuid", "hex": hex(u)}),
        Value::Timestamp(t) => json!({"kind": "timestamp", "micros": t.to_string()}),
        Value::Date(d) => json!({"kind": "date", "days": d}),
        Value::Json(j) => json!({"kind": "json", "text": j.to_string()}),
        Value::List(items) => json!({"kind": "list", "items": items.iter().map(value).collect::<Vec<_>>()}),
    }
}

fn ty(t: &Type) -> Json {
    match t {
        Type::Bool => json!({"kind": "bool"}),
        Type::Int => json!({"kind": "int"}),
        Type::Float => json!({"kind": "float"}),
        Type::Decimal(None) => json!({"kind": "decimal", "precision": null, "scale": null}),
        Type::Decimal(Some((p, s))) => json!({"kind": "decimal", "precision": p, "scale": s}),
        Type::Text => json!({"kind": "text"}),
        Type::Bytes => json!({"kind": "bytes"}),
        Type::Uuid => json!({"kind": "uuid"}),
        Type::Timestamp => json!({"kind": "timestamp"}),
        Type::Date => json!({"kind": "date"}),
        Type::Json => json!({"kind": "json"}),
        Type::List(of) => json!({"kind": "list", "of": ty(of)}),
    }
}

const CMP_OPS: [(CmpOp, &str); 8] = [
    (CmpOp::Eq, "eq"),
    (CmpOp::Ne, "ne"),
    (CmpOp::Lt, "lt"),
    (CmpOp::Le, "le"),
    (CmpOp::Gt, "gt"),
    (CmpOp::Ge, "ge"),
    (CmpOp::IsDistinctFrom, "is_distinct_from"),
    (CmpOp::IsNotDistinctFrom, "is_not_distinct_from"),
];

const ARITH_OPS: [(ArithOp, &str); 5] = [(ArithOp::Add, "add"), (ArithOp::Sub, "sub"), (ArithOp::Mul, "mul"), (ArithOp::Div, "div"), (ArithOp::Mod, "mod")];

const AGG_FUNCS: [(AggFunc, &str); 8] = [
    (AggFunc::Count, "count"),
    (AggFunc::Sum, "sum"),
    (AggFunc::Min, "min"),
    (AggFunc::Max, "max"),
    (AggFunc::Avg, "avg"),
    (AggFunc::Collect, "collect"),
    (AggFunc::BoolOr, "bool_or"),
    (AggFunc::BoolAnd, "bool_and"),
];

const SUBQUERY_KINDS: [(SubqueryKind, &str); 3] = [(SubqueryKind::Scalar, "scalar"), (SubqueryKind::Exists, "exists"), (SubqueryKind::List, "list")];

const FUNCS: [(Func, &str); 52] = [
    (Func::Lower, "lower"),
    (Func::Upper, "upper"),
    (Func::Length, "length"),
    (Func::OctetLength, "octet_length"),
    (Func::Trim, "trim"),
    (Func::Left, "left"),
    (Func::Substr, "substr"),
    (Func::SubstrBytes, "substr_bytes"),
    (Func::StrPos, "strpos"),
    (Func::Concat, "concat"),
    (Func::Replace, "replace"),
    (Func::Hex, "hex"),
    (Func::Similarity, "similarity"),
    (Func::Abs, "abs"),
    (Func::Ceil, "ceil"),
    (Func::Floor, "floor"),
    (Func::Round, "round"),
    (Func::Power, "power"),
    (Func::Least, "least"),
    (Func::Greatest, "greatest"),
    (Func::Sign, "sign"),
    (Func::Coalesce, "coalesce"),
    (Func::NullIf, "nullif"),
    (Func::Now, "now"),
    (Func::ParseTimestamp, "parse_timestamp"),
    (Func::RandomUuid, "random_uuid"),
    (Func::DatePart, "date_part"),
    (Func::JsonGet, "json_get"),
    (Func::JsonGetText, "json_get_text"),
    (Func::JsonTypeof, "json_typeof"),
    (Func::JsonLen, "json_len"),
    (Func::JsonContains, "json_contains"),
    (Func::JsonHasKey, "json_has_key"),
    (Func::JsonHasAny, "json_has_any"),
    (Func::JsonHasAll, "json_has_all"),
    (Func::JsonMerge, "json_merge"),
    (Func::JsonStripNulls, "json_strip_nulls"),
    (Func::JsonRemove, "json_remove"),
    (Func::JsonElems, "json_elems"),
    (Func::JsonPluck, "json_pluck"),
    (Func::JsonArray, "json_array"),
    (Func::JsonObject, "json_object"),
    (Func::ToJson, "to_json"),
    (Func::JsonTexts, "json_texts"),
    (Func::ListLen, "list_len"),
    (Func::ListOverlaps, "list_overlaps"),
    (Func::ListConcat, "list_concat"),
    (Func::ListDistinct, "list_distinct"),
    (Func::ListPosition, "list_position"),
    (Func::Matches, "matches"),
    (Func::MatchRank, "match_rank"),
    (Func::Headline, "headline"),
];

const ERROR_CODES: [(ErrorCode, &str); 22] = [
    (ErrorCode::UniqueViolation, "UniqueViolation"),
    (ErrorCode::ForeignKeyViolation, "ForeignKeyViolation"),
    (ErrorCode::NotNullViolation, "NotNullViolation"),
    (ErrorCode::CheckViolation, "CheckViolation"),
    (ErrorCode::TypeMismatch, "TypeMismatch"),
    (ErrorCode::NotFound, "NotFound"),
    (ErrorCode::AlreadyExists, "AlreadyExists"),
    (ErrorCode::InvalidRequest, "InvalidRequest"),
    (ErrorCode::KeyTooLong, "KeyTooLong"),
    (ErrorCode::LockTimeout, "LockTimeout"),
    (ErrorCode::Timeout, "Timeout"),
    (ErrorCode::TransactionAborted, "TransactionAborted"),
    (ErrorCode::AccessDenied, "AccessDenied"),
    (ErrorCode::ProtocolMismatch, "ProtocolMismatch"),
    (ErrorCode::Connection, "Connection"),
    (ErrorCode::Io, "Io"),
    (ErrorCode::Corrupt, "Corrupt"),
    (ErrorCode::Unavailable, "Unavailable"),
    (ErrorCode::Internal, "Internal"),
    (ErrorCode::Arithmetic, "Arithmetic"),
    (ErrorCode::OutOfRange, "OutOfRange"),
    (ErrorCode::Conflict, "Conflict"),
];

fn named<T: PartialEq + Copy>(table: &[(T, &'static str)], item: T) -> &'static str {
    table.iter().find(|(known, _)| *known == item).map(|(_, name)| *name).expect("every variant has a name")
}

fn opt_expr(e: &Option<Expr>) -> Json {
    e.as_ref().map_or(Json::Null, expr)
}

fn opt_boxed(e: &Option<Box<Expr>>) -> Json {
    e.as_ref().map_or(Json::Null, |e| expr(e))
}

fn exprs(list: &[Expr]) -> Json {
    Json::Array(list.iter().map(expr).collect())
}

fn expr(e: &Expr) -> Json {
    match e {
        Expr::Value(v) => json!({"kind": "value", "value": value(v)}),
        Expr::Column(table, name) => json!({"kind": "column", "table": table, "name": name}),
        Expr::Excluded(name) => json!({"kind": "excluded", "name": name}),
        Expr::Old(name) => json!({"kind": "old", "name": name}),
        Expr::Depth => json!({"kind": "depth"}),
        Expr::Rank => json!({"kind": "rank"}),
        Expr::Not(e) => json!({"kind": "not", "expr": expr(e)}),
        Expr::And(parts) => json!({"kind": "and", "parts": exprs(parts)}),
        Expr::Or(parts) => json!({"kind": "or", "parts": exprs(parts)}),
        Expr::Cmp(op, a, b) => json!({"kind": "cmp", "op": named(&CMP_OPS, *op), "left": expr(a), "right": expr(b)}),
        Expr::IsNull(e, negated) => json!({"kind": "is_null", "expr": expr(e), "negated": negated}),
        Expr::In(e, list, negated) => json!({"kind": "in", "expr": expr(e), "list": expr(list), "negated": negated}),
        Expr::Like { expr: e, pattern, case_insensitive, negated } => {
            json!({"kind": "like", "expr": expr(e), "pattern": expr(pattern), "case_insensitive": case_insensitive, "negated": negated})
        }
        Expr::Arith(op, a, b) => json!({"kind": "arith", "op": named(&ARITH_OPS, *op), "left": expr(a), "right": expr(b)}),
        Expr::Neg(e) => json!({"kind": "neg", "expr": expr(e)}),
        Expr::Call(func, args) => json!({"kind": "call", "func": named(&FUNCS, *func), "args": exprs(args)}),
        Expr::Case(branches, otherwise) => json!({
            "kind": "case",
            "branches": branches.iter().map(|(c, v)| json!({"when": expr(c), "then": expr(v)})).collect::<Vec<_>>(),
            "otherwise": opt_boxed(otherwise),
        }),
        Expr::Cast(e, to, lenient) => json!({"kind": "cast", "expr": expr(e), "to": ty(to), "lenient": lenient}),
        Expr::List(items) => json!({"kind": "list", "items": exprs(items)}),
        Expr::Agg { func, arg, filter, distinct } => {
            json!({"kind": "agg", "func": named(&AGG_FUNCS, *func), "arg": opt_boxed(arg), "filter": opt_boxed(filter), "distinct": distinct})
        }
        Expr::Subquery(q, kind) => json!({"kind": "subquery", "subquery": named(&SUBQUERY_KINDS, *kind), "query": query(q)}),
        // An expression of a later version: no vector here holds one.
        #[allow(unreachable_patterns)]
        _ => panic!("an expression that version {VECTORS_VERSION} of the protocol does not have"),
    }
}

fn selects(items: &[Select]) -> Json {
    Json::Array(items.iter().map(|s| json!({"name": s.name, "expr": expr(&s.expr)})).collect())
}

fn bound(b: &Bound) -> Json {
    match b {
        Bound::Unbounded => json!({"kind": "unbounded"}),
        Bound::Included(values) => json!({"kind": "included", "values": exprs(values)}),
        Bound::Excluded(values) => json!({"kind": "excluded", "values": exprs(values)}),
    }
}

fn ranges(list: &[KeyRange]) -> Json {
    Json::Array(list.iter().map(|r| json!({"lower": bound(&r.lower), "upper": bound(&r.upper), "nulls": r.nulls})).collect())
}

fn access(a: &Access) -> Json {
    match a {
        Access::Primary { ranges: r, reverse } => json!({"kind": "primary", "ranges": ranges(r), "reverse": reverse}),
        Access::Index { index, ranges: r, reverse } => json!({"kind": "index", "index": index, "ranges": ranges(r), "reverse": reverse}),
        Access::Walk { start, parent, direction, index, prune, stop, max_depth } => json!({
            "kind": "walk",
            "start": start.iter().map(|key| exprs(key)).collect::<Vec<_>>(),
            "parent": parent,
            "direction": match direction { Direction::Up => "up", Direction::Down => "down" },
            "index": index,
            "prune": opt_expr(prune),
            "stop": opt_expr(stop),
            "max_depth": max_depth,
        }),
        Access::Search { index, prefix, query, match_prefix } => {
            json!({"kind": "search", "index": index, "prefix": exprs(prefix), "query": expr(query), "match_prefix": match_prefix})
        }
    }
}

fn query(q: &Query) -> Json {
    json!({
        "table": q.table,
        "alias": q.alias,
        "access": access(&q.access),
        "joins": q.joins.iter().map(|j| json!({
            "kind": match j.kind { JoinKind::Inner => "inner", JoinKind::Left => "left" },
            "query": query(&j.query),
        })).collect::<Vec<_>>(),
        "unnest": q.unnest.as_ref().map_or(Json::Null, |s| json!({"name": s.name, "expr": expr(&s.expr)})),
        "filter": opt_expr(&q.filter),
        "group_by": exprs(&q.group_by),
        "having": opt_expr(&q.having),
        "order_by": q.order_by.iter().map(|o| json!({"expr": expr(&o.expr), "desc": o.desc, "nulls_last": o.nulls_last})).collect::<Vec<_>>(),
        "offset": q.offset.to_string(),
        "limit": q.limit.map(|n| n.to_string()),
        "select": selects(&q.select),
        "distinct": q.distinct,
        "with_total": q.with_total,
    })
}

fn assigns(set: &[Assign]) -> Json {
    Json::Array(set.iter().map(|a| json!({"column": a.column, "value": expr(&a.value)})).collect())
}

fn insert(i: &Insert) -> Json {
    json!({
        "table": i.table,
        "columns": i.columns,
        "source": match &i.source {
            InsertSource::Values(rows) => json!({"kind": "values", "rows": rows.iter().map(|row| exprs(row)).collect::<Vec<_>>()}),
            InsertSource::Query(q) => json!({"kind": "query", "query": query(q)}),
        },
        "on_conflict": match &i.on_conflict {
            OnConflict::Error => json!({"kind": "error"}),
            OnConflict::DoNothing { target } => json!({"kind": "do_nothing", "target": target}),
            OnConflict::DoUpdate { target, set, filter } => json!({"kind": "do_update", "target": target, "set": assigns(set), "filter": opt_expr(filter)}),
        },
        "returning": selects(&i.returning),
    })
}

fn update(u: &Update) -> Json {
    json!({"rows": query(&u.rows), "set": assigns(&u.set), "returning": selects(&u.returning)})
}

fn delete(d: &Delete) -> Json {
    json!({"rows": query(&d.rows), "returning": selects(&d.returning)})
}

fn column(c: &ColumnDef) -> Json {
    json!({"name": c.name, "type": ty(&c.ty), "nullable": c.nullable, "default": opt_expr(&c.default), "auto_increment": c.auto_increment})
}

fn index(i: &IndexDef) -> Json {
    json!({
        "name": i.name,
        "keys": i.keys.iter().map(|k| json!({"expr": expr(&k.expr), "desc": k.desc, "each": k.each})).collect::<Vec<_>>(),
        "unique": i.unique,
        "filter": opt_expr(&i.filter),
        "index": match &i.kind {
            IndexKind::Ordered => json!({"kind": "ordered"}),
            IndexKind::FullText { config, fields } => json!({
                "kind": "full_text",
                "config": config.name(),
                "fields": fields.iter().map(|f| json!({"expr": expr(&f.expr), "weight_bits": format!("{:016x}", f.weight.to_bits())})).collect::<Vec<_>>(),
            }),
        },
    })
}

fn table(t: &TableDef) -> Json {
    json!({
        "name": t.name,
        "columns": t.columns.iter().map(column).collect::<Vec<_>>(),
        "primary_key": t.primary_key,
        "indexes": t.indexes.iter().map(index).collect::<Vec<_>>(),
        "foreign_keys": t.foreign_keys.iter().map(|fk| json!({
            "name": fk.name,
            "columns": fk.columns,
            "ref_table": fk.ref_table,
            "ref_columns": fk.ref_columns,
            "on_delete": match fk.on_delete { OnDelete::Restrict => "restrict", OnDelete::Cascade => "cascade", OnDelete::SetNull => "set_null" },
        })).collect::<Vec<_>>(),
        "checks": t.checks.iter().map(|c| json!({"name": c.name, "expr": expr(&c.expr)})).collect::<Vec<_>>(),
        "volatile": t.volatile,
    })
}

fn ddl(d: &Ddl) -> Json {
    match d {
        Ddl::CreateTable { def, if_not_exists } => json!({"kind": "create_table", "def": table(def), "if_not_exists": if_not_exists}),
        Ddl::DropTable { name, if_exists } => json!({"kind": "drop_table", "name": name, "if_exists": if_exists}),
        Ddl::AddColumn { table, column: c, if_not_exists } => json!({"kind": "add_column", "table": table, "column": column(c), "if_not_exists": if_not_exists}),
        Ddl::DropColumn { table, column, if_exists } => json!({"kind": "drop_column", "table": table, "column": column, "if_exists": if_exists}),
        Ddl::CreateIndex { table, index: i, if_not_exists } => json!({"kind": "create_index", "table": table, "index": index(i), "if_not_exists": if_not_exists}),
        Ddl::DropIndex { table, index, if_exists } => json!({"kind": "drop_index", "table": table, "index": index, "if_exists": if_exists}),
    }
}

fn admin(a: &Admin) -> Json {
    match a {
        Admin::CreateDatabase { name, password, if_not_exists } => json!({"kind": "create_database", "name": name, "password": password, "if_not_exists": if_not_exists}),
        Admin::DropDatabase { name, if_exists } => json!({"kind": "drop_database", "name": name, "if_exists": if_exists}),
        Admin::SetPassword { name, password } => json!({"kind": "set_password", "name": name, "password": password}),
        Admin::ListDatabases => json!({"kind": "list_databases"}),
        Admin::Checkpoint => json!({"kind": "checkpoint"}),
        Admin::Check => json!({"kind": "check"}),
        Admin::Stats => json!({"kind": "stats"}),
        Admin::Dump { databases } => json!({"kind": "dump", "databases": databases}),
        Admin::RestoreBegin { replace } => json!({"kind": "restore_begin", "replace": replace}),
        Admin::RestoreChunk(bytes) => json!({"kind": "restore_chunk", "hex": hex(bytes)}),
        Admin::RestoreEnd => json!({"kind": "restore_end"}),
        // A request about roles, of a later version: no vector here
        // holds one.
        other => unreachable!("{other:?} is not of the vectors' version"),
    }
}

fn request(r: &Request) -> Json {
    match r {
        Request::Hello { version, database, password, client } => json!({"kind": "hello", "version": version, "database": database, "password": password, "client": client}),
        Request::Ping => json!({"kind": "ping"}),
        Request::Begin { kind, lock_wait_ms } => json!({
            "kind": "begin",
            "txn": match kind { TxnKind::Read => "read", TxnKind::Exclusive => "exclusive", TxnKind::Optimistic => "optimistic" },
            "lock_wait_ms": lock_wait_ms.to_string(),
        }),
        Request::Commit => json!({"kind": "commit"}),
        Request::Rollback => json!({"kind": "rollback"}),
        Request::Savepoint(name) => json!({"kind": "savepoint", "name": name}),
        Request::RollbackTo(name) => json!({"kind": "rollback_to", "name": name}),
        Request::Release(name) => json!({"kind": "release", "name": name}),
        Request::Query(q) => json!({"kind": "query", "query": query(q)}),
        Request::Insert(i) => json!({"kind": "insert", "insert": insert(i)}),
        Request::Update(u) => json!({"kind": "update", "update": update(u)}),
        Request::Delete(d) => json!({"kind": "delete", "delete": delete(d)}),
        Request::Ddl(d) => json!({"kind": "ddl", "ddl": ddl(d)}),
        Request::Describe => json!({"kind": "describe"}),
        Request::Lock { name, wait_ms } => json!({"kind": "lock", "name": name, "wait_ms": wait_ms.to_string()}),
        Request::Unlock { name } => json!({"kind": "unlock", "name": name}),
        Request::Now => json!({"kind": "now"}),
        Request::Admin(a) => json!({"kind": "admin", "admin": admin(a)}),
        // A request of a later version: no vector here holds one.
        #[allow(unreachable_patterns)]
        _ => panic!("a request that version {VECTORS_VERSION} of the protocol does not have"),
    }
}

fn response(r: &Response) -> Json {
    match r {
        Response::Ok => json!({"kind": "ok"}),
        Response::Hello { version, server } => json!({"kind": "hello", "version": version, "server": server}),
        Response::Rows { set, more } => json!({
            "kind": "rows",
            "more": more,
            "columns": set.columns,
            "affected": set.affected.to_string(),
            "total": set.total.map(|n| n.to_string()),
            "rows": set.rows.iter().map(|row| row.iter().map(value).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
        Response::Tables(tables) => json!({"kind": "tables", "tables": tables.iter().map(table).collect::<Vec<_>>()}),
        Response::Flag(b) => json!({"kind": "flag", "value": b}),
        Response::Time(t) => json!({"kind": "time", "micros": t.to_string()}),
        Response::Names(names) => json!({"kind": "names", "names": names}),
        Response::Text(text) => json!({"kind": "text", "text": text}),
        Response::Chunk(bytes) => json!({"kind": "chunk", "hex": hex(bytes)}),
        Response::Error(e) => json!({
            "kind": "error",
            "code": e.code as u8,
            "code_name": named(&ERROR_CODES, e.code),
            "message": e.message,
            "constraint": e.constraint,
            "table": e.table,
        }),
        // A response of a later version: no vector here holds one.
        #[allow(unreachable_patterns)]
        _ => panic!("a response that version {VECTORS_VERSION} of the protocol does not have"),
    }
}

// -------------------------------------------------------------- the list

/// A named message: its description, its bytes, and whether encoding the
/// description gives those bytes (false for bytes only a decoder meets,
/// such as an error code this version does not know).
struct Vector {
    name: String,
    json: Json,
}

struct Vectors(Vec<Vector>);

impl Vectors {
    fn request(&mut self, name: &str, timeout_ms: u64, r: Request) {
        let bytes = encode_request(&r, timeout_ms);
        let (decoded, timeout) = decode_request(&bytes).unwrap_or_else(|e| panic!("{name} does not decode: {e}"));
        assert_eq!(timeout, timeout_ms, "{name}");
        // Compared as bytes and as descriptions: NaN is not equal to itself.
        assert_eq!(encode_request(&decoded, timeout), bytes, "{name} changes when decoded and encoded again");
        assert_eq!(request(&decoded), request(&r), "{name} decodes to something else");
        self.push(name, json!({"name": format!("request.{name}"), "kind": "request", "canonical": true, "timeout_ms": timeout_ms.to_string(), "message": request(&r), "hex": hex(&bytes)}));
    }

    fn response(&mut self, name: &str, r: Response) {
        let bytes = encode_response(&r);
        let decoded = decode_response(&bytes).unwrap_or_else(|e| panic!("{name} does not decode: {e}"));
        assert_eq!(encode_response(&decoded), bytes, "{name} changes when decoded and encoded again");
        assert_eq!(response(&decoded), response(&r), "{name} decodes to something else");
        self.push(name, json!({"name": format!("response.{name}"), "kind": "response", "canonical": true, "message": response(&r), "hex": hex(&bytes)}));
    }

    /// Bytes this crate never writes but must read.
    fn response_bytes(&mut self, name: &str, bytes: &[u8]) {
        let decoded = decode_response(bytes).unwrap_or_else(|e| panic!("{name} does not decode: {e}"));
        self.push(name, json!({"name": format!("response.{name}"), "kind": "response", "canonical": false, "message": response(&decoded), "hex": hex(bytes)}));
    }

    fn push(&mut self, name: &str, json: Json) {
        assert!(self.0.iter().all(|v| v.name != name || v.json["kind"] != json["kind"]), "two vectors are named {name}");
        self.0.push(Vector { name: name.to_string(), json });
    }
}

fn every_value() -> Vec<Value> {
    let mut values = vec![Value::Null, Value::Bool(true), Value::Bool(false)];
    // Where a zigzag varint grows by a byte, and both ends of 64 bits.
    for n in [0, 1, -1, 63, 64, -64, -65, 8191, 8192, -8193, 1 << 31, -(1 << 31), (1 << 53) - 1, 1 << 53, -(1 << 53), i64::MAX, i64::MIN] {
        values.push(Value::Int(n));
    }
    for f in [0.0, -0.0, 1.5, -0.5, 0.1, f64::MAX, f64::MIN_POSITIVE, f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        values.push(Value::Float(f));
    }
    for d in ["0", "-12345.67", "0.000001", "5.00", "123456789012345678.90"] {
        values.push(Value::Decimal(d.parse().unwrap()));
    }
    values.push(Value::Decimal(Decimal::new(i128::MAX, MAX_SCALE)));
    values.push(Value::Decimal(Decimal::new(i128::MIN + 1, 0)));
    values.push(Value::Text(String::new()));
    values.push(Value::Text("héllo, wörld: 日本語 🚀".into()));
    values.push(Value::Text("x".repeat(200)));
    values.push(Value::Bytes(Vec::new()));
    values.push(Value::Bytes(vec![0, 255, 7]));
    values.push(Value::Uuid([0; 16]));
    values.push(Value::Uuid([0x6f, 0x96, 0x19, 0xff, 0x8b, 0x86, 0xd0, 0x11, 0xb4, 0x2d, 0x00, 0xc0, 0x4f, 0xc9, 0x64, 0xff]));
    for t in [0, -5, 1_772_706_030_123_456, i64::MAX, i64::MIN] {
        values.push(Value::Timestamp(t));
    }
    for d in [0, -1, 20_517, i32::MAX, i32::MIN] {
        values.push(Value::Date(d));
    }
    for j in [json!(null), json!(true), json!(12), json!(-2.5), json!("text"), json!([]), json!({}), json!({"a": [1, 2.5, null, {"b": "c"}], "é": "ü\n\"\\"})] {
        values.push(Value::Json(j));
    }
    values.push(Value::List(Vec::new()));
    values.push(Value::List(vec![Value::Int(1), Value::Null, Value::List(vec![Value::Text("x".into())])]));
    values
}

fn sample_query() -> Query {
    Query::table("mails")
        .alias("m")
        .index("mails_user_date", KeyRange::within([val(Value::Uuid([1; 16]))], Bound::Unbounded, Bound::excluded([val(5)])))
        .reverse()
        .join(Query::table("map").alias("x").key([col("m.user_id"), val("INBOX"), col("m.mail_id")]))
        .left_join(Query::table("users").alias("u").key([col("m.user_id")]).limit(1))
        .each("address", col("to").json_pluck("address"))
        .filter(col("expunged").eq(false).and(col("subject").ilike("%hi%")).and(col("cc").is_null().not()))
        .filter(any([col("a").in_list(vec![1i64, 2]), exists(Query::table("t").key([col("m.id")]))]))
        .group_by([col("user_id")])
        .having(count().gt(1))
        .order_by(col("date").desc().nulls_last())
        .order_by(max(col("date")).filter(col("read").eq(false)))
        .offset(3)
        .limit(10)
        .select("n", count().distinct())
        .select("score", when(col("x").gt(1), col("x") * 2 + 1, null()).cast(Type::Float))
        .select("j", col("doc").json_text([val("a"), val(0)]))
        .select("s", scalar(Query::table("t").select("m", max(col("v")))))
        .distinct()
        .with_total()
}

fn sample_table() -> TableDef {
    TableDef::new("users")
        .column(ColumnDef::new("id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("email", Type::Text).nullable())
        .column(ColumnDef::new("n", Type::Int).auto_increment())
        .column(ColumnDef::new("amount", Type::decimal(15, 2)).default(0))
        .column(ColumnDef::new("ratio", Type::Decimal(None)).nullable())
        .column(ColumnDef::new("tags", Type::list(Type::Text)).default(Vec::<String>::new()))
        .column(ColumnDef::new("grid", Type::list(Type::list(Type::Int))).nullable())
        .column(ColumnDef::new("at", Type::Timestamp).default_now())
        .column(ColumnDef::new("day", Type::Date).nullable())
        .column(ColumnDef::new("flag", Type::Bool).default(false))
        .column(ColumnDef::new("score", Type::Float).default(1.5))
        .column(ColumnDef::new("blob", Type::Bytes).nullable())
        .column(ColumnDef::new("doc", Type::Json).nullable())
        .column(ColumnDef::new("account", Type::Uuid).nullable())
        .primary_key(["id"])
        .unique("users_n", ["n"])
        .index(IndexDef::new("users_email").on_expr(col("email").lower()).unique().filter(col("email").is_not_null()))
        .index(IndexDef::new("users_tags").on(["id"]).on_each(col("tags")).on_desc("at"))
        .index(IndexDef::new("users_text").on(["id"]).full_text(TextConfig::English, [(col("email"), 1.0), (col("doc").json_text(["bio"]), 0.4)]))
        .index(IndexDef::new("users_words").full_text(TextConfig::Simple, [(col("email"), 1.0)]))
        .foreign_key(["id"], "accounts", OnDelete::Cascade)
        .foreign_key(["account"], "accounts", OnDelete::SetNull)
        .foreign_key_to(["email"], "addresses", ["address"], OnDelete::Restrict)
        .check("positive", col("n").ge(0))
        .volatile()
}

fn all_vectors() -> Vectors {
    let mut v = Vectors(Vec::new());

    // ---- the connection and its transaction
    v.request("hello", 0, Request::Hello { version: VECTORS_VERSION, database: "shop".into(), password: "pw".into(), client: "test".into() });
    v.request("hello.admin", 0, Request::Hello { version: VECTORS_VERSION, database: String::new(), password: "admin-password".into(), client: String::new() });
    v.request("ping", 0, Request::Ping);
    v.request("ping.timeout_127", 127, Request::Ping);
    v.request("ping.timeout_128", 128, Request::Ping);
    v.request("ping.timeout_60000", 60_000, Request::Ping);
    v.request("ping.timeout_largest", u64::MAX, Request::Ping);
    v.request("begin.exclusive", 1500, Request::Begin { kind: TxnKind::Exclusive, lock_wait_ms: 30_000 });
    v.request("begin.optimistic", 1500, Request::Begin { kind: TxnKind::Optimistic, lock_wait_ms: 0 });
    v.request("begin.read", 1500, Request::Begin { kind: TxnKind::Read, lock_wait_ms: 0 });
    v.request("commit", 1500, Request::Commit);
    v.request("rollback", 1500, Request::Rollback);
    v.request("savepoint", 1500, Request::Savepoint("a".into()));
    v.request("rollback_to", 1500, Request::RollbackTo("a".into()));
    v.request("release", 1500, Request::Release("a".into()));
    v.request("describe", 1500, Request::Describe);
    v.request("lock", 1500, Request::Lock { name: "migrate".into(), wait_ms: 5000 });
    v.request("unlock", 1500, Request::Unlock { name: "migrate".into() });
    v.request("now", 1500, Request::Now);

    // ---- queries
    v.request("query.table", 60_000, Request::Query(Query::table("mails")));
    v.request("query.key", 60_000, Request::Query(Query::table("mails").key([val(7)])));
    v.request("query.keys", 60_000, Request::Query(Query::table("mails").keys([[val(1), val("a")], [val(2), val("b")]])));
    v.request("query.range", 60_000, Request::Query(Query::table("mails").range(KeyRange::new(Bound::excluded([val(10)]), Bound::Unbounded)).limit(50)));
    v.request("query.index", 60_000, Request::Query(Query::table("mails").index("mails_user_date", KeyRange::eq([val(Value::Uuid([1; 16]))])).limit(50)));
    v.request(
        "query.index_ranges",
        60_000,
        Request::Query(Query::table("mails").index_ranges(
            "mails_user_date",
            [KeyRange::eq([val(1)]), KeyRange::is([null()]), KeyRange::all(), KeyRange::new(Bound::included([val(1), val(2)]), Bound::excluded([val(3)])).nulls()],
        )),
    );
    v.request("query.sample", 1500, Request::Query(sample_query()));
    v.request("query.row", 60_000, Request::Query(Query::row().select("taken", exists(Query::table("t").key([val(1)]))).select("at", now())));
    v.request("query.columns", 60_000, Request::Query(Query::table("mails").alias("m").columns(["m.subject", "date"])));
    v.request("query.ancestors", 60_000, Request::Query(Query::table("pages").ancestors([[val(9)]], "parent_id")));
    v.request(
        "query.descendants",
        60_000,
        Request::Query(Query::table("pages").descendants([[val(1)], [val(2)]], "parent_id", "pages_parent").prune(col("deleted_at").is_null()).stop_at(col("id").eq(9)).max_depth(5).select("d", Expr::Depth)),
    );
    v.request("query.search", 60_000, Request::Query(Query::table("mails").search("mails_search", [val(1)], "hello world").match_prefix().order_by(Expr::Rank.desc())));
    v.request(
        "query.order",
        60_000,
        Request::Query(Query::table("t").order_by(col("a")).order_by(col("b").desc()).order_by(col("c").asc().nulls_first()).order_by(col("d").desc().nulls_last())),
    );
    let mut every = Query::row();
    for (i, value) in every_value().into_iter().enumerate() {
        every = every.select(&format!("v{i}"), val(value));
    }
    v.request("query.every_value", 60_000, Request::Query(every));
    let mut every = Query::row();
    for (func, name) in FUNCS {
        every = every.select(name, call(func, [col("a")]));
    }
    v.request("query.every_function", 60_000, Request::Query(every));
    let mut every = Query::table("t");
    for (op, name) in CMP_OPS {
        every = every.select(name, Expr::Cmp(op, Box::new(col("a")), Box::new(val(1))));
    }
    for (op, name) in ARITH_OPS {
        every = every.select(name, Expr::Arith(op, Box::new(col("a")), Box::new(val(2))));
    }
    for (func, name) in AGG_FUNCS {
        every = every.select(name, Expr::Agg { func, arg: Some(Box::new(col("a"))), filter: None, distinct: false });
    }
    v.request("query.every_operator", 60_000, Request::Query(every));
    v.request(
        "query.every_expression",
        60_000,
        Request::Query(
            Query::table("t")
                .select("value", val("x"))
                .select("column", col("a"))
                .select("qualified", col("t.a"))
                .select("excluded", excluded("a"))
                .select("old", old("a"))
                .select("depth", Expr::Depth)
                .select("rank", Expr::Rank)
                .select("not", col("a").not())
                .select("and", all([]))
                .select("or", any([col("a"), col("b"), col("c")]))
                .select("is_null", col("a").is_null())
                .select("is_not_null", col("a").is_not_null())
                .select("in", col("a").in_list(vec!["x", "y"]))
                .select("not_in", col("a").not_in_list(list_of(Query::table("u").columns(["a"]))))
                .select("like", col("a").like("a\\%%"))
                .select("not_like", col("a").not_like("_"))
                .select("ilike", col("a").ilike("%A%"))
                .select("neg", -col("a"))
                .select("case", Expr::Case(vec![(col("a").gt(1), val("many")), (col("a").eq(1), val("one"))], None))
                .select("when", when(col("a").is_null(), 0, col("a")))
                .select("cast", col("a").cast(Type::decimal(15, 2)))
                .select("try_cast", col("a").try_cast(Type::list(Type::Uuid)))
                .select("list", list([col("a"), val(1)]).lt(list([val(2), val(3)])))
                .select("count", count())
                .select("count_distinct", count().distinct().filter(col("a").gt(0)))
                .select("scalar", scalar(Query::table("u").key([col("t.a")]).columns(["b"])))
                .select("exists", exists(Query::table("u").key([col("t.a")])))
                .select("between", col("a").between(1, 9))
                .select("span", now() - minutes(5)),
        ),
    );

    // ---- writes
    v.request(
        "insert.upsert",
        1500,
        Request::Insert(
            Insert::into("t")
                .columns(["a", "b"])
                .values([val(1), now() + seconds(5)])
                .values([val(2), null()])
                .on_conflict_update(PRIMARY)
                .update("b", excluded("b"))
                .update_excluded(["a"])
                .update_where(col("b").lt(excluded("b")))
                .returning(["a"])
                .returning_expr("old_b", old("b")),
        ),
    );
    v.request("insert.set", 1500, Request::Insert(Insert::into("users").set("username", "ann").set("age", 30).returning(["id", "created"])));
    v.request("insert.from_query", 1500, Request::Insert(Insert::into("t").columns(["a"]).from_query(Query::table("s").columns(["a"])).on_conflict_do_nothing()));
    v.request("insert.do_nothing_on", 1500, Request::Insert(Insert::into("t").set("a", 1).on_conflict_do_nothing_on("t_a").returning_all()));
    v.request("insert.empty", 1500, Request::Insert(Insert::into("t")));
    v.request("update.rows", 1500, Request::Update(Update::rows(sample_query()).set("hits", col("hits") + 1).returning(["hits"]).returning_expr("was", old("hits"))));
    v.request(
        "update.key",
        1500,
        Request::Update(Update::table("auth_codes").key([val(Value::Bytes(vec![1, 2, 3]))]).filter(col("used_at").is_null()).set("used_at", now()).returning(["m.client_id"])),
    );
    v.request("delete.index", 1500, Request::Delete(Delete::from("t").index("t_a", KeyRange::eq([1])).filter(col("b").lt(now())).returning(["a"])));
    v.request("delete.key", 1500, Request::Delete(Delete::from("access").key([val("u"), val("svc")]).returning_all()));

    // ---- schema
    v.request("ddl.create_table", 1500, Request::Ddl(Ddl::CreateTable { def: sample_table(), if_not_exists: true }));
    v.request(
        "ddl.create_table.small",
        1500,
        Request::Ddl(Ddl::CreateTable { def: TableDef::new("t").column(ColumnDef::new("a", Type::Int)).primary_key(["a"]), if_not_exists: false }),
    );
    v.request("ddl.drop_table", 1500, Request::Ddl(Ddl::DropTable { name: "t".into(), if_exists: false }));
    v.request("ddl.add_column", 1500, Request::Ddl(Ddl::AddColumn { table: "t".into(), column: ColumnDef::new("c", Type::Json).nullable(), if_not_exists: true }));
    v.request("ddl.drop_column", 1500, Request::Ddl(Ddl::DropColumn { table: "t".into(), column: "c".into(), if_exists: true }));
    v.request("ddl.create_index", 1500, Request::Ddl(Ddl::CreateIndex { table: "t".into(), index: IndexDef::new("i").on(["a"]), if_not_exists: false }));
    v.request("ddl.drop_index", 1500, Request::Ddl(Ddl::DropIndex { table: "t".into(), index: "i".into(), if_exists: true }));

    // ---- administration
    v.request("admin.create_database", 0, Request::Admin(Admin::CreateDatabase { name: "d".into(), password: "p".into(), if_not_exists: true }));
    v.request("admin.drop_database", 0, Request::Admin(Admin::DropDatabase { name: "d".into(), if_exists: true }));
    v.request("admin.set_password", 0, Request::Admin(Admin::SetPassword { name: "d".into(), password: "q".into() }));
    v.request("admin.list_databases", 0, Request::Admin(Admin::ListDatabases));
    v.request("admin.checkpoint", 0, Request::Admin(Admin::Checkpoint));
    v.request("admin.check", 0, Request::Admin(Admin::Check));
    v.request("admin.stats", 0, Request::Admin(Admin::Stats));
    v.request("admin.dump", 0, Request::Admin(Admin::Dump { databases: vec!["a".into(), "b".into()] }));
    v.request("admin.dump.all", 0, Request::Admin(Admin::Dump { databases: Vec::new() }));
    v.request("admin.restore_begin", 0, Request::Admin(Admin::RestoreBegin { replace: true }));
    v.request("admin.restore_chunk", 0, Request::Admin(Admin::RestoreChunk(vec![1, 2, 3])));
    v.request("admin.restore_end", 0, Request::Admin(Admin::RestoreEnd));

    // ---- responses
    v.response("ok", Response::Ok);
    v.response("hello", Response::Hello { version: VECTORS_VERSION, server: "database 0.1.0".into() });
    let values = every_value();
    let width = 4;
    let rows: Vec<Vec<Value>> = values.chunks(width).map(|chunk| (0..width).map(|i| chunk.get(i).cloned().unwrap_or(Value::Null)).collect()).collect();
    let columns: Vec<String> = (0..width).map(|i| format!("c{i}")).collect();
    v.response("rows.every_value", Response::Rows { set: RowSet { columns, rows, affected: 9, total: Some(100) }, more: true });
    v.response("rows.empty", Response::Rows { set: RowSet::default(), more: false });
    v.response("rows.affected", Response::Rows { set: RowSet { columns: Vec::new(), rows: Vec::new(), affected: 3, total: None }, more: false });
    v.response("rows.no_columns", Response::Rows { set: RowSet { columns: Vec::new(), rows: vec![Vec::new(); 3], affected: 3, total: None }, more: false });
    v.response(
        "rows.one",
        Response::Rows { set: RowSet { columns: vec!["id".into(), "name".into()], rows: vec![vec![Value::Int(1), Value::Text("ann".into())]], affected: 1, total: None }, more: false },
    );
    v.response("tables", Response::Tables(vec![sample_table(), TableDef::new("t").column(ColumnDef::new("a", Type::Int)).primary_key(["a"])]));
    v.response("tables.none", Response::Tables(Vec::new()));
    v.response("flag.true", Response::Flag(true));
    v.response("flag.false", Response::Flag(false));
    v.response("time", Response::Time(1_700_000_000_000_000));
    v.response("time.before_1970", Response::Time(-1));
    v.response("names", Response::Names(vec!["a".into(), "b".into()]));
    v.response("text", Response::Text("ok".into()));
    v.response("chunk", Response::Chunk((0..=255).collect()));
    v.response("error.constraint", Response::Error(DbError::new(ErrorCode::UniqueViolation, "duplicate key").with_constraint("users", "users_email")));
    for (code, name) in ERROR_CODES {
        v.response(&format!("error.{name}"), Response::Error(DbError::new(code, format!("an error of kind {name}"))));
    }
    // A code this version does not know reads as `Internal`.
    v.response_bytes("error.unknown_code", &[10, 200, 1, b'x', 0, 0]);
    v.response_bytes("error.code_zero", &[10, 0, 0, 0, 0]);
    v
}

// ---------------------------------------------------------------- the file

fn tables() -> Json {
    let funcs: Map<String, Json> = FUNCS.iter().map(|(f, name)| (name.to_string(), json!(*f as u16))).collect();
    let errors: Map<String, Json> = ERROR_CODES.iter().map(|(c, name)| (name.to_string(), json!(*c as u8))).collect();
    let numbered = |pairs: Vec<(&str, u8)>| -> Json { Json::Object(pairs.into_iter().map(|(name, n)| (name.to_string(), json!(n))).collect()) };
    json!({
        "functions": funcs,
        "error_codes": errors,
        "comparisons": numbered(CMP_OPS.iter().map(|(op, name)| (*name, *op as u8)).collect()),
        "arithmetic": numbered(ARITH_OPS.iter().map(|(op, name)| (*name, *op as u8)).collect()),
        "aggregates": numbered(AGG_FUNCS.iter().map(|(f, name)| (*name, *f as u8)).collect()),
        "subqueries": numbered(SUBQUERY_KINDS.iter().map(|(k, name)| (*name, *k as u8)).collect()),
    })
}

/// The file's text: its head spread out, each vector on one line, so
/// that a change to one message is a change to one line.
fn render(vectors: &Vectors) -> String {
    let head = json!({
        "about": "Golden vectors of the wire protocol: see docs/PROTOCOL.md. Written by protocol/tests/vectors.rs; do not edit by hand.",
        "protocol_version": VECTORS_VERSION,
        "max_frame": MAX_FRAME,
        "rows_frame_target": ROWS_FRAME_TARGET,
        "max_nesting": 100,
        "max_json_nesting": MAX_JSON_DEPTH,
        "max_decimal_scale": MAX_SCALE,
        "tables": tables(),
    });
    let mut out = serde_json::to_string_pretty(&head).unwrap();
    assert_eq!(out.pop(), Some('}'));
    assert_eq!(out.pop(), Some('\n'));
    out.push_str(",\n  \"vectors\": [\n");
    for (i, vector) in vectors.0.iter().enumerate() {
        out.push_str("    ");
        out.push_str(&serde_json::to_string(&vector.json).unwrap());
        out.push_str(if i + 1 < vectors.0.len() { ",\n" } else { "\n" });
    }
    out.push_str("  ]\n}\n");
    out
}

#[test]
fn every_enum_has_a_name_in_the_tables() {
    // A function, an operator or an error code added to the crate without
    // a name here would be missing from the file and from the clients.
    let known: Vec<u16> = (0..=u16::MAX).filter(|n| Func::from_u16(*n).is_some()).collect();
    assert_eq!(known, FUNCS.iter().map(|(f, _)| *f as u16).collect::<Vec<_>>());
    assert_eq!((0..=u8::MAX).filter(|n| CmpOp::from_u8(*n).is_some()).count(), CMP_OPS.len());
    assert_eq!((0..=u8::MAX).filter(|n| ArithOp::from_u8(*n).is_some()).count(), ARITH_OPS.len());
    assert_eq!((0..=u8::MAX).filter(|n| AggFunc::from_u8(*n).is_some()).count(), AGG_FUNCS.len());
    assert_eq!((0..=u8::MAX).filter(|n| SubqueryKind::from_u8(*n).is_some()).count(), SUBQUERY_KINDS.len());
    // Up to this version's last code, every number but those of the
    // known codes reads as `Internal`. Codes above it are a later
    // version's, and not this file's to name.
    let mut codes: Vec<u8> = (0..=LAST_ERROR_CODE).filter(|n| ErrorCode::from_u8(*n) != ErrorCode::Internal).collect();
    codes.push(ErrorCode::Internal as u8);
    codes.sort_unstable();
    assert_eq!(codes, ERROR_CODES.iter().map(|(c, _)| *c as u8).collect::<Vec<_>>());
    for (code, _) in ERROR_CODES {
        assert_eq!(ErrorCode::from_u8(code as u8), code);
    }
    // A code no version has reads as `Internal`, never as a failure to decode.
    assert_eq!(ErrorCode::from_u8(0), ErrorCode::Internal);
    assert_eq!(ErrorCode::from_u8(200), ErrorCode::Internal);
}

#[test]
fn the_fixtures_are_what_this_crate_encodes() {
    let rendered = render(&all_vectors());
    if std::env::var("PROTOCOL_VECTORS").as_deref() == Ok("write") {
        std::fs::create_dir_all(std::path::Path::new(FIXTURES).parent().unwrap()).unwrap();
        std::fs::write(FIXTURES, &rendered).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(FIXTURES).unwrap_or_else(|e| panic!("cannot read {FIXTURES}: {e}"));
    if on_disk != rendered {
        let line = on_disk.lines().zip(rendered.lines()).position(|(a, b)| a != b).map_or(on_disk.lines().count().min(rendered.lines().count()), |n| n) + 1;
        panic!(
            "{FIXTURES} is not what this crate encodes (first difference on line {line}).\n\
             If the wire format was changed on purpose, update docs/PROTOCOL.md and the clients in clients/, then write the file again:\n\
             PROTOCOL_VECTORS=write cargo test -p database-protocol --test vectors"
        );
    }
}

#[test]
fn the_fixtures_decode_to_what_they_describe() {
    // Read back from the file, not from memory: this is what the other
    // clients are held to.
    let text = match std::fs::read_to_string(FIXTURES) {
        Ok(text) => text,
        Err(_) if std::env::var("PROTOCOL_VECTORS").as_deref() == Ok("write") => return,
        Err(e) => panic!("cannot read {FIXTURES}: {e}"),
    };
    let file: Json = serde_json::from_str(&text).unwrap();
    let vectors = file["vectors"].as_array().unwrap();
    assert!(vectors.len() > 100);
    for vector in vectors {
        let name = vector["name"].as_str().unwrap();
        let bytes = unhex(vector["hex"].as_str().unwrap());
        match vector["kind"].as_str().unwrap() {
            "request" => {
                let (decoded, timeout) = decode_request(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(request(&decoded), vector["message"], "{name}");
                assert_eq!(timeout.to_string(), vector["timeout_ms"].as_str().unwrap(), "{name}");
            }
            "response" => {
                let decoded = decode_response(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(response(&decoded), vector["message"], "{name}");
                if vector["canonical"] == json!(true) {
                    assert_eq!(encode_response(&decoded), bytes, "{name}");
                }
            }
            other => panic!("{name}: a vector of kind {other}"),
        }
    }
}

#[test]
fn what_the_specification_says_of_limits_holds() {
    // docs/PROTOCOL.md states these; a change here is a change there.
    assert_eq!(MAX_FRAME, 256 * 1024 * 1024);
    assert_eq!(ROWS_FRAME_TARGET, 4 * 1024 * 1024);
    assert_eq!(RESERVE, 1024);
    assert_eq!(MAX_SCALE, 30);
    assert_eq!(MAX_JSON_DEPTH, 100);

    // Nesting: 100 levels of expression decode, 101 do not.
    let nested = |depth: usize| {
        let mut e = col("a");
        for _ in 1..depth {
            e = e.not();
        }
        encode_request(&Request::Query(Query::table("t").filter(e)), 0)
    };
    // The query is one level, so 99 expressions inside it make 100.
    assert!(decode_request(&nested(99)).is_ok());
    assert!(decode_request(&nested(100)).is_err());

    // A varint is at most ten bytes, and the tenth holds one bit.
    let ping = |timeout: &[u8]| {
        let mut frame = timeout.to_vec();
        frame.push(2);
        decode_request(&frame)
    };
    assert_eq!(ping(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]).unwrap().1, u64::MAX);
    assert!(ping(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]).is_err());
    assert!(ping(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).is_err());
    // A longer spelling of a small number is read all the same.
    assert_eq!(ping(&[0x80, 0x00]).unwrap().1, 0);

    // A flag is 0 or 1; bytes after the message are an error.
    assert!(decode_response(&[5, 2]).is_err());
    assert!(decode_response(&[1, 0]).is_err());
    assert!(decode_request(&[0, 2, 0]).is_err());

    // The decimal no 128-bit number can negate, and a scale past 30, are refused.
    let decimal = |scale: u8, mantissa: i128| {
        let mut frame = vec![3, 0, 1, 1, b'c', 0, 0, 1, 5, scale];
        frame.extend_from_slice(&mantissa.to_le_bytes());
        decode_response(&frame)
    };
    assert!(decimal(30, i128::MAX).is_ok());
    assert!(decimal(31, 1).is_err());
    assert!(decimal(0, i128::MIN).is_err());
}
