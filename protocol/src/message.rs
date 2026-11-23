//! What travels between a client and the server.
//!
//! A connection carries frames: a four-byte little-endian length, then
//! that many bytes. The client's first frame is `Request::Hello`. After
//! that each request gets one response, in order, except that a dump
//! answers with `Chunk` frames until `Ok`, and a large result with several
//! `Rows` frames, all but the last marked `more`.

use crate::codec::*;
use crate::error::{DbError, DbResult, ErrorCode};
use crate::query::{Delete, Insert, Query, Update};
use crate::schema::{ColumnDef, IndexDef, TableDef};
use crate::value::Value;

/// Bumped when a frame's meaning changes. The server refuses a client
/// that speaks another version, in the answer to its `Hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest frame either side accepts. A row holding one 35 MiB mail
/// body must fit.
pub const MAX_FRAME: usize = 256 * 1024 * 1024;

/// A result is cut into frames of about this size.
pub const ROWS_FRAME_TARGET: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// `database` may be empty for a connection that only administers.
    Hello { version: u32, database: String, password: String, client: String },
    Ping,
    /// Starts a transaction. A writing one waits for the database's write
    /// lock, for at most `lock_wait_ms`.
    Begin { write: bool, lock_wait_ms: u64 },
    Commit,
    Rollback,
    Savepoint(String),
    RollbackTo(String),
    Release(String),
    Query(Query),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    Ddl(Ddl),
    /// Every table's definition.
    Describe,
    /// Takes a named lock for this connection, shared by nobody: waits up
    /// to `wait_ms`, answers whether it was taken.
    Lock { name: String, wait_ms: u64 },
    Unlock { name: String },
    /// The server's clock.
    Now,
    Admin(Admin),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ddl {
    CreateTable { def: TableDef, if_not_exists: bool },
    DropTable { name: String, if_exists: bool },
    AddColumn { table: String, column: ColumnDef, if_not_exists: bool },
    DropColumn { table: String, column: String, if_exists: bool },
    CreateIndex { table: String, index: IndexDef, if_not_exists: bool },
    DropIndex { table: String, index: String, if_exists: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Admin {
    CreateDatabase { name: String, password: String, if_not_exists: bool },
    DropDatabase { name: String, if_exists: bool },
    SetPassword { name: String, password: String },
    ListDatabases,
    /// Writes everything in memory to the data file and trims the log.
    Checkpoint,
    /// Reads every page of the connection's database and checks that the
    /// trees, the indexes and the free list agree. Answers with a report.
    Check,
    /// Counters, as text.
    Stats,
    /// Streams a dump of the named databases, or of all when none is
    /// named, as `Chunk` frames.
    Dump { databases: Vec<String> },
    /// Starts loading a dump. `Chunk` requests follow, then `RestoreEnd`.
    /// With `replace`, databases in the dump replace existing ones of the
    /// same name; without it they must not exist.
    RestoreBegin { replace: bool },
    RestoreChunk(Vec<u8>),
    RestoreEnd,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RowSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Rows inserted, updated or deleted by a write; rows returned by a
    /// read.
    pub affected: u64,
    /// For a query `with_total`: rows before offset and limit.
    pub total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Ok,
    Hello { version: u32, server: String },
    Rows { set: RowSet, more: bool },
    Tables(Vec<TableDef>),
    Flag(bool),
    Time(i64),
    Names(Vec<String>),
    Text(String),
    Chunk(Vec<u8>),
    Error(DbError),
}

// ---------------------------------------------------------------- encoding

/// A request and how long the server may spend on it, in milliseconds
/// (0 for no limit beyond the server's own).
pub fn encode_request(request: &Request, timeout_ms: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(timeout_ms);
    match request {
        Request::Hello { version, database, password, client } => {
            w.u8(1);
            w.u32(*version);
            w.str(database);
            w.str(password);
            w.str(client);
        }
        Request::Ping => w.u8(2),
        Request::Begin { write, lock_wait_ms } => {
            w.u8(3);
            w.bool(*write);
            w.varint(*lock_wait_ms);
        }
        Request::Commit => w.u8(4),
        Request::Rollback => w.u8(5),
        Request::Savepoint(name) => {
            w.u8(6);
            w.str(name);
        }
        Request::RollbackTo(name) => {
            w.u8(7);
            w.str(name);
        }
        Request::Release(name) => {
            w.u8(8);
            w.str(name);
        }
        Request::Query(q) => {
            w.u8(9);
            put_query(&mut w, q);
        }
        Request::Insert(i) => {
            w.u8(10);
            put_insert(&mut w, i);
        }
        Request::Update(u) => {
            w.u8(11);
            put_update(&mut w, u);
        }
        Request::Delete(d) => {
            w.u8(12);
            put_delete(&mut w, d);
        }
        Request::Ddl(ddl) => {
            w.u8(13);
            put_ddl(&mut w, ddl);
        }
        Request::Describe => w.u8(14),
        Request::Lock { name, wait_ms } => {
            w.u8(15);
            w.str(name);
            w.varint(*wait_ms);
        }
        Request::Unlock { name } => {
            w.u8(16);
            w.str(name);
        }
        Request::Now => w.u8(17),
        Request::Admin(admin) => {
            w.u8(18);
            put_admin(&mut w, admin);
        }
    }
    w.buf
}

pub fn decode_request(frame: &[u8]) -> DbResult<(Request, u64)> {
    let mut r = Reader::new(frame);
    let timeout_ms = r.varint()?;
    let request = match r.u8()? {
        1 => Request::Hello { version: r.u32()?, database: r.str()?, password: r.str()?, client: r.str()? },
        2 => Request::Ping,
        3 => Request::Begin { write: r.bool()?, lock_wait_ms: r.varint()? },
        4 => Request::Commit,
        5 => Request::Rollback,
        6 => Request::Savepoint(r.str()?),
        7 => Request::RollbackTo(r.str()?),
        8 => Request::Release(r.str()?),
        9 => Request::Query(get_query(&mut r)?),
        10 => Request::Insert(get_insert(&mut r)?),
        11 => Request::Update(get_update(&mut r)?),
        12 => Request::Delete(get_delete(&mut r)?),
        13 => Request::Ddl(get_ddl(&mut r)?),
        14 => Request::Describe,
        15 => Request::Lock { name: r.str()?, wait_ms: r.varint()? },
        16 => Request::Unlock { name: r.str()? },
        17 => Request::Now,
        18 => Request::Admin(get_admin(&mut r)?),
        _ => return Err(DbError::new(ErrorCode::InvalidRequest, "unknown request")),
    };
    r.finish()?;
    Ok((request, timeout_ms))
}

fn put_ddl(w: &mut Writer, ddl: &Ddl) {
    match ddl {
        Ddl::CreateTable { def, if_not_exists } => {
            w.u8(1);
            put_table(w, def);
            w.bool(*if_not_exists);
        }
        Ddl::DropTable { name, if_exists } => {
            w.u8(2);
            w.str(name);
            w.bool(*if_exists);
        }
        Ddl::AddColumn { table, column, if_not_exists } => {
            w.u8(3);
            w.str(table);
            put_column(w, column);
            w.bool(*if_not_exists);
        }
        Ddl::DropColumn { table, column, if_exists } => {
            w.u8(4);
            w.str(table);
            w.str(column);
            w.bool(*if_exists);
        }
        Ddl::CreateIndex { table, index, if_not_exists } => {
            w.u8(5);
            w.str(table);
            put_index(w, index);
            w.bool(*if_not_exists);
        }
        Ddl::DropIndex { table, index, if_exists } => {
            w.u8(6);
            w.str(table);
            w.str(index);
            w.bool(*if_exists);
        }
    }
}

fn get_ddl(r: &mut Reader) -> DbResult<Ddl> {
    Ok(match r.u8()? {
        1 => Ddl::CreateTable { def: get_table(r)?, if_not_exists: r.bool()? },
        2 => Ddl::DropTable { name: r.str()?, if_exists: r.bool()? },
        3 => Ddl::AddColumn { table: r.str()?, column: get_column(r)?, if_not_exists: r.bool()? },
        4 => Ddl::DropColumn { table: r.str()?, column: r.str()?, if_exists: r.bool()? },
        5 => Ddl::CreateIndex { table: r.str()?, index: get_index(r)?, if_not_exists: r.bool()? },
        6 => Ddl::DropIndex { table: r.str()?, index: r.str()?, if_exists: r.bool()? },
        _ => return Err(DbError::new(ErrorCode::InvalidRequest, "unknown schema change")),
    })
}

fn put_admin(w: &mut Writer, admin: &Admin) {
    match admin {
        Admin::CreateDatabase { name, password, if_not_exists } => {
            w.u8(1);
            w.str(name);
            w.str(password);
            w.bool(*if_not_exists);
        }
        Admin::DropDatabase { name, if_exists } => {
            w.u8(2);
            w.str(name);
            w.bool(*if_exists);
        }
        Admin::SetPassword { name, password } => {
            w.u8(3);
            w.str(name);
            w.str(password);
        }
        Admin::ListDatabases => w.u8(4),
        Admin::Checkpoint => w.u8(5),
        Admin::Check => w.u8(6),
        Admin::Stats => w.u8(7),
        Admin::Dump { databases } => {
            w.u8(8);
            w.strs(databases);
        }
        Admin::RestoreBegin { replace } => {
            w.u8(9);
            w.bool(*replace);
        }
        Admin::RestoreChunk(bytes) => {
            w.u8(10);
            w.bytes(bytes);
        }
        Admin::RestoreEnd => w.u8(11),
    }
}

fn get_admin(r: &mut Reader) -> DbResult<Admin> {
    Ok(match r.u8()? {
        1 => Admin::CreateDatabase { name: r.str()?, password: r.str()?, if_not_exists: r.bool()? },
        2 => Admin::DropDatabase { name: r.str()?, if_exists: r.bool()? },
        3 => Admin::SetPassword { name: r.str()?, password: r.str()? },
        4 => Admin::ListDatabases,
        5 => Admin::Checkpoint,
        6 => Admin::Check,
        7 => Admin::Stats,
        8 => Admin::Dump { databases: r.strs()? },
        9 => Admin::RestoreBegin { replace: r.bool()? },
        10 => Admin::RestoreChunk(r.bytes()?.to_vec()),
        11 => Admin::RestoreEnd,
        _ => return Err(DbError::new(ErrorCode::InvalidRequest, "unknown admin request")),
    })
}

pub fn encode_response(response: &Response) -> Vec<u8> {
    let mut w = Writer::new();
    match response {
        Response::Ok => w.u8(1),
        Response::Hello { version, server } => {
            w.u8(2);
            w.u32(*version);
            w.str(server);
        }
        Response::Rows { set, more } => {
            w.u8(3);
            w.bool(*more);
            w.strs(&set.columns);
            w.varint(set.affected);
            match set.total {
                Some(n) => {
                    w.u8(1);
                    w.varint(n);
                }
                None => w.u8(0),
            }
            w.varint(set.rows.len() as u64);
            for row in &set.rows {
                for value in row {
                    put_value(&mut w, value);
                }
            }
        }
        Response::Tables(tables) => {
            w.u8(4);
            w.varint(tables.len() as u64);
            for t in tables {
                put_table(&mut w, t);
            }
        }
        Response::Flag(b) => {
            w.u8(5);
            w.bool(*b);
        }
        Response::Time(t) => {
            w.u8(6);
            w.zigzag(*t);
        }
        Response::Names(names) => {
            w.u8(7);
            w.strs(names);
        }
        Response::Text(text) => {
            w.u8(8);
            w.str(text);
        }
        Response::Chunk(bytes) => {
            w.u8(9);
            w.bytes(bytes);
        }
        Response::Error(e) => {
            w.u8(10);
            put_error(&mut w, e);
        }
    }
    w.buf
}

pub fn decode_response(frame: &[u8]) -> DbResult<Response> {
    let mut r = Reader::new(frame);
    let response = match r.u8()? {
        1 => Response::Ok,
        2 => Response::Hello { version: r.u32()?, server: r.str()? },
        3 => {
            let more = r.bool()?;
            let columns = r.strs()?;
            let affected = r.varint()?;
            let total = if r.bool()? { Some(r.varint()?) } else { None };
            let n = r.varint()? as usize;
            // A row of no columns takes no bytes, so the count cannot be
            // checked against the frame; cap what is reserved up front.
            let mut rows = Vec::with_capacity(n.min(r.remaining()));
            for _ in 0..n {
                let mut row = Vec::with_capacity(columns.len());
                for _ in 0..columns.len() {
                    row.push(get_value(&mut r)?);
                }
                rows.push(row);
                if columns.is_empty() && rows.len() > 1_000_000 {
                    return Err(DbError::new(ErrorCode::InvalidRequest, "too many empty rows"));
                }
            }
            Response::Rows { set: RowSet { columns, rows, affected, total }, more }
        }
        4 => {
            let n = r.count()?;
            Response::Tables((0..n).map(|_| get_table(&mut r)).collect::<DbResult<_>>()?)
        }
        5 => Response::Flag(r.bool()?),
        6 => Response::Time(r.zigzag()?),
        7 => Response::Names(r.strs()?),
        8 => Response::Text(r.str()?),
        9 => Response::Chunk(r.bytes()?.to_vec()),
        10 => Response::Error(get_error(&mut r)?),
        _ => return Err(DbError::new(ErrorCode::Connection, "unknown response")),
    };
    r.finish()?;
    Ok(response)
}

/// Cuts a result into responses of about `ROWS_FRAME_TARGET` bytes.
pub fn rows_responses(set: RowSet) -> Vec<Response> {
    let RowSet { columns, rows, affected, total } = set;
    let mut out = Vec::new();
    let mut batch = Vec::new();
    let mut size = 0usize;
    for row in rows {
        size += row.iter().map(approx_size).sum::<usize>();
        batch.push(row);
        if size >= ROWS_FRAME_TARGET {
            let set = RowSet { columns: columns.clone(), rows: std::mem::take(&mut batch), affected, total };
            out.push(Response::Rows { set, more: true });
            size = 0;
        }
    }
    out.push(Response::Rows { set: RowSet { columns, rows: batch, affected, total }, more: false });
    out
}

fn approx_size(v: &Value) -> usize {
    match v {
        Value::Text(s) => s.len() + 4,
        Value::Bytes(b) => b.len() + 4,
        Value::Json(j) => match j {
            serde_json::Value::String(s) => s.len() + 4,
            serde_json::Value::Array(a) => 16 * a.len() + 16,
            serde_json::Value::Object(o) => 32 * o.len() + 16,
            _ => 9,
        },
        Value::List(items) => items.iter().map(approx_size).sum::<usize>() + 4,
        _ => 9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::*;
    use crate::query::*;
    use crate::schema::*;
    use crate::value::Type;
    use serde_json::json;

    fn sample_values() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(0),
            Value::Int(-1),
            Value::Int(i64::MAX),
            Value::Int(i64::MIN),
            Value::Float(-0.5),
            Value::Float(f64::INFINITY),
            Value::Decimal("-12345.67".parse().unwrap()),
            Value::Text("héllo".into()),
            Value::Text(String::new()),
            Value::Bytes(vec![0, 255, 7]),
            Value::Uuid([9; 16]),
            Value::Timestamp(-5),
            Value::Date(20_000),
            Value::Json(json!({"a": [1, 2.5, null, {"b": "c"}]})),
            Value::List(vec![Value::Int(1), Value::Null, Value::List(vec![Value::Text("x".into())])]),
        ]
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

    #[test]
    fn requests_round_trip() {
        let table = TableDef::new("users")
            .column(ColumnDef::new("id", Type::Uuid).default_random_uuid())
            .column(ColumnDef::new("email", Type::Text).nullable())
            .column(ColumnDef::new("n", Type::Int).auto_increment())
            .column(ColumnDef::new("amount", Type::decimal(15, 2)).default(0))
            .column(ColumnDef::new("tags", Type::list(Type::Text)).default(Vec::<String>::new()))
            .column(ColumnDef::new("at", Type::Timestamp).default_now())
            .primary_key(["id"])
            .unique("users_n", ["n"])
            .index(IndexDef::new("users_email").on_expr(col("email").lower()).unique().filter(col("email").is_not_null()))
            .index(IndexDef::new("users_tags").on(["id"]).on_each(col("tags")).on_desc("at"))
            .index(IndexDef::new("users_text").on(["id"]).full_text(TextConfig::English, [(col("email"), 1.0)]))
            .foreign_key(["id"], "accounts", OnDelete::Cascade)
            .check("positive", col("n").ge(0))
            .volatile();
        let requests = vec![
            Request::Hello { version: PROTOCOL_VERSION, database: "mail".into(), password: "pw".into(), client: "test".into() },
            Request::Ping,
            Request::Begin { write: true, lock_wait_ms: 30_000 },
            Request::Commit,
            Request::Rollback,
            Request::Savepoint("a".into()),
            Request::RollbackTo("a".into()),
            Request::Release("a".into()),
            Request::Query(sample_query()),
            Request::Query(Query::table("pages").descendants([[val(1)]], "parent_id", "pages_parent").prune(col("deleted_at").is_null()).stop_at(col("id").eq(9)).max_depth(5).select("d", Expr::Depth)),
            Request::Query(Query::table("mails").search("mails_search", [val(1)], "hello world").match_prefix().order_by(Expr::Rank.desc())),
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
            Request::Insert(Insert::into("t").columns(["a"]).from_query(Query::table("s").columns(["a"])).on_conflict_do_nothing()),
            Request::Insert(Insert::into("t").set("a", 1).on_conflict_do_nothing_on("t_a")),
            Request::Update(Update::rows(sample_query()).set("hits", col("hits") + 1).returning(["hits"]).returning_expr("was", old("hits"))),
            Request::Delete(Delete::from("t").index("t_a", KeyRange::eq([1])).filter(col("b").lt(now())).returning(["a"])),
            Request::Ddl(Ddl::CreateTable { def: table.clone(), if_not_exists: true }),
            Request::Ddl(Ddl::DropTable { name: "t".into(), if_exists: false }),
            Request::Ddl(Ddl::AddColumn { table: "t".into(), column: ColumnDef::new("c", Type::Json).nullable(), if_not_exists: true }),
            Request::Ddl(Ddl::DropColumn { table: "t".into(), column: "c".into(), if_exists: true }),
            Request::Ddl(Ddl::CreateIndex { table: "t".into(), index: IndexDef::new("i").on(["a"]), if_not_exists: false }),
            Request::Ddl(Ddl::DropIndex { table: "t".into(), index: "i".into(), if_exists: true }),
            Request::Describe,
            Request::Lock { name: "migrate".into(), wait_ms: 0 },
            Request::Unlock { name: "migrate".into() },
            Request::Now,
            Request::Admin(Admin::CreateDatabase { name: "d".into(), password: "p".into(), if_not_exists: true }),
            Request::Admin(Admin::DropDatabase { name: "d".into(), if_exists: true }),
            Request::Admin(Admin::SetPassword { name: "d".into(), password: "q".into() }),
            Request::Admin(Admin::ListDatabases),
            Request::Admin(Admin::Checkpoint),
            Request::Admin(Admin::Check),
            Request::Admin(Admin::Stats),
            Request::Admin(Admin::Dump { databases: vec!["a".into()] }),
            Request::Admin(Admin::RestoreBegin { replace: true }),
            Request::Admin(Admin::RestoreChunk(vec![1, 2, 3])),
            Request::Admin(Admin::RestoreEnd),
        ];
        for request in requests {
            let frame = encode_request(&request, 1500);
            assert_eq!(decode_request(&frame).unwrap(), (request, 1500));
        }
    }

    #[test]
    fn responses_round_trip() {
        let set = RowSet { columns: vec!["a".into(), "b".into()], rows: sample_values().chunks(2).map(<[Value]>::to_vec).collect(), affected: 9, total: Some(100) };
        let responses = vec![
            Response::Ok,
            Response::Hello { version: 1, server: "database 0.1.0".into() },
            Response::Rows { set, more: true },
            Response::Rows { set: RowSet::default(), more: false },
            Response::Tables(vec![TableDef::new("t").column(ColumnDef::new("a", Type::Int)).primary_key(["a"])]),
            Response::Flag(true),
            Response::Time(1_700_000_000_000_000),
            Response::Names(vec!["a".into(), "b".into()]),
            Response::Text("ok".into()),
            Response::Chunk(vec![0; 1000]),
            Response::Error(DbError::new(ErrorCode::UniqueViolation, "duplicate key").with_constraint("users", "users_email")),
        ];
        for response in responses {
            // NaN aside, what is decoded equals what was encoded.
            assert_eq!(decode_response(&encode_response(&response)).unwrap(), response);
        }
    }

    #[test]
    fn a_damaged_frame_is_an_error_never_a_panic() {
        let frame = encode_request(&Request::Query(sample_query()), 0);
        for cut in 0..frame.len() {
            assert!(decode_request(&frame[..cut]).is_err(), "cut at {cut}");
        }
        let mut rng = 0x1234_5678_9abc_def0u64;
        for _ in 0..20_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let mut bad = frame.clone();
            let at = (rng % bad.len() as u64) as usize;
            bad[at] ^= (rng >> 32) as u8 | 1;
            let _ = decode_request(&bad);
            let _ = decode_response(&bad);
        }
    }

    #[test]
    fn nesting_beyond_the_limit_is_refused() {
        let mut e = col("a");
        for _ in 0..300 {
            e = e.not();
        }
        let frame = encode_request(&Request::Query(Query::table("t").filter(e)), 0);
        assert!(decode_request(&frame).is_err());
    }

    #[test]
    fn large_results_are_cut_into_frames() {
        let rows: Vec<Vec<Value>> = (0..100).map(|_| vec![Value::Text("x".repeat(200_000))]).collect();
        let responses = rows_responses(RowSet { columns: vec!["body".into()], rows, affected: 100, total: None });
        assert!(responses.len() > 3);
        let mut seen = 0;
        for (i, response) in responses.iter().enumerate() {
            let Response::Rows { set, more } = response else { panic!() };
            assert_eq!(*more, i + 1 < responses.len());
            seen += set.rows.len();
        }
        assert_eq!(seen, 100);
    }
}
