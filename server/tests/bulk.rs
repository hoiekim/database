//! Large statements: their texts cut into words before their turn to
//! write, and the memory they may hold while in flight.

mod common;
use common::*;

use std::time::Duration;

fn notes() -> TableDef {
    TableDef::new("notes")
        .column(ColumnDef::new("id", Type::Int))
        .column(ColumnDef::new("title", Type::Text).default(""))
        .column(ColumnDef::new("body", Type::Text).default("fallback words"))
        .primary_key(["id"])
        .index(IndexDef::new("notes_search").full_text(TextConfig::English, [(col("title"), 2.0), (col("body"), 1.0)]))
}

async fn found(db: &Client, words: &str) -> DbResult<Vec<i64>> {
    let rows = db.query(Query::table("notes").search("notes_search", Vec::<Value>::new(), words).columns(["id"])).await?;
    let mut ids = column::<i64>(&rows, "id");
    ids.sort();
    Ok(ids)
}

/// Words enough to make a statement of a few hundred rows larger than a
/// request of ordinary size.
fn padding() -> String {
    "padding ".repeat(60)
}

// The words a row is indexed under are those of the text it is written
// with, whatever was cut from the statement before its turn came: a
// default in place of a text not given, an upsert that writes another
// text, a statement that fails and leaves nothing.
#[tokio::test(flavor = "multi_thread")]
async fn words_cut_ahead_are_those_of_the_row_as_written() -> DbResult<()> {
    let (server, db) = fresh().await;
    db.create_table(notes()).await?;
    for id in 0..10i64 {
        db.insert(Insert::into("notes").set("id", id).set("title", "old").set("body", "earlier")).await?;
    }

    // Rows 0 to 9 are there: the upsert writes another text for them.
    let mut insert = Insert::into("notes").columns(["id", "title", "body"]).on_conflict_update(PRIMARY).update("body", "rewritten text");
    for id in 0..300i64 {
        insert = insert.values([val(id), val(format!("title{id}")), val(format!("given{id} {}", padding()))]);
    }
    db.insert(insert).await?;
    assert_eq!(found(&db, "rewritten").await?, (0..10).collect::<Vec<_>>());
    assert!(found(&db, "given3").await?.is_empty(), "the upsert's text was not written");
    assert_eq!(found(&db, "given42").await?, [42]);
    // Words of two fields, each weighed as its field is.
    assert_eq!(found(&db, "title42 given42").await?, [42]);
    assert!(found(&db, "earlier").await?.is_empty());

    // A text not given: the default's words.
    let mut insert = Insert::into("notes").columns(["id", "title"]);
    for id in 1000..1300i64 {
        insert = insert.values([val(id), val(format!("untitled{id} {}", padding()))]);
    }
    db.insert(insert).await?;
    assert_eq!(found(&db, "fallback").await?, (1000..1300).collect::<Vec<_>>());

    // A statement that fails at its last row leaves no row and no word.
    let mut insert = Insert::into("notes").columns(["id", "body"]);
    for id in 2000..2300i64 {
        insert = insert.values([val(id), val(format!("doomed{id} {}", padding()))]);
    }
    insert = insert.values([val(5i64), val("taken")]);
    assert_eq!(db.insert(insert).await.unwrap_err().code, ErrorCode::UniqueViolation);
    assert!(found(&db, "doomed2001").await?.is_empty());
    assert_eq!(db.query(Query::table("notes")).await?.len(), 600);

    let report = server.admin().await.admin().check().await?;
    assert!(!report.is_empty());
    Ok(())
}

// The memory for large requests in flight is a budget that a request
// waits for. A connection that holds the turn to write never waits for
// it: whoever holds the budget may be waiting for that turn.
#[tokio::test(flavor = "multi_thread")]
async fn the_holder_of_the_turn_does_not_wait_for_memory_held_by_one_waiting_for_the_turn() -> DbResult<()> {
    let server = TestServer::start().await;
    let db = server.client("test").await;
    db.create_table(TableDef::new("blobs").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("data", Type::Text)).primary_key(["id"])).await?;
    let big = |from: i64, rows: i64| {
        let mut insert = Insert::into("blobs").columns(["id", "data"]);
        for id in from..from + rows {
            insert = insert.values([val(id), val("x".repeat(100_000))]);
        }
        insert
    };

    // An exclusive transaction holds the turn.
    let tx = db.begin().await?;
    tx.insert(Insert::into("blobs").set("id", 0i64).set("data", "first")).await?;

    // Another connection sends more than the whole budget, and waits for
    // the turn with all of it.
    let other = server.client("test").await;
    let waiting = tokio::spawn(async move { other.insert(big(1000, 60)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "it waits for the turn");

    // A request of ordinary size does not wait for the budget.
    let third = server.client("test").await;
    tokio::time::timeout(Duration::from_secs(5), third.query(Query::table("blobs"))).await.expect("a small request goes at once")?;

    // The holder of the turn sends a large request too, and is answered.
    tokio::time::timeout(Duration::from_secs(20), tx.insert(big(1, 20))).await.expect("the holder of the turn does not wait for the budget")?;
    tx.commit().await?;
    tokio::time::timeout(Duration::from_secs(30), waiting).await.expect("the large request goes once the turn is free").unwrap()?;
    assert_eq!(db.query(Query::table("blobs")).await?.len(), 81);
    Ok(())
}

// Nothing shared between connections is held while the server waits on a
// client's bytes. A connection that announces a frame larger than the
// whole budget and then sends nothing (a client gone quiet, or a large
// statement on a slow link) keeps no other connection's large request
// from being answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_frame_that_never_comes_holds_up_no_other_connection() -> DbResult<()> {
    use database_protocol::message::{encode_request, Request, PROTOCOL_VERSION};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let server = TestServer::start().await;
    let db = server.client("test").await;
    db.create_table(TableDef::new("blobs").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("data", Type::Text)).primary_key(["id"])).await?;

    // Signed in, then the length of 16 MB, and no more.
    let mut quiet = tokio::net::TcpStream::connect(("127.0.0.1", server.port)).await.unwrap();
    let hello = encode_request(&Request::Hello { version: PROTOCOL_VERSION, database: "test".into(), password: PASSWORD.into(), client: "quiet".into() }, 0);
    quiet.write_all(&(hello.len() as u32).to_le_bytes()).await.unwrap();
    quiet.write_all(&hello).await.unwrap();
    let mut len = [0u8; 4];
    quiet.read_exact(&mut len).await.unwrap();
    let mut greeting = vec![0u8; u32::from_le_bytes(len) as usize];
    quiet.read_exact(&mut greeting).await.unwrap();
    quiet.write_all(&(16u32 * 1024 * 1024).to_le_bytes()).await.unwrap();
    quiet.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Another connection's large insert is answered all the same.
    let mut insert = Insert::into("blobs").columns(["id", "data"]);
    for id in 0..20i64 {
        insert = insert.values([val(id), val("x".repeat(100_000))]);
    }
    tokio::time::timeout(Duration::from_secs(10), db.insert(insert)).await.expect("a quiet connection holds up no other")?;
    assert_eq!(db.query(Query::table("blobs")).await?.len(), 20);
    drop(quiet);
    Ok(())
}
