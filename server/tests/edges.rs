//! Edges: the cases at the border of a rule, where a database is most
//! often wrong. Each is something that once behaved differently here.

mod common;
use common::*;

use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// R-T5, R-Q4: a bound on a decimal key is compared by its value, not
// first rounded to the column's scale.
#[tokio::test(flavor = "multi_thread")]
async fn a_decimal_bound_keeps_its_digits() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(TableDef::new("prices").column(ColumnDef::new("amount", Type::decimal(15, 2))).column(ColumnDef::new("label", Type::Text)).primary_key(["amount"])).await?;
    for (amount, label) in [("1.00", "one"), ("1.01", "one and a cent"), ("10.00", "ten")] {
        db.insert(Insert::into("prices").set("amount", amount).set("label", label)).await?;
    }
    let labels = |rows: Rows| column::<String>(&rows, "label");
    // 1.005 is no stored amount: rounded it would be 1.01.
    assert!(db.query(Query::table("prices").key([1.005])).await?.is_empty());
    assert!(db.query(Query::table("prices").key(["1.005"])).await?.is_empty());
    assert_eq!(labels(db.query(Query::table("prices").key([1.01])).await?), ["one and a cent"]);
    assert_eq!(labels(db.query(Query::table("prices").key(["1.010"])).await?), ["one and a cent"]);
    // Everything below 10.004 includes 10.00; rounded, the bound would
    // be 10.00 itself and exclude it.
    let below = KeyRange::new(Bound::Unbounded, Bound::excluded([10.004]));
    assert_eq!(labels(db.query(Query::table("prices").range(below)).await?), ["one", "one and a cent", "ten"]);
    let above = KeyRange::new(Bound::excluded([1.004]), Bound::Unbounded);
    assert_eq!(labels(db.query(Query::table("prices").range(above)).await?), ["one and a cent", "ten"]);
    // What is stored is still rounded to the cent.
    db.insert(Insert::into("prices").set("amount", 2.345).set("label", "rounded")).await?;
    assert_eq!(labels(db.query(Query::table("prices").key(["2.35"])).await?), ["rounded"]);
    // A decimal with more digits than any column can hold is not a type.
    let wide = db.create_table(TableDef::new("wide").column(ColumnDef::new("n", Type::decimal(38, 35))).primary_key(["n"])).await.unwrap_err();
    assert_eq!(wide.code, ErrorCode::InvalidRequest);
    Ok(())
}

// R-S9, R-Q6: a bound on an index over the items of a list is read as
// the items' type, as a bound on a plain column is.
#[tokio::test(flavor = "multi_thread")]
async fn a_bound_on_a_list_index_is_read_as_the_items_type() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(
        TableDef::new("pages")
            .column(ColumnDef::new("id", Type::Int))
            .column(ColumnDef::new("label_ids", Type::list(Type::Uuid)).default(Value::List(Vec::new())))
            .primary_key(["id"])
            .index(IndexDef::new("pages_labels").on_each(col("label_ids"))),
    )
    .await?;
    db.insert(Insert::into("pages").set("id", 1).set("label_ids", Value::List(vec![uuid(7).into(), uuid(8).into()]))).await?;
    db.insert(Insert::into("pages").set("id", 2).set("label_ids", Value::List(vec![uuid(8).into()]))).await?;
    let ids = |rows: Rows| column::<i64>(&rows, "id");
    assert_eq!(ids(db.query(Query::table("pages").index("pages_labels", KeyRange::eq([uuid(8)]))).await?), [1, 2]);
    // The same label written as text, in either case.
    let text = uuid(8).to_string();
    assert_eq!(ids(db.query(Query::table("pages").index("pages_labels", KeyRange::eq([text.as_str()]))).await?), [1, 2]);
    assert_eq!(ids(db.query(Query::table("pages").index("pages_labels", KeyRange::eq([text.to_uppercase()]))).await?), [1, 2]);
    assert_eq!(ids(db.query(Query::table("pages").index("pages_labels", KeyRange::eq([uuid(7).to_string()]))).await?), [1]);
    let wrong = db.query(Query::table("pages").index("pages_labels", KeyRange::eq(["not a uuid"]))).await.unwrap_err();
    assert_eq!(wrong.code, ErrorCode::TypeMismatch);
    Ok(())
}

