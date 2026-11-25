//! Roles: a name and a password within a database, with rights on its
//! tables. Each right is proved the same way: a role that lacks only
//! that right is refused every statement that needs it, by every way a
//! statement can be sent, and nothing is changed; a role that holds it
//! is answered. R-A1 to R-A8.

mod common;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use common::*;
use database_protocol::message::{decode_response, encode_request, Admin, BatchItem, Ddl, Request, Response, TxnKind, PROTOCOL_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The password every role of these tests is given.
const ROLE_PASSWORD: &str = "role-password";

// ------------------------------------------------------------ the wire

/// One connection, spoken to in requests: so that a test says exactly
/// which request carries a statement.
struct Wire {
    stream: tokio::net::TcpStream,
}

impl Wire {
    /// Greets as `database` (a database, or `database/role`) with a
    /// client of protocol `version`.
    async fn greet(server: &TestServer, database: &str, password: &str, version: u32) -> DbResult<Wire> {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
        let mut wire = Wire { stream };
        match wire.call(Request::Hello { version, database: database.into(), password: password.into(), client: "wire".into() }).await {
            Response::Hello { .. } => Ok(wire),
            Response::Error(e) => Err(e),
            other => panic!("a greeting was answered {other:?}"),
        }
    }

    async fn role(server: &TestServer, role: &str) -> Wire {
        Wire::greet(server, &format!("test/{role}"), ROLE_PASSWORD, PROTOCOL_VERSION).await.unwrap_or_else(|e| panic!("role {role} connects: {e}"))
    }

    async fn send(&mut self, request: &Request) {
        let frame = encode_request(request, 10_000);
        self.stream.write_all(&(frame.len() as u32).to_le_bytes()).await.unwrap();
        self.stream.write_all(&frame).await.unwrap();
    }

    async fn receive(&mut self) -> Response {
        loop {
            let mut len = [0u8; 4];
            self.stream.read_exact(&mut len).await.unwrap();
            let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
            self.stream.read_exact(&mut frame).await.unwrap();
            match decode_response(&frame).unwrap() {
                Response::Rows { more: true, .. } => continue,
                response => return response,
            }
        }
    }

    async fn call(&mut self, request: Request) -> Response {
        self.send(&request).await;
        self.receive().await
    }

    /// A request's answer as success or the error it was.
    async fn ask(&mut self, request: Request) -> DbResult<Response> {
        match self.call(request).await {
            Response::Error(e) => Err(e),
            response => Ok(response),
        }
    }
}

/// A way a statement reaches the server.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Way {
    /// As a request of its own.
    Alone,
    /// As a prepared statement, sent with its number.
    Prepared,
    /// As the one statement of a batch.
    Batch,
    /// With the begin of a transaction, which is then committed.
    BeginWith(TxnKind),
    /// In a transaction begun before it, which is then committed.
    Within(TxnKind),
    /// To be explained: queries only.
    Explain,
    /// Prepared, and to be explained.
    ExplainPrepared,
}

fn is_query(request: &Request) -> bool {
    matches!(request, Request::Query(_))
}

/// The ways a statement can be sent: a schema change only on its own or
/// in a transaction that may write, a write not in a reading transaction,
/// and only a query to be explained.
fn ways(request: &Request) -> Vec<Way> {
    let writing = [TxnKind::Optimistic, TxnKind::Exclusive];
    let mut ways = vec![Way::Alone];
    ways.extend(writing.map(Way::BeginWith));
    ways.extend(writing.map(Way::Within));
    if !matches!(request, Request::Ddl(_)) {
        ways.extend([Way::Prepared, Way::Batch]);
    }
    if is_query(request) {
        ways.extend([Way::BeginWith(TxnKind::Read), Way::Within(TxnKind::Read), Way::Explain, Way::ExplainPrepared]);
    }
    ways
}

static NEXT: AtomicI64 = AtomicI64::new(1_000);

/// A number no other statement of the test run has used: a key for a row
/// to insert, a statement's number on its connection.
fn fresh() -> i64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Sends `statement` the given way on a new connection of `role` and
/// returns how it went: the statement's own error, or the commit's.
async fn send(server: &TestServer, role: &str, way: Way, statement: Request) -> DbResult<()> {
    let mut wire = Wire::role(server, role).await;
    let prepared = |statement: Request| Request::Prepared { id: fresh() as u64, statement: Some(std::sync::Arc::new(statement)), params: Vec::new() };
    let ended = |answer: DbResult<Response>| answer.map(|_| ());
    match way {
        Way::Alone => ended(wire.ask(statement).await),
        Way::Prepared => ended(wire.ask(prepared(statement)).await),
        Way::Batch => ended(wire.ask(Request::Batch(vec![BatchItem { statement, expect: None }])).await),
        Way::Explain => ended(wire.ask(Request::Explain(Box::new(statement))).await),
        Way::ExplainPrepared => ended(wire.ask(Request::Explain(Box::new(prepared(statement)))).await),
        Way::BeginWith(kind) | Way::Within(kind) => {
            let ran = if matches!(way, Way::BeginWith(_)) {
                wire.ask(Request::BeginWith { kind, lock_wait_ms: 5_000, statement: Box::new(statement) }).await
            } else {
                wire.ask(Request::Begin { kind, lock_wait_ms: 5_000 }).await.expect("a transaction begins");
                wire.ask(statement).await
            };
            match ran {
                Ok(_) => ended(wire.ask(Request::Commit).await),
                Err(e) => {
                    // Whatever the commit of a failed transaction says,
                    // the statement's error is the answer.
                    let _ = wire.ask(Request::Commit).await;
                    Err(e)
                }
            }
        }
    }
}

// ------------------------------------------------------------ the data

/// The tables the tests are about, and what points at what:
///
/// ```text
/// customers  <- orders (cascade)   <- lines (cascade)
///                                  <- notes (set null)
///            <- invoices (restrict)
/// audit, docs (with a full-text index): on their own
/// ```
const TABLES: [&str; 7] = ["customers", "orders", "lines", "notes", "invoices", "audit", "docs"];

