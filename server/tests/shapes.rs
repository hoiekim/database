//! Shapes of request that more than one app's suite asked for, tested
//! once here: a query of no table, a whole row returned from a write, a
//! missing value given as a key, a JSON array read as a list of text.

mod common;
use common::*;

async fn people(db: &Client) -> DbResult<()> {
    db.create_table(
        TableDef::new("people")
            .column(ColumnDef::new("id", Type::Int).auto_increment())
            .column(ColumnDef::new("name", Type::Text))
            .column(ColumnDef::new("team", Type::Text).nullable())
            .column(ColumnDef::new("emails", Type::Json).default(serde_json::json!([])))
            .column(ColumnDef::new("joined", Type::Timestamp).default_now())
            .primary_key(["id"])
            .unique("people_name", ["name"])
            .index(IndexDef::new("people_team").on(["team", "id"])),
    )
    .await?;
    for (name, team) in [("ann", Some("red")), ("bob", Some("red")), ("cat", None), ("dan", None), ("eve", Some("blue"))] {
        db.insert(Insert::into("people").set("name", name).set("team", team)).await?;
    }
    Ok(())
}

// R-Q10, R-O3: SQL's `SELECT EXISTS(..)`, `SELECT now()`, `SELECT f(x)`:
// one row computed from no table.
#[tokio::test(flavor = "multi_thread")]
async fn a_query_of_no_table_computes_one_row() -> DbResult<()> {
    let (_server, db) = fresh().await;
    people(&db).await?;
    let taken = |name: &'static str| exists(Query::table("people").index("people_name", KeyRange::eq([name])));
    let row = db
        .query_one(
            Query::row()
                .select("ann", taken("ann"))
                .select("zed", taken("zed"))
                .select("reds", scalar(Query::table("people").index("people_team", KeyRange::eq(["red"])).select("n", count())))
                .select("at", now())
                .select("likeness", call(Func::Similarity, [val("word"), val("two words")])),
        )
        .await?;
    assert_eq!((row.get::<bool>("ann")?, row.get::<bool>("zed")?, row.get::<i64>("reds")?), (true, false, 2));
    assert!((row.get::<f64>("likeness")? - 0.363_636_37).abs() < 1e-6);
    assert!(row.get::<Timestamp>("at")? > ts("2001-01-01T00:00:00Z"));

    // It is one row whatever is asked of it: filtered away, counted,
    // or unselected, when it has no columns at all.
    assert!(db.query(Query::row().filter(taken("zed")).select("x", val(1))).await?.is_empty());
    assert_eq!(db.query_one(Query::row().select("n", count())).await?.get::<i64>("n")?, 1);
    let bare = db.query(Query::row()).await?;
    assert_eq!((bare.len(), bare.columns().len()), (1, 0));
    // A list computed from nothing can still be spread into rows.
    let spread = db.query(Query::row().each("item", list([val("a"), val("b"), val("a")])).select("item", col("item"))).await?;
    assert_eq!(column::<String>(&spread, "item"), ["a", "b"]);
    // In a transaction it reads what the transaction has written.
    let tx = db.begin().await?;
    tx.insert(Insert::into("people").set("name", "zed")).await?;
    assert!(tx.query_one(Query::row().select("zed", taken("zed"))).await?.get::<bool>("zed")?);
    tx.rollback().await?;
    // There is nothing in it to write to.
    let err = db.update(Update::rows(Query::row()).set("x", 1)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    Ok(())
}

// R-W7: SQL's `RETURNING *` from each kind of write.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_can_return_the_whole_row() -> DbResult<()> {
    let (_server, db) = fresh().await;
    people(&db).await?;
    let names = ["id", "name", "team", "emails", "joined"];

    let inserted = db.insert(Insert::into("people").set("name", "fay").returning_all()).await?;
    assert_eq!(inserted.columns(), names);
    let row = inserted.one()?;
    assert_eq!((row.get::<i64>("id")?, row.get::<String>("name")?, row.is_null("team")?, row.get::<Json>("emails")?), (6, "fay".to_string(), true, serde_json::json!([])));

    // An upsert that updates returns the row as it now is.
    let upserted = db.insert(Insert::into("people").set("name", "fay").set("team", "blue").on_conflict_update("people_name").update_excluded(["team"]).returning_all()).await?;
    let row = upserted.one()?;
    assert_eq!((row.get::<i64>("id")?, row.get::<Option<String>>("team")?), (6, Some("blue".to_string())));

    let updated = db.update(Update::table("people").index("people_team", KeyRange::eq(["red"])).set("team", "green").returning_all()).await?;
    assert_eq!((updated.columns().len(), updated.len()), (5, 2));
    assert_eq!(column::<String>(&updated, "team"), ["green", "green"]);
    assert_eq!(column::<String>(&updated, "name"), ["ann", "bob"]);

    let deleted = db.delete(Delete::from("people").key([1]).returning_all()).await?;
    assert_eq!(deleted.columns(), names);
    assert_eq!(deleted.one()?.get::<String>("name")?, "ann");

    // A column added later is among them; a dropped one is not.
    db.add_column("people", ColumnDef::new("nick", Type::Text).default("-")).await?;
    db.drop_column("people", "joined").await?;
    let later = db.delete(Delete::from("people").key([2]).returning_all()).await?;
    assert_eq!(later.columns(), ["id", "name", "team", "emails", "nick"]);
    assert_eq!(later.one()?.get::<String>("nick")?, "-");
    Ok(())
}