// R-S6: a key that rows of another table point at cannot change under
// them, when it is a unique key that is not the primary key; and rows
// that outlive a restart cannot point at rows that do not.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_others_point_at_stays_while_they_do() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(TableDef::new("users").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("email", Type::Text)).primary_key(["id"]).unique("users_email", ["email"])).await?;
    db.create_table(
        TableDef::new("invites")
            .column(ColumnDef::new("id", Type::Int))
            .column(ColumnDef::new("email", Type::Text).nullable())
            .primary_key(["id"])
            .foreign_key_to(["email"], "users", ["email"], OnDelete::Restrict),
    )
    .await?;
    db.insert(Insert::into("users").set("id", 1).set("email", "ann@example.com")).await?;
    db.insert(Insert::into("users").set("id", 2).set("email", "bob@example.com")).await?;
    db.insert(Insert::into("invites").set("id", 10).set("email", "ann@example.com")).await?;

    let refused = db.update(Update::table("users").key([1]).set("email", "ann@elsewhere.example")).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::ForeignKeyViolation);
    assert_eq!(db.get("users", [1]).await?.unwrap().get::<String>("email")?, "ann@example.com");
    // Written again as it is, or changed where nothing points: fine.
    db.update(Update::table("users").key([1]).set("email", "ann@example.com")).await?;
    db.update(Update::table("users").key([2]).set("email", "bob@elsewhere.example")).await?;
    // Once nothing points at it, it may change.
    db.update(Update::table("invites").key([10]).set("email", Value::Null)).await?;
    db.update(Update::table("users").key([1]).set("email", "ann@elsewhere.example")).await?;

    // A table that is emptied at every start cannot be depended on by
    // one that is not.
    db.create_table(TableDef::new("leases").column(ColumnDef::new("id", Type::Int)).primary_key(["id"]).volatile()).await?;
    let lasting = TableDef::new("jobs").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("lease", Type::Int).nullable()).primary_key(["id"]).foreign_key(["lease"], "leases", OnDelete::SetNull);
    assert_eq!(db.create_table(lasting.clone()).await.unwrap_err().code, ErrorCode::InvalidRequest);
    // Two that are both emptied may depend on each other.
    db.create_table(lasting.volatile()).await?;
    Ok(())
}

// R-T9, R-Q6, R-Q9, R-Q22: small rules of SQL that are easy to get wrong.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_in_an_empty_list_and_other_small_rules() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(TableDef::new("t").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("team", Type::Text).nullable()).column(ColumnDef::new("x", Type::Float).default(0.0)).primary_key(["id"])).await?;
    db.insert(Insert::into("t").set("id", 1).set("team", "red").set("x", -0.0)).await?;
    db.insert(Insert::into("t").set("id", 2).set("x", 1.5)).await?;
    let ids = |rows: Rows| column::<i64>(&rows, "id");
    let none: Vec<String> = Vec::new();

    // `x NOT IN ()` is true for every row, a row whose x is NULL too.
    assert_eq!(ids(db.query(Query::table("t").filter(col("team").not_in_list(none.clone()))).await?), [1, 2]);
    assert!(db.query(Query::table("t").filter(col("team").in_list(none))).await?.is_empty());
    // In a list that is not empty, NULL is still unknown.
    assert_eq!(ids(db.query(Query::table("t").filter(col("team").not_in_list(vec!["blue"]))).await?), [1]);

    // Zero and minus zero are one number.
    assert_eq!(ids(db.query(Query::table("t").filter(col("x").eq(0.0))).await?), [1]);
    assert_eq!(ids(db.query(Query::table("t").filter(col("x").ge(0.0))).await?), [1, 2]);

    // A join asked for no rows gives none: an inner join drops the row,
    // a left join keeps it alone.
    let joined = |query: Query| Query::table("t").alias("a").join(query).select("id", col("a.id"));
    assert!(db.query(joined(Query::table("t").alias("b").key([col("a.id")]).limit(0))).await?.is_empty());
    assert_eq!(db.query(joined(Query::table("t").alias("b").key([col("a.id")]).limit(1))).await?.len(), 2);
    let left = Query::table("t").alias("a").left_join(Query::table("t").alias("b").key([col("a.id")]).limit(0)).select("id", col("a.id")).select("other", col("b.id"));
    assert_eq!(column::<Option<i64>>(&db.query(left).await?, "other"), [None, None]);

    // A float too large for an integer is out of range, not the largest
    // integer; one just inside converts.
    let cast = |f: f64| db.query_one(Query::row().select("n", val(f).cast(Type::Int)));
    assert_eq!(cast(9.3e18).await.unwrap_err().code, ErrorCode::OutOfRange);
    assert_eq!(cast(9_223_372_036_854_775_808.0).await.unwrap_err().code, ErrorCode::OutOfRange);
    assert_eq!(cast(-9.3e18).await.unwrap_err().code, ErrorCode::OutOfRange);
    assert_eq!(cast(9.2e18).await?.get::<i64>("n")?, 9_200_000_000_000_000_000);
    assert_eq!(cast(-9_223_372_036_854_775_808.0).await?.get::<i64>("n")?, i64::MIN);
    Ok(())
}