async fn tables(db: &Client) -> DbResult<()> {
    let id = || ColumnDef::new("id", Type::Int);
    db.create_table(TableDef::new("customers").column(id()).column(ColumnDef::new("name", Type::Text)).column(ColumnDef::new("code", Type::Int).nullable()).primary_key(["id"]).unique("customers_code", ["code"])).await?;
    db.create_table(TableDef::new("orders").column(id()).column(ColumnDef::new("customer_id", Type::Int).nullable()).column(ColumnDef::new("total", Type::Int).default(0)).primary_key(["id"]).foreign_key(["customer_id"], "customers", OnDelete::Cascade)).await?;
    db.create_table(TableDef::new("lines").column(id()).column(ColumnDef::new("order_id", Type::Int)).primary_key(["id"]).foreign_key(["order_id"], "orders", OnDelete::Cascade)).await?;
    db.create_table(TableDef::new("notes").column(id()).column(ColumnDef::new("order_id", Type::Int).nullable()).primary_key(["id"]).foreign_key(["order_id"], "orders", OnDelete::SetNull)).await?;
    db.create_table(TableDef::new("invoices").column(id()).column(ColumnDef::new("customer_id", Type::Int)).primary_key(["id"]).foreign_key(["customer_id"], "customers", OnDelete::Restrict)).await?;
    db.create_table(TableDef::new("audit").column(id()).column(ColumnDef::new("what", Type::Text).default("")).column(ColumnDef::new("times", Type::Int).default(1)).primary_key(["id"])).await?;
    db.create_table(TableDef::new("docs").column(id()).column(ColumnDef::new("body", Type::Text)).primary_key(["id"]).index(IndexDef::new("docs_search").on(["id"]).full_text(TextConfig::English, [(col("body"), 1.0)]))).await?;
    // Rows for the statements to find: customers 1 to 9, each with an
    // order of the same number, which has a line and a note.
    for n in 1..10 {
        db.insert(Insert::into("customers").set("id", n).set("name", format!("customer {n}")).set("code", n)).await?;
        db.insert(Insert::into("orders").set("id", n).set("customer_id", n)).await?;
        db.insert(Insert::into("lines").set("id", n).set("order_id", n)).await?;
        db.insert(Insert::into("notes").set("id", n).set("order_id", n)).await?;
        db.insert(Insert::into("audit").set("id", n).set("what", "seen")).await?;
    }
    Ok(())
}

/// Every row of every table, to compare before and after.
async fn everything(db: &Client) -> DbResult<Vec<(String, Vec<Vec<Value>>)>> {
    let mut all = Vec::new();
    for table in db.describe().await? {
        let rows = db.query(Query::table(&table.name)).await?;
        all.push((format!("{table:?}"), rows.iter().map(|row| row.values().to_vec()).collect()));
    }
    Ok(all)
}

/// A server with the tables, and the owner's client on them.
async fn shop() -> DbResult<(TestServer, Client)> {
    let (server, owner) = fresh_server().await;
    tables(&owner).await?;
    Ok((server, owner))
}

async fn fresh_server() -> (TestServer, Client) {
    common::fresh().await
}

/// Makes a role that holds every right on every table, each granted on
/// the table itself, but for `lacking` on the tables of `on`.
async fn role_lacking(owner: &Client, name: &str, lacking: Rights, on: &[&str]) -> DbResult<()> {
    owner.roles().create(name, ROLE_PASSWORD).await?;
    for table in TABLES {
        let rights = if on.contains(&table) { Rights::ALL.without(lacking) } else { Rights::ALL };
        owner.roles().grant(name, rights, table).await?;
    }
    Ok(())
}

/// The proof for one right. Two roles are made that hold everything,
/// but that one of them lacks `right` on the tables of `on`. Each of
/// `statements` (made anew for each sending, from a number of its own)
/// must be refused the one, by every way it can be sent, with nothing in
/// the database changed by all those refusals together; and must be
/// answered the other, by every way.
async fn proves(server: &TestServer, owner: &Client, right: Rights, on: &[&str], statements: &[(&str, &dyn Fn(i64) -> Request)]) -> DbResult<()> {
    let tag = fresh();
    let (without, with) = (format!("without-{tag}"), format!("with-{tag}"));
    role_lacking(owner, &without, right, on).await?;
    role_lacking(owner, &with, Rights::NONE, &[]).await?;
    let before = everything(owner).await?;
    for (name, statement) in statements {
        for way in ways(&statement(0)) {
            let refused = send(server, &without, way, statement(fresh())).await;
            let e = refused.err().unwrap_or_else(|| panic!("{name}, sent {way:?} by a role without the right to {right} on {on:?}, was answered"));
            assert_eq!(e.code, ErrorCode::AccessDenied, "{name}, sent {way:?}: {}", e.message);
            assert!(e.message.contains(&without), "{name}, sent {way:?}: {}", e.message);
        }
    }
    assert!(everything(owner).await? == before, "statements refused for the right to {right} on {on:?} changed something");
    for (name, statement) in statements {
        for way in ways(&statement(0)) {
            if let Err(e) = send(server, &with, way, statement(fresh())).await {
                panic!("{name}, sent {way:?} by a role with every right, was refused: {} ({:?})", e.message, e.code);
            }
        }
    }
    owner.roles().drop(&without).await?;
    owner.roles().drop(&with).await
}

fn q(query: Query) -> Request {
    Request::Query(query)
}

// ---------------------------------------------------------- the rights

