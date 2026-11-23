//! The shortest path through everything: a table, a row in, a row out.

mod common;
use common::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_row_written_is_read_back() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(
        TableDef::new("users")
            .column(ColumnDef::new("id", Type::Int).auto_increment())
            .column(ColumnDef::new("name", Type::Text))
            .column(ColumnDef::new("created", Type::Timestamp).default_now())
            .primary_key(["id"])
            .unique("users_name_key", ["name"]),
    )
    .await?;

    let inserted = db.insert(Insert::into("users").set("name", "ann").returning(["id", "created"])).await?;
    assert_eq!(inserted.affected(), 1);
    let id: i64 = inserted.one()?.get("id")?;

    let row = db.get("users", [id]).await?.expect("the row is there");
    assert_eq!(row.get::<String>("name")?, "ann");

    let by_name = db.query_one(Query::table("users").index("users_name_key", KeyRange::eq(["ann"]))).await?;
    assert_eq!(by_name.get::<i64>("id")?, id);

    let duplicate = db.insert(Insert::into("users").set("name", "ann")).await.unwrap_err();
    assert_eq!(duplicate.code, ErrorCode::UniqueViolation);
    assert!(duplicate.violates("users_name_key"));
    Ok(())
}