// R-T6, R-Q13: JSON made by wrapping JSON cannot grow deeper than JSON
// read from a client may be, so what is stored can always be read back,
// and dumped.
#[tokio::test(flavor = "multi_thread")]
async fn json_cannot_be_wrapped_deeper_than_it_can_be_read() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(TableDef::new("docs").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("j", Type::Json)).primary_key(["id"])).await?;
    let nested = |depth: usize| (0..depth).fold(json!("x"), |inner, _| json!([inner]));
    // As deep as it gets.
    db.insert(Insert::into("docs").set("id", 1).set("j", nested(100))).await?;
    // One more level, however it is added, is refused, and the row stays
    // as it was.
    let wrap = db.update(Update::table("docs").key([1]).set("j", call(Func::JsonArray, [col("j")]))).await.unwrap_err();
    assert_eq!(wrap.code, ErrorCode::OutOfRange);
    let object = db.update(Update::table("docs").key([1]).set("j", call(Func::JsonObject, [val("k"), col("j")]))).await.unwrap_err();
    assert_eq!(object.code, ErrorCode::OutOfRange);
    assert_eq!(db.query_one(Query::table("docs").key([1]).select("w", call(Func::JsonArray, [col("j")]))).await.unwrap_err().code, ErrorCode::OutOfRange);
    assert_eq!(db.get("docs", [1]).await?.unwrap().get::<Json>("j")?, nested(100));
    // Wrapping what is shallower works, up to the limit and no further.
    db.insert(Insert::into("docs").set("id", 2).set("j", nested(98))).await?;
    for _ in 0..2 {
        db.update(Update::table("docs").key([2]).set("j", call(Func::JsonArray, [col("j")]))).await?;
    }
    assert_eq!(db.update(Update::table("docs").key([2]).set("j", call(Func::JsonArray, [col("j")]))).await.unwrap_err().code, ErrorCode::OutOfRange);
    assert_eq!(db.get("docs", [2]).await?.unwrap().get::<Json>("j")?, nested(100));
    // And the database can still be dumped.
    let mut dump = Vec::new();
    assert!(db.admin().dump(&[], &mut dump).await? > 0);
    Ok(())
}

// R-O2, R-X1: a connection to a database that has since been dropped, or
// replaced by another of the same name, can neither commit to it nor
// dump what took its place.
#[tokio::test(flavor = "multi_thread")]
async fn a_connection_does_not_outlive_its_database() -> DbResult<()> {
    let server = TestServer::start().await;
    let admin = server.admin().await;
    let old = server.client("shop").await;
    old.create_table(TableDef::new("t").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    let tx = old.begin().await?;
    tx.insert(Insert::into("t").set("id", 1)).await?;

    admin.admin().drop_database("shop").await?;
    admin.admin().create_database("shop", "another-password").await?;

    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::Unavailable);
    assert_eq!(old.query(Query::table("t")).await.unwrap_err().code, ErrorCode::Unavailable);
    let mut sink = Vec::new();
    assert_eq!(old.admin().dump(&[], &mut sink).await.unwrap_err().code, ErrorCode::Unavailable);
    assert!(sink.is_empty());
    // The new database is empty and takes its own password.
    let new = Client::connect_url(&format!("database://shop:another-password@127.0.0.1:{}", server.port)).await?;
    assert!(new.describe().await?.is_empty());
    Ok(())
}

// R-X9: a call given up on midway does not leave its answer to be taken
// for the next one's.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_call_ends_its_transaction() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(TableDef::new("t").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    db.insert(Insert::into("t").set("id", 1)).await?;
    let tx = db.begin().await?;
    tx.insert(Insert::into("t").set("id", 2)).await?;
    // Sent, and dropped before the answer could arrive.
    let slow = Query::table("t").select("n", count());
    let abandoned = tokio::time::timeout(Duration::from_nanos(1), tx.query(slow)).await;
    if abandoned.is_err() {
        // Every later call on the transaction fails; none reads the
        // count as if it were its own answer.
        assert_eq!(tx.query(Query::table("t")).await.unwrap_err().code, ErrorCode::Connection);
        assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::Connection);
        // Nothing of the transaction was kept, and the client goes on
        // with other connections.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(column::<i64>(&db.query(Query::table("t")).await?, "id"), [1]);
    } else {
        // The answer came within a nanosecond: nothing was abandoned.
        tx.rollback().await?;
    }
    Ok(())
}

// R-O2: before a connection has said who it is, it is given room for a
// greeting and no more.
#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_cannot_make_the_server_hold_a_large_frame() -> DbResult<()> {
    let server = TestServer::start().await;
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    // A megabyte announced as the first frame: the connection is closed
    // at the length, before any of it is read.
    stream.write_all(&(1024u32 * 1024).to_le_bytes()).await.unwrap();
    let mut answer = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut answer)).await.expect("the server answers or closes");
    assert!(matches!(closed, Ok(0) | Err(_)), "the connection was closed");
    // The server is none the worse for it.
    server.admin().await.ping().await?;
    Ok(())
}