// R-A2: no statement shows a role the rows of a table it may not read.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_right_to_read_a_table_no_statement_is_told_what_it_holds() -> DbResult<()> {
    let (server, owner) = shop().await?;
    let customer_of = || Query::table("customers").key([col("orders.customer_id")]);
    proves(
        &server,
        &owner,
        Rights::READ,
        &["customers"],
        &[
            ("a query of the table", &|_| q(Query::table("customers").key([1]))),
            ("a query of all of it", &|_| q(Query::table("customers"))),
            ("a count of its rows", &|_| q(Query::table("customers").select("n", count()))),
            ("a query through its index", &|_| q(Query::table("customers").index("customers_code", KeyRange::eq([1])))),
            ("a join to it", &|_| q(Query::table("orders").join(Query::table("customers").alias("c").key([col("orders.customer_id")])))),
            ("a left join to it", &|_| q(Query::table("orders").left_join(Query::table("customers").alias("c").key([col("orders.customer_id")])))),
            ("a join that is asked for no rows", &|_| q(Query::table("orders").left_join(Query::table("customers").alias("c").key([col("orders.customer_id")]).limit(0)))),
            ("a filter that asks whether it holds a row", &|_| q(Query::table("orders").filter(exists(customer_of())))),
            ("a value selected from it", &|_| q(Query::table("orders").select("name", scalar(customer_of().columns(["name"]))))),
            ("a list made of it", &|_| q(Query::table("orders").filter(col("customer_id").in_list(list_of(Query::table("customers").columns(["id"])))))),
            ("a sort by a value of it", &|_| q(Query::table("orders").order_by(scalar(customer_of().columns(["name"])).asc()))),
            ("a query of no table that selects from it", &|_| q(Query::table("").select("n", scalar(Query::table("customers").select("n", count()))))),
            ("a subquery inside a subquery", &|_| q(Query::table("audit").filter(exists(Query::table("orders").filter(exists(customer_of())))))),
            ("an insert of its rows into another table", &|n| Request::Insert(Insert::into("audit").columns(["id", "what"]).from_query(Query::table("customers").key([1]).select("id", val(n)).select("what", col("name"))))),
            ("an insert into it that returns the row", &|n| Request::Insert(Insert::into("customers").set("id", n).set("name", "new").returning(["id"]))),
            ("an insert into it that updates on conflict", &|_| Request::Insert(Insert::into("customers").set("id", 1).set("name", "again").on_conflict_update(PRIMARY).update("name", excluded("name")))),
            ("an update of it by key", &|_| Request::Update(Update::table("customers").key([1]).set("name", "renamed"))),
            ("an update of it by filter", &|_| Request::Update(Update::rows(Query::table("customers").filter(col("name").eq("customer 2"))).set("name", "customer 2"))),
            ("a delete from it by key", &|n| Request::Delete(Delete::from("customers").key([n]))),
            ("an update of another table's rows found through it", &|_| Request::Update(Update::rows(Query::table("orders").join(Query::table("customers").alias("c").key([col("orders.customer_id")]))).set("total", 1))),
            ("an update of another table to a value of it", &|_| Request::Update(Update::table("orders").key([1]).set("total", scalar(Query::table("customers").select("n", count()))))),
            ("a delete from another table that returns a value of it", &|n| Request::Delete(Delete::rows(Query::table("audit").key([n % 9 + 1])).returning_expr("n", scalar(Query::table("customers").select("n", count()))))),
            ("an insert of a row that points at it", &|n| Request::Insert(Insert::into("orders").set("id", n).set("customer_id", 1))),
            ("an update that points a row at it", &|_| Request::Update(Update::table("orders").key([2]).set("customer_id", 2))),
        ],
    )
    .await?;
    // What the keys of a parent are is also learnt from a delete that the
    // rows of a child forbid, and from a change of a key that a child's
    // rows hold: each takes the right to read the child.
    owner.insert(Insert::into("invoices").set("id", 1).set("customer_id", 1)).await?;
    proves(&server, &owner, Rights::READ, &["invoices"], &[("a delete from the table its rows point at", &|n| Request::Delete(Delete::from("customers").key([n])))]).await?;
    // A foreign key is a way to ask a table which keys it holds: making
    // one takes the right to read the table it points at.
    let pointing = |n: i64| Request::Ddl(Ddl::CreateTable { def: TableDef::new(&format!("made_{n}")).column(ColumnDef::new("id", Type::Int)).primary_key(["id"]).foreign_key(["id"], "customers", OnDelete::Cascade), if_not_exists: false });
    for (role, reads) in [("maker", false), ("reading-maker", true)] {
        owner.roles().create(role, ROLE_PASSWORD).await?;
        owner.roles().grant_everywhere(role, Rights::SCHEMA).await?;
        if reads {
            owner.roles().grant(role, Rights::READ, "customers").await?;
        }
        for way in ways(&pointing(0)) {
            let made = send(&server, role, way, pointing(fresh())).await;
            assert_eq!(made.as_ref().err().map(|e| e.code), (!reads).then_some(ErrorCode::AccessDenied), "{role}, {way:?}: {made:?}");
        }
    }
    // A table is read when a statement comes to it. A subquery that no
    // row calls for reads nothing, and is not refused.
    owner.roles().create("filer", ROLE_PASSWORD).await?;
    owner.roles().grant("filer", Rights::ALL, "audit").await?;
    let unread = |key: i64| Request::Delete(Delete::rows(Query::table("audit").key([key])).returning_expr("n", scalar(Query::table("customers").select("n", count()))));
    send(&server, "filer", Way::Alone, unread(-1)).await?;
    assert_eq!(send(&server, "filer", Way::Alone, unread(9)).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
    Ok(())
}

// R-A2, the other half of a parent's key: a change of a key that rows of
// another table point at says whether they do.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_of_a_key_that_a_table_points_at_takes_the_right_to_read_that_table() -> DbResult<()> {
    let (server, owner) = shop().await?;
    // `invoices` here points at the customers' unique code, which unlike
    // a primary key can be changed.
    owner.drop_table("invoices").await?;
    owner.create_table(TableDef::new("invoices").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("code", Type::Int).nullable()).primary_key(["id"]).foreign_key_to(["code"], "customers", ["code"], OnDelete::Restrict)).await?;
    proves(&server, &owner, Rights::READ, &["invoices"], &[("a change of the key its rows point at", &|n| Request::Update(Update::table("customers").key([9]).set("code", n)))]).await
}

