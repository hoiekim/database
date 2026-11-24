//! Large statements: their texts cut into words before their turn to
//! write.

mod common;
use common::*;

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