// R-Q1, R-Q2, R-Q9: `WHERE key = $1` with a NULL parameter finds nothing,
// as in SQL; `IS NULL` and `IS NOT DISTINCT FROM` are asked for by name.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_value_given_as_a_key_finds_nothing() -> DbResult<()> {
    let (_server, db) = fresh().await;
    people(&db).await?;
    let none: Option<&str> = None;
    let names = |rows: Rows| column::<String>(&rows, "name");

    // By index: two people have no team, and `= NULL` is not how to ask.
    assert!(db.query(Query::table("people").index("people_team", KeyRange::eq([none]))).await?.is_empty());
    assert_eq!(names(db.query(Query::table("people").index("people_team", KeyRange::is([none]))).await?), ["cat", "dan"]);
    assert_eq!(names(db.query(Query::table("people").index("people_team", KeyRange::is([Some("red")]))).await?), ["ann", "bob"]);
    // A whole key: the team that is missing, and the id.
    assert_eq!(names(db.query(Query::table("people").index("people_team", KeyRange::is([val(none), val(4)]))).await?), ["dan"]);
    assert!(db.query(Query::table("people").index("people_team", KeyRange::eq([val(none), val(4)]))).await?.is_empty());
    // A NULL at either end of a range empties it.
    let from_null = KeyRange::new(Bound::included([none]), Bound::Unbounded);
    assert!(db.query(Query::table("people").index("people_team", from_null.clone())).await?.is_empty());
    assert_eq!(names(db.query(Query::table("people").index("people_team", from_null.nulls())).await?), ["cat", "dan"]);
    let to_null = KeyRange::new(Bound::Unbounded, Bound::excluded([none]));
    assert!(db.query(Query::table("people").index("people_team", to_null.clone())).await?.is_empty());
    assert_eq!(names(db.query(Query::table("people").index("people_team", to_null.nulls())).await?), ["eve", "ann", "bob"]);
    // An open range runs through the missing keys, which sort last.
    assert_eq!(names(db.query(Query::table("people").index("people_team", KeyRange::all())).await?), ["eve", "ann", "bob", "cat", "dan"]);

    // By primary key, and in the writes that find their rows by key.
    let no_id: Option<i64> = None;
    assert!(db.get("people", [no_id]).await?.is_none());
    assert_eq!(db.update(Update::table("people").key([no_id]).set("team", "x")).await?.affected(), 0);
    assert_eq!(db.update(Update::table("people").index("people_team", KeyRange::eq([none])).set("team", "x")).await?.affected(), 0);
    assert_eq!(db.delete(Delete::from("people").index("people_team", KeyRange::eq([none]))).await?.affected(), 0);
    assert_eq!(db.query(Query::table("people")).await?.len(), 5);

    // A join on a key that is missing matches no row.
    db.create_table(TableDef::new("teams").column(ColumnDef::new("name", Type::Text)).column(ColumnDef::new("motto", Type::Text).nullable()).primary_key(["name"])).await?;
    db.insert(Insert::into("teams").set("name", "red").set("motto", "go")).await?;
    let joined = db
        .query(Query::table("people").alias("p").left_join(Query::table("teams").alias("t").key([col("p.team")])).select("name", col("p.name")).select("motto", col("t.motto")))
        .await?;
    assert_eq!(column::<Option<String>>(&joined, "motto"), [Some("go".to_string()), Some("go".to_string()), None, None, None]);
    let inner = db.query(Query::table("people").alias("p").join(Query::table("teams").alias("t").key([col("p.team")])).select("name", col("p.name"))).await?;
    assert_eq!(names(inner), ["ann", "bob"]);
    Ok(())
}

// R-Q13, R-Q14: `jsonb_array_elements_text`.
#[tokio::test(flavor = "multi_thread")]
async fn a_json_array_reads_as_a_list_of_text() -> DbResult<()> {
    let (_server, db) = fresh().await;
    people(&db).await?;
    db.update(Update::table("people").key([1]).set("emails", serde_json::json!(["a@x.io", "b@x.io", 7, null, {"k": [1, 2]}, true]))).await?;
    db.update(Update::table("people").key([2]).set("emails", serde_json::json!({"not": "an array"}))).await?;
    let rows = db.query(Query::table("people").range(KeyRange::new(Bound::Unbounded, Bound::included([3]))).select("list", col("emails").json_texts())).await?;
    assert_eq!(
        column::<Vec<Option<String>>>(&rows, "list"),
        [
            vec![Some("a@x.io".to_string()), Some("b@x.io".to_string()), Some("7".to_string()), None, Some("{\"k\": [1, 2]}".to_string()), Some("true".to_string())],
            vec![],
            vec![],
        ]
    );
    // As a list it can be searched, spread and counted.
    let found = db.query(Query::table("people").filter(val("b@x.io").in_list(col("emails").json_texts())).columns(["name"])).await?;
    assert_eq!(column::<String>(&found, "name"), ["ann"]);
    let spread = db.query(Query::table("people").key([1]).each("email", col("emails").json_texts()).filter(col("email").like("%@%")).select("email", col("email"))).await?;
    assert_eq!(column::<String>(&spread, "email"), ["a@x.io", "b@x.io"]);
    Ok(())
}