// R-A2: no statement adds a row to a table a role may not insert into.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_right_to_insert_no_statement_adds_a_row() -> DbResult<()> {
    let (server, owner) = shop().await?;
    let many = |n: i64| -> Request {
        // More rows than a small insert has, into a table with text to
        // cut into words: the statement of a bulk load, which the server
        // takes a way of its own.
        let mut insert = Insert::into("docs").columns(["id", "body"]);
        for i in 0..40 {
            insert = insert.values([val(n * 100 + i), val("words to cut ahead of the turn")]);
        }
        Request::Insert(insert)
    };
    proves(
        &server,
        &owner,
        Rights::INSERT,
        &["audit", "docs"],
        &[
            ("an insert of a row", &|n| Request::Insert(Insert::into("audit").set("id", n))),
            ("an insert of several rows", &|n| Request::Insert(Insert::into("audit").columns(["id"]).values([val(n * 100)]).values([val(n * 100 + 1)]))),
            ("an insert of another query's rows", &|n| Request::Insert(Insert::into("audit").columns(["id"]).from_query(Query::table("orders").key([1]).select("id", val(n))))),
            ("an insert that does nothing on conflict", &|_| Request::Insert(Insert::into("audit").set("id", 1).on_conflict_do_nothing())),
            ("an insert that updates on conflict", &|_| Request::Insert(Insert::into("audit").set("id", 1).on_conflict_update(PRIMARY).update("times", col("times") + 1))),
            ("an insert that returns its row", &|n| Request::Insert(Insert::into("audit").set("id", n).returning(["id"]))),
            ("the insert of a bulk load", &many),
        ],
    )
    .await
}

// R-A2: no statement changes a row of a table a role may not update.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_right_to_update_no_statement_changes_a_row() -> DbResult<()> {
    let (server, owner) = shop().await?;
    proves(
        &server,
        &owner,
        Rights::UPDATE,
        &["audit"],
        &[
            ("an update by key", &|_| Request::Update(Update::table("audit").key([1]).set("what", "changed"))),
            ("an update by filter", &|_| Request::Update(Update::rows(Query::table("audit").filter(col("what").eq("seen"))).set("times", col("times") + 1))),
            ("an update that changes no row", &|n| Request::Update(Update::table("audit").key([n]).set("what", "nobody"))),
            ("an update that returns the row", &|_| Request::Update(Update::table("audit").key([2]).set("what", "changed").returning(["what"]))),
            ("an update of rows found through a join", &|_| Request::Update(Update::rows(Query::table("audit").join(Query::table("orders").key([col("audit.id")]))).set("what", "has an order"))),
            ("an insert that updates on conflict", &|_| Request::Insert(Insert::into("audit").set("id", 1).on_conflict_update(PRIMARY).update("times", col("times") + 1))),
        ],
    )
    .await?;
    // A delete that clears the pointers of another table's rows changes
    // those rows.
    proves(&server, &owner, Rights::UPDATE, &["notes"], &[("a delete from the table its rows point at, which clears them", &|n| Request::Delete(Delete::from("orders").key([n])))]).await
}

// R-A2: no statement removes a row of a table a role may not delete from.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_right_to_delete_no_statement_removes_a_row() -> DbResult<()> {
    let (server, owner) = shop().await?;
    proves(
        &server,
        &owner,
        Rights::DELETE,
        &["audit"],
        &[
            ("a delete by key", &|_| Request::Delete(Delete::from("audit").key([1]))),
            ("a delete by filter", &|_| Request::Delete(Delete::rows(Query::table("audit").filter(col("what").eq("nothing is called this"))))),
            ("a delete that returns the row", &|_| Request::Delete(Delete::from("audit").key([2]).returning(["what"]))),
            ("a delete that removes no row", &|n| Request::Delete(Delete::from("audit").key([n]))),
        ],
    )
    .await?;
    // A delete that takes the rows of another table with it removes
    // those rows: of the table that points at this one, and of the table
    // that points at that.
    let cascades: &[(&str, &dyn Fn(i64) -> Request)] = &[("a delete from the table its rows go with", &|n| Request::Delete(Delete::from("customers").key([n])))];
    proves(&server, &owner, Rights::DELETE, &["orders"], cascades).await?;
    proves(&server, &owner, Rights::DELETE, &["lines"], cascades).await
}

// R-A2: no statement changes what a table is for a role that may not.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_right_to_the_schema_no_statement_changes_a_table() -> DbResult<()> {
    let (server, owner) = shop().await?;
    proves(
        &server,
        &owner,
        Rights::SCHEMA,
        &["audit"],
        &[
            ("a column added", &|n| Request::Ddl(Ddl::AddColumn { table: "audit".into(), column: ColumnDef::new(&format!("c{n}"), Type::Int).nullable(), if_not_exists: false })),
            ("a column added if it is not there, which it is", &|_| Request::Ddl(Ddl::AddColumn { table: "audit".into(), column: ColumnDef::new("what", Type::Text), if_not_exists: true })),
            ("a column dropped", &|n| Request::Ddl(Ddl::DropColumn { table: "audit".into(), column: format!("gone{n}"), if_exists: true })),
            ("an index made", &|n| Request::Ddl(Ddl::CreateIndex { table: "audit".into(), index: IndexDef::new(&format!("audit_{n}")).on(["what"]), if_not_exists: false })),
            ("an index dropped", &|n| Request::Ddl(Ddl::DropIndex { table: "audit".into(), index: format!("gone{n}"), if_exists: true })),
        ],
    )
    .await?;
    // The table itself: dropped only by a role with the right on it, and
    // made only by one with the right on every table, since a table that
    // is not there yet is none to hold a right on.
    owner.roles().create("on-audit", ROLE_PASSWORD).await?;
    owner.roles().grant("on-audit", Rights::ALL, "audit").await?;
    owner.roles().create("everywhere", ROLE_PASSWORD).await?;
    owner.roles().grant_everywhere("everywhere", Rights::SCHEMA).await?;
    let make = |n: i64| Request::Ddl(Ddl::CreateTable { def: TableDef::new(&format!("made_{n}")).column(ColumnDef::new("id", Type::Int)).primary_key(["id"]), if_not_exists: true });
    let drop_it = |table: &str| Request::Ddl(Ddl::DropTable { name: table.into(), if_exists: false });
    for way in ways(&make(0)) {
        let refused = send(&server, "on-audit", way, make(fresh())).await.err().unwrap_or_else(|| panic!("a table was made, sent {way:?}, by a role with rights on one table"));
        assert_eq!(refused.code, ErrorCode::AccessDenied, "{way:?}: {}", refused.message);
        let refused = send(&server, "on-audit", way, drop_it("orders")).await.err().unwrap_or_else(|| panic!("a table was dropped, sent {way:?}, by a role with no right on it"));
        assert_eq!(refused.code, ErrorCode::AccessDenied, "{way:?}: {}", refused.message);
        let n = fresh();
        send(&server, "everywhere", way, make(n)).await?;
        send(&server, "everywhere", way, drop_it(&format!("made_{n}"))).await?;
    }
    assert_eq!(owner.describe().await?.len(), TABLES.len(), "nothing was made or dropped that should not have been");
    send(&server, "on-audit", Way::Alone, drop_it("audit")).await?;
    assert!(owner.describe_table("audit").await?.is_none());
    Ok(())
}

// R-A2: a refusal says the same of a table that is not there as of one
// that is, so a role cannot ask which names exist.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_is_not_told_which_tables_exist_among_those_it_has_no_right_to() -> DbResult<()> {
    let (server, owner) = shop().await?;
    owner.roles().create("clerk", ROLE_PASSWORD).await?;
    owner.roles().grant("clerk", Rights::READ, "audit").await?;
    let message = |table: &'static str, statement: Request| {
        let server = &server;
        async move {
            let e = send(server, "clerk", Way::Alone, statement).await.expect_err("refused");
            assert_eq!(e.code, ErrorCode::AccessDenied, "{}", e.message);
            e.message.replace(table, "T")
        }
    };
    for (there, absent) in [("orders", "nowhere")] {
        assert_eq!(message(there, q(Query::table(there))).await, message(absent, q(Query::table(absent))).await);
        assert_eq!(message(there, Request::Insert(Insert::into(there).set("id", 1))).await, message(absent, Request::Insert(Insert::into(absent).set("id", 1))).await);
        assert_eq!(message(there, Request::Update(Update::table(there).key([1]).set("id", 1))).await, message(absent, Request::Update(Update::table(absent).key([1]).set("id", 1))).await);
        assert_eq!(message(there, Request::Delete(Delete::from(there).key([1]))).await, message(absent, Request::Delete(Delete::from(absent).key([1]))).await);
        assert_eq!(message(there, Request::Ddl(Ddl::DropTable { name: there.into(), if_exists: true })).await, message(absent, Request::Ddl(Ddl::DropTable { name: absent.into(), if_exists: true })).await);
    }
    // It is shown the definitions of the tables it has a right on, and
    // of no other.
    let mut wire = Wire::role(&server, "clerk").await;
    let Response::Tables(shown) = wire.call(Request::Describe).await else { panic!("describe is answered with tables") };
    assert_eq!(shown.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["audit"]);
    wire.ask(Request::Begin { kind: TxnKind::Exclusive, lock_wait_ms: 1_000 }).await?;
    let Response::Tables(shown) = wire.call(Request::Describe).await else { panic!("describe is answered with tables") };
    assert_eq!(shown.len(), 1);
    Ok(())
}

// R-A2: a role with one right does with it what the right is for, and a
// table it was granted nothing on is closed to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_does_what_it_was_granted_and_nothing_else() -> DbResult<()> {
    let (server, owner) = shop().await?;
    owner.roles().create("filer", ROLE_PASSWORD).await?;
    owner.roles().grant("filer", Rights::INSERT, "audit").await?;
    owner.roles().create("reader", ROLE_PASSWORD).await?;
    owner.roles().grant_everywhere("reader", Rights::READ).await?;
    let as_role = |role: &str| {
        let mut config = Config::from_url(&server.url("test")).unwrap();
        config.role = Some(role.to_string());
        config.password = ROLE_PASSWORD.to_string();
        Client::connect(config)
    };
    // One that may only insert files rows it can neither see nor change.
    let filer = as_role("filer").await?;
    filer.insert(Insert::into("audit").set("id", 100).set("what", "filed")).await?;
    filer.insert(Insert::into("audit").set("id", 100).on_conflict_do_nothing()).await?;
    let taken = filer.insert(Insert::into("audit").set("id", 100)).await.expect_err("the key is taken");
    assert_eq!(taken.code, ErrorCode::UniqueViolation);
    for refused in [filer.query(Query::table("audit")).await.err(), filer.update(Update::table("audit").key([100]).set("what", "x")).await.err(), filer.delete(Delete::from("audit").key([100])).await.err(), filer.query(Query::table("orders")).await.err(), filer.insert(Insert::into("orders").set("id", 100)).await.err()] {
        assert_eq!(refused.map(|e| e.code), Some(ErrorCode::AccessDenied));
    }
    // One that may read everything reads tables made after the grant
    // too, and writes none.
    let reader = as_role("reader").await?;
    owner.create_table(TableDef::new("later").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    assert_eq!(reader.query(Query::table("later")).await?.len(), 0);
    assert_eq!(reader.query(Query::table("audit").key([100])).await?.one()?.get::<String>("what")?, "filed");
    assert_eq!(reader.describe().await?.len(), TABLES.len() + 1);
    assert_eq!(reader.insert(Insert::into("later").set("id", 1)).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
    // A statement refused inside a transaction leaves it as a failed
    // statement does: nothing of it commits.
    let tx = filer.begin().await?;
    tx.insert(Insert::into("audit").set("id", 101)).await?;
    assert_eq!(tx.query(Query::table("audit")).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
    assert!(tx.commit().await.is_err());
    assert!(owner.get("audit", [101]).await?.is_none());
    Ok(())
}

// ------------------------------------------------------------ greeting

// R-A1: a role is named in the greeting's database, so a client of any
// protocol version connects as one; its password is its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_connects_with_its_own_password_from_a_client_of_any_version() -> DbResult<()> {
    let (server, owner) = shop().await?;
    owner.roles().create("reader", ROLE_PASSWORD).await?;
    owner.roles().grant("reader", Rights::READ, "audit").await?;
    for version in 2..=PROTOCOL_VERSION {
        let mut wire = Wire::greet(&server, "test/reader", ROLE_PASSWORD, version).await?;
        assert!(matches!(wire.call(q(Query::table("audit").key([1]))).await, Response::Rows { set, .. } if set.rows.len() == 1), "version {version}");
        assert!(matches!(wire.call(q(Query::table("orders"))).await, Response::Error(e) if e.code == ErrorCode::AccessDenied), "version {version}");
    }
    // Nobody else's password opens a role, the administrator's included:
    // a greeting that names a role gets what the role may do or nothing.
    for (database, password) in [("test/reader", PASSWORD), ("test/reader", ADMIN_PASSWORD), ("test/reader", ""), ("test/nobody", ROLE_PASSWORD), ("other/reader", ROLE_PASSWORD), ("test/", ROLE_PASSWORD), ("test/reader/x", ROLE_PASSWORD), ("/reader", ADMIN_PASSWORD), ("test", ROLE_PASSWORD)] {
        let refused = Wire::greet(&server, database, password, PROTOCOL_VERSION).await.err().unwrap_or_else(|| panic!("{database:?} was opened with {password:?}"));
        assert_eq!(refused.code, ErrorCode::AccessDenied, "{database:?}");
        assert_eq!(refused.message, "no such database, or the wrong password", "one answer for every refusal");
    }
    // A role whose password is the administrator's is still only a role.
    owner.roles().create("twin", ADMIN_PASSWORD).await?;
    let mut twin = Wire::greet(&server, "test/twin", ADMIN_PASSWORD, PROTOCOL_VERSION).await?;
    assert!(matches!(twin.call(q(Query::table("audit"))).await, Response::Error(e) if e.code == ErrorCode::AccessDenied));
    assert!(matches!(twin.call(Request::Admin(Admin::ListDatabases)).await, Response::Error(e) if e.code == ErrorCode::AccessDenied));
    // A changed password is the one that opens it from then on.
    owner.roles().set_password("reader", "another").await?;
    assert!(Wire::greet(&server, "test/reader", ROLE_PASSWORD, 2).await.is_err());
    Wire::greet(&server, "test/reader", "another", 2).await?;
    // The client names the role in its URL.
    let reader = Client::connect_url(&format!("database://test:another@127.0.0.1:{}?role=reader", server.port)).await?;
    assert_eq!(reader.query(Query::table("audit")).await?.len(), 9);
    // Names are held to what a greeting can carry.
    for bad in ["", "Reader", "a/b", "a b", "a.b"] {
        assert_eq!(owner.roles().create(bad, ROLE_PASSWORD).await.err().map(|e| e.code), Some(ErrorCode::InvalidRequest), "{bad:?}");
    }
    Ok(())
}

// R-A7: a database with no roles is what it was: one password, which
// may do anything in it, from clients of every version.
#[tokio::test(flavor = "multi_thread")]
async fn a_database_with_one_password_is_served_as_before() -> DbResult<()> {
    let (server, owner) = shop().await?;
    for version in 2..=PROTOCOL_VERSION {
        let mut wire = Wire::greet(&server, "test", PASSWORD, version).await?;
        assert!(matches!(wire.call(q(Query::table("customers"))).await, Response::Rows { .. }));
        assert!(matches!(wire.call(Request::Admin(Admin::Check)).await, Response::Text(_)));
        let mut admin = Wire::greet(&server, "test", ADMIN_PASSWORD, version).await?;
        assert!(matches!(admin.call(Request::Admin(Admin::ListDatabases)).await, Response::Names(_)));
    }
    assert!(owner.roles().list().await?.is_empty());
    // Roles beside it take nothing from it.
    owner.roles().create("reader", ROLE_PASSWORD).await?;
    owner.update(Update::table("customers").key([1]).set("name", "still the owner's")).await?;
    owner.create_table(TableDef::new("more").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    owner.admin().check().await?;
    let guard = owner.lock("job").await?;
    guard.unlock().await
}

// ------------------------------------------------------- who may grant

// R-A3: roles are made and granted by the database's own password or
// the administrator's, and by no role, whatever it holds.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_and_the_administrator_change_roles() -> DbResult<()> {
    let (server, owner) = shop().await?;
    owner.roles().create("chief", ROLE_PASSWORD).await?;
    owner.roles().grant_everywhere("chief", Rights::ALL).await?;
    let requests = || {
        vec![
            Admin::CreateRole { name: "made".into(), password: "p".into(), if_not_exists: true },
            Admin::DropRole { name: "chief".into(), if_exists: true },
            Admin::SetRolePassword { name: "chief".into(), password: "p".into() },
            Admin::Grant { role: "chief".into(), table: None, rights: Rights::ALL },
            Admin::Revoke { role: "chief".into(), table: Some("audit".into()), rights: Rights::NONE },
            Admin::ListRoles,
            // And what a database's own password may ask besides.
            Admin::Checkpoint,
            Admin::Check,
            Admin::Stats,
            Admin::Dump { databases: Vec::new() },
            Admin::ListDatabases,
            Admin::SetPassword { name: "test".into(), password: "p".into() },
        ]
    };
    let mut chief = Wire::role(&server, "chief").await;
    for request in requests() {
        let said = format!("{request:?}");
        let Response::Error(e) = chief.call(Request::Admin(request)).await else { panic!("a role was answered {said}") };
        assert_eq!(e.code, ErrorCode::AccessDenied, "{said}");
    }
    assert_eq!(owner.roles().list().await?, [RoleRights { role: "chief".into(), table: None, rights: Rights::ALL }]);
    Wire::role(&server, "chief").await;
    // The administrator, connected to the database, may; connected to
    // none, there is no database for the roles to be of.
    let mut admin = Wire::greet(&server, "test", ADMIN_PASSWORD, PROTOCOL_VERSION).await?;
    admin.ask(Request::Admin(Admin::CreateRole { name: "by-admin".into(), password: "p".into(), if_not_exists: false })).await?;
    let mut nowhere = Wire::greet(&server, "", ADMIN_PASSWORD, PROTOCOL_VERSION).await?;
    assert_eq!(nowhere.ask(Request::Admin(Admin::ListRoles)).await.err().map(|e| e.code), Some(ErrorCode::InvalidRequest));
    // A change of roles is a transaction of its own.
    let mut own = Wire::greet(&server, "test", PASSWORD, PROTOCOL_VERSION).await?;
    own.ask(Request::Begin { kind: TxnKind::Optimistic, lock_wait_ms: 0 }).await?;
    assert_eq!(own.ask(Request::Admin(Admin::DropRole { name: "chief".into(), if_exists: false })).await.err().map(|e| e.code), Some(ErrorCode::InvalidRequest));
    own.ask(Request::Rollback).await?;

    // What the requests answer.
    let roles = owner.roles();
    assert_eq!(roles.create("chief", "p").await.err().map(|e| e.code), Some(ErrorCode::AlreadyExists));
    roles.create_if_not_exists("chief", "ignored").await?;
    Wire::role(&server, "chief").await;
    assert_eq!(roles.drop("nobody").await.err().map(|e| e.code), Some(ErrorCode::NotFound));
    roles.drop_if_exists("nobody").await?;
    assert_eq!(roles.grant("nobody", Rights::READ, "audit").await.err().map(|e| e.code), Some(ErrorCode::NotFound));
    assert_eq!(roles.grant("chief", Rights::READ, "no-table").await.err().map(|e| e.code), Some(ErrorCode::NotFound));
    assert_eq!(roles.set_password("nobody", "p").await.err().map(|e| e.code), Some(ErrorCode::NotFound));
    roles.create("clerk", "p").await?;
    roles.grant("clerk", Rights::READ | Rights::INSERT, "audit").await?;
    roles.grant("clerk", Rights::UPDATE, "audit").await?;
    roles.grant("clerk", Rights::READ, "orders").await?;
    roles.revoke("clerk", Rights::INSERT | Rights::DELETE, "audit").await?;
    roles.revoke("clerk", Rights::READ, "orders").await?;
    let listed: Vec<RoleRights> = roles.list().await?.into_iter().filter(|r| r.role == "clerk").collect();
    assert_eq!(listed, [RoleRights { role: "clerk".into(), table: None, rights: Rights::NONE }, RoleRights { role: "clerk".into(), table: Some("audit".into()), rights: Rights::READ | Rights::UPDATE }]);
    Ok(())
}

// R-A5: a named lock is for those who write.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_that_only_reads_holds_no_named_lock() -> DbResult<()> {
    let (server, owner) = shop().await?;
    owner.roles().create("reader", ROLE_PASSWORD).await?;
    owner.roles().grant_everywhere("reader", Rights::READ).await?;
    owner.roles().create("writer", ROLE_PASSWORD).await?;
    owner.roles().grant("writer", Rights::INSERT, "audit").await?;
    let mut reader = Wire::role(&server, "reader").await;
    assert_eq!(reader.ask(Request::Lock { name: "migrate".into(), wait_ms: 0 }).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
    let mut writer = Wire::role(&server, "writer").await;
    assert!(matches!(writer.ask(Request::Lock { name: "migrate".into(), wait_ms: 0 }).await?, Response::Flag(true)));
    assert!(owner.try_lock("migrate").await?.is_none(), "the writer holds it");
    Ok(())
}

// -------------------------------------------------------------- revoke

async fn reads(wire: &mut Wire, table: &str) -> DbResult<Response> {
    wire.ask(q(Query::table(table).limit(1))).await
}

// R-A4: once a revoke is answered, no statement that begins runs by the
// right it took: not a prepared one kept from before, not one in a
// transaction that was open, and no transaction commits a write past it.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoke_binds_every_statement_that_begins_after_it_is_answered() -> DbResult<()> {
    let (server, owner) = fresh_server().await;
    tables(&owner).await?;
    let roles = owner.roles();
    roles.create("clerk", ROLE_PASSWORD).await?;
    roles.grant("clerk", Rights::READ | Rights::INSERT | Rights::UPDATE, "audit").await?;
    roles.grant("clerk", Rights::READ, "orders").await?;
    let denied = |answer: DbResult<Response>, what: &str| {
        let e = answer.err().unwrap_or_else(|| panic!("{what} was answered after the revoke"));
        assert_eq!(e.code, ErrorCode::AccessDenied, "{what}: {}", e.message);
    };

    // A statement kept by its connection from before the revoke.
    let mut kept = Wire::role(&server, "clerk").await;
    let by_number = |params: Vec<Value>| Request::Prepared { id: 7, statement: None, params };
    kept.ask(Request::Prepared { id: 7, statement: Some(std::sync::Arc::new(q(Query::table("orders").key([param(0)])))), params: vec![Value::Int(1)] }).await?;
    kept.ask(by_number(vec![Value::Int(2)])).await?;
    // A reading transaction that has read the table, an optimistic one
    // that has, and an optimistic one that has written beside it.
    let mut reading = Wire::role(&server, "clerk").await;
    reading.ask(Request::Begin { kind: TxnKind::Read, lock_wait_ms: 0 }).await?;
    reads(&mut reading, "orders").await?;
    let mut optimistic = Wire::role(&server, "clerk").await;
    optimistic.ask(Request::Begin { kind: TxnKind::Optimistic, lock_wait_ms: 1_000 }).await?;
    reads(&mut optimistic, "orders").await?;
    let mut writing = Wire::role(&server, "clerk").await;
    writing.ask(Request::Begin { kind: TxnKind::Optimistic, lock_wait_ms: 1_000 }).await?;
    reads(&mut writing, "orders").await?;
    writing.ask(Request::Insert(Insert::into("audit").set("id", 500).set("what", "copied from an order"))).await?;

    roles.revoke("clerk", Rights::READ, "orders").await?;

    denied(kept.ask(by_number(vec![Value::Int(1)])).await, "a statement kept from before");
    denied(kept.ask(Request::Explain(Box::new(by_number(vec![Value::Int(1)])))).await, "a kept statement, explained");
    denied(kept.ask(Request::Batch(vec![BatchItem { statement: by_number(vec![Value::Int(1)]), expect: None }])).await, "a kept statement in a batch");
    denied(reads(&mut reading, "orders").await, "a statement in a reading transaction open since before");
    denied(reads(&mut optimistic, "orders").await, "a statement in an optimistic transaction open since before");
    // Not even of a table the role may still read: the transaction is on
    // the version, and the rights, of before the revoke.
    assert_eq!(reads(&mut reading, "audit").await.err().map(|e| e.code), Some(ErrorCode::TransactionAborted));
    reading.ask(Request::Rollback).await?;
    reads(&mut reading, "audit").await?;
    // The transaction that wrote what it had read does not commit.
    assert!(writing.ask(Request::Commit).await.is_err(), "a transaction committed a write past the revoke of what it read");
    assert!(owner.get("audit", [500]).await?.is_none());

    // While a role's exclusive transaction is open no rights change: the
    // revoke waits its turn like any write, here for longer than it is
    // given. When it is answered, the transaction is over.
    let mut exclusive = Wire::role(&server, "clerk").await;
    exclusive.ask(Request::Begin { kind: TxnKind::Exclusive, lock_wait_ms: 1_000 }).await?;
    exclusive.ask(Request::Update(Update::table("audit").key([1]).set("what", "under the turn"))).await?;
    let revoking = tokio::spawn({
        let owner = owner.clone();
        async move { owner.roles().revoke("clerk", Rights::UPDATE, "audit").await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!revoking.is_finished(), "a revoke was answered while a transaction held the turn by the right it takes");
    exclusive.ask(Request::Update(Update::table("audit").key([2]).set("what", "under the turn"))).await?;
    exclusive.ask(Request::Commit).await?;
    revoking.await.unwrap()?;
    denied(exclusive.ask(Request::Update(Update::table("audit").key([3]).set("what", "after"))).await, "an update after the revoke");

    // Rights held on every table are not taken from one table alone: the
    // revoke fails whole, and says how it is done.
    roles.grant_everywhere("clerk", Rights::READ).await?;
    let partial = roles.revoke("clerk", Rights::READ | Rights::INSERT, "audit").await.expect_err("refused");
    assert_eq!(partial.code, ErrorCode::InvalidRequest);
    assert!(partial.message.contains("every table"), "{}", partial.message);
    let mut clerk = Wire::role(&server, "clerk").await;
    clerk.ask(Request::Insert(Insert::into("audit").set("id", 600))).await?;
    // Taken from every table, they are gone however they were granted.
    roles.grant("clerk", Rights::READ, "orders").await?;
    roles.revoke_everywhere("clerk", Rights::READ).await?;
    for table in ["orders", "audit", "customers"] {
        denied(reads(&mut clerk, table).await, "a read after the right was taken from every table");
    }
    clerk.ask(Request::Insert(Insert::into("audit").set("id", 601))).await?;

    // A dropped role's connections are refused from their next statement,
    // and it opens no new one; a role made again under its name is
    // another, with nothing.
    roles.drop("clerk").await?;
    denied(clerk.ask(Request::Insert(Insert::into("audit").set("id", 602))).await, "an insert by a dropped role");
    assert!(Wire::greet(&server, "test/clerk", ROLE_PASSWORD, PROTOCOL_VERSION).await.is_err());
    roles.create("clerk", ROLE_PASSWORD).await?;
    denied(clerk.ask(Request::Insert(Insert::into("audit").set("id", 603))).await, "an insert by a role made again");
    assert!(owner.get("audit", [602]).await?.is_none() && owner.get("audit", [603]).await?.is_none());

    // The rights on a table go with the table: one made again under its
    // name is another table.
    roles.grant("clerk", Rights::ALL, "audit").await?;
    owner.drop_table("audit").await?;
    owner.create_table(TableDef::new("audit").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    denied(reads(&mut clerk, "audit").await, "a read of a table made again");
    assert!(roles.list().await?.iter().all(|r| r.table.as_deref() != Some("audit")));
    Ok(())
}

// ---------------------------------------------------------- durability

// R-A6: roles and their rights are part of the database: they are there
// after a restart, and they travel in a dump.
#[tokio::test(flavor = "multi_thread")]
async fn roles_are_kept_across_a_restart_and_travel_in_a_dump() -> DbResult<()> {
    let (server, owner) = shop().await?;
    let roles = owner.roles();
    roles.create("reader", ROLE_PASSWORD).await?;
    roles.grant_everywhere("reader", Rights::READ).await?;
    roles.create("clerk", "clerk-password").await?;
    roles.grant("clerk", Rights::READ | Rights::INSERT, "audit").await?;
    roles.create("nobody", "p").await?;
    let listed = roles.list().await?;
    let admin = server.admin().await;
    let mut dump = Vec::new();
    admin.admin().dump(&["test"], &mut dump).await?;
    // A role dumps nothing: a dump holds every table.
    let mut clerk = Wire::greet(&server, "test/clerk", "clerk-password", PROTOCOL_VERSION).await?;
    assert_eq!(clerk.ask(Request::Admin(Admin::Dump { databases: Vec::new() })).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));

    let check = |server: TestServer, listed: Vec<RoleRights>| async move {
        let owner = Client::connect_url(&server.url("test")).await?;
        assert_eq!(owner.roles().list().await?, listed);
        let mut clerk = Wire::greet(&server, "test/clerk", "clerk-password", PROTOCOL_VERSION).await?;
        clerk.ask(Request::Insert(Insert::into("audit").set("id", fresh()))).await?;
        assert_eq!(reads(&mut clerk, "orders").await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
        let mut reader = Wire::role(&server, "reader").await;
        reads(&mut reader, "orders").await?;
        assert_eq!(reader.ask(Request::Insert(Insert::into("audit").set("id", fresh()))).await.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
        assert!(Wire::greet(&server, "test/reader", "clerk-password", PROTOCOL_VERSION).await.is_err());
        Ok::<_, DbError>(server)
    };
    // Stopped and started on the same files.
    let dir = server.restart_dir().await;
    let server = check(TestServer::start_with(dir, |_| {}).await, listed.clone()).await?;
    server.stop().await;
    // Loaded into a server that never had them.
    let elsewhere = TestServer::start().await;
    elsewhere.admin().await.admin().restore(&mut dump.as_slice(), false).await?;
    check(elsewhere, listed).await?;
    Ok(())
}
