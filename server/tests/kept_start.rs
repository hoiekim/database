//! The start of a large value kept in its row (R-T10).
//!
//! A value too large for its row lives outside it, and a column may ask
//! that its first bytes stay behind (`ColumnDef::keep_start`), so that
//! `left`, `substr` and `substr_bytes` from the start are answered without
//! a second lookup. Nothing a client can ask may tell the two apart: every
//! read here is made of the same rows in a table that keeps starts and in
//! one that does not, and must answer the same, and as the text itself
//! says. That the start is in fact read from the row is shown where the
//! row is in hand, in the engine's own tests (`row.rs`, `eval.rs`).

mod common;
use common::*;

/// A table of notes whose two large columns keep `keep` bytes of their
/// values' starts, or none.
fn notes(name: &str, keep: u32) -> TableDef {
    let large = |column: ColumnDef| if keep > 0 { column.keep_start(keep) } else { column };
    TableDef::new(name)
        .column(ColumnDef::new("id", Type::Int))
        .column(ColumnDef::new("title", Type::Text).default(""))
        .column(large(ColumnDef::new("body", Type::Text).nullable()))
        .column(large(ColumnDef::new("raw", Type::Bytes).nullable()))
        .primary_key(["id"])
}

/// Texts whose characters are of every width, long and short, so that a
/// start cut after any number of bytes meets a character it may not cut.
fn bodies() -> Vec<Option<String>> {
    vec![
        None,
        Some(String::new()),
        Some("short enough to stay in the row".into()),
        Some("x".repeat(100)),
        Some("x".repeat(5_000)),
        Some("é".repeat(3_000)),
        Some(format!("a{}", "é".repeat(3_000))),
        Some("日本語のメール".repeat(600)),
        Some("📧".repeat(1_500)),
        Some("Grüße aus Köln, naïve café — 日本語 📧\r\n".repeat(400)),
        // Exactly as many bytes as a start of 100 or of 256 holds, then more.
        Some(format!("{}{}", "y".repeat(100), "é".repeat(2_000))),
        Some(format!("{}{}", "y".repeat(255), "日".repeat(2_000))),
        // Large, but not by much: a few hundred bytes more than a row has.
        Some("z".repeat(2_100)),
    ]
}

fn raws() -> Vec<Option<Vec<u8>>> {
    vec![None, Some(Vec::new()), Some(vec![7; 40]), Some((0..6_000u32).map(|n| (n * 7 + n / 256) as u8).collect()), Some(vec![0xFF; 2_500])]
}

async fn fill(db: &Client, table: &str) -> DbResult<i64> {
    let (bodies, raws) = (bodies(), raws());
    let mut id = 0;
    for body in &bodies {
        for raw in &raws {
            id += 1;
            db.insert(Insert::into(table).set("id", id).set("title", format!("note {id}")).set("body", body.clone()).set("raw", raw.clone())).await?;
        }
    }
    Ok(id)
}

/// Every row of a query, as values.
async fn all(db: &impl Db, query: Query) -> DbResult<Vec<Vec<Value>>> {
    Ok(db.query(query).await?.iter().map(|row| row.values().to_vec()).collect())
}

const TABLES: [(&str, u32); 4] = [("plain", 0), ("kept_100", 100), ("kept_256", 256), ("kept_most", 1_024)];

// R-T10, R-T8, R-Q15
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_start_changes_no_answer() -> DbResult<()> {
    let (_server, db) = fresh().await;
    for (table, keep) in TABLES {
        db.create_table(notes(table, keep)).await?;
        fill(&db, table).await?;
    }
    let texts: Vec<Option<String>> = bodies().into_iter().flat_map(|body| raws().into_iter().map(move |_| body.clone())).collect();
    let bytes: Vec<Option<Vec<u8>>> = bodies().into_iter().flat_map(|_| raws()).collect();

    // The first n characters, for n on every side of what a start holds.
    for n in [0i64, 1, 2, 3, 49, 50, 51, 99, 100, 101, 127, 128, 200, 255, 256, 257, 400, 1_023, 1_024, 1_025, 2_099, 2_100, 6_000, 100_000] {
        let want: Vec<Vec<Value>> = texts.iter().map(|text| vec![text.as_ref().map(|t| t.chars().take(n as usize).collect::<String>()).into()]).collect();
        for (table, _) in TABLES {
            assert_eq!(all(&db, Query::table(table).select("head", col("body").left(n))).await?, want, "left({n}) of {table}");
        }
    }
    // A stretch of characters: from the start, from before it, from inside
    // the start, across its end, beyond it.
    for (start, len) in [(1i64, 0i64), (1, 1), (1, 50), (1, 100), (1, 101), (1, 256), (1, 300), (0, 10), (-5, 20), (2, 30), (51, 49), (51, 50), (90, 20), (100, 1), (101, 1), (200, 56), (200, 57), (257, 10), (1_000, 24), (1_000, 25), (3_000, 12_000), (1, i64::MAX), (i64::MAX, 5)] {
        let want: Vec<Vec<Value>> = texts
            .iter()
            .map(|text| {
                let end = start.saturating_add(len);
                let from = start.max(1);
                vec![text.as_ref().map(|t| if end <= from { String::new() } else { t.chars().skip((from - 1) as usize).take((end - from) as usize).collect() }).into()]
            })
            .collect();
        for (table, _) in TABLES {
            assert_eq!(all(&db, Query::table(table).select("part", col("body").substr(start, len))).await?, want, "substr({start}, {len}) of {table}");
        }
    }
    // A stretch of bytes, of the bytes column and of the text's own bytes.
    for (start, len) in [(1i64, 0i64), (1, 1), (1, 40), (1, 99), (1, 100), (1, 101), (1, 256), (1, 257), (0, 50), (-3, 10), (50, 50), (50, 51), (100, 1), (101, 100), (250, 6), (250, 7), (1_020, 4), (1_020, 5), (5_990, 100), (1, 100_000)] {
        let cut = |all: &[u8]| -> Vec<u8> {
            let end = (start.saturating_add(len.max(0)) - 1).clamp(0, all.len() as i64) as usize;
            let from = ((start.max(1) - 1) as usize).min(end);
            all[from..end].to_vec()
        };
        let of_raw: Vec<Vec<Value>> = bytes.iter().map(|raw| vec![raw.as_deref().map(cut).into()]).collect();
        let of_text: Vec<Vec<Value>> = texts.iter().map(|text| vec![text.as_ref().map(|t| cut(t.as_bytes())).into()]).collect();
        for (table, _) in TABLES {
            assert_eq!(all(&db, Query::table(table).select("part", col("raw").substr_bytes(start, len))).await?, of_raw, "substr_bytes({start}, {len}) of raw in {table}");
            assert_eq!(all(&db, Query::table(table).select("part", col("body").substr_bytes(start, len))).await?, of_text, "substr_bytes({start}, {len}) of body in {table}");
        }
    }
    // Lengths, the values whole, and a start used to choose and to order
    // rows: all as in the table that keeps none.
    let plain = |query: fn(&str) -> Query| query("plain");
    let questions: [fn(&str) -> Query; 5] = [
        |table| Query::table(table).select("octets", col("body").octet_length()).select("characters", col("body").length()).select("raw_octets", col("raw").octet_length()),
        |table| Query::table(table),
        |table| Query::table(table).filter(col("body").left(3).eq("ééé")).columns(["id"]),
        |table| Query::table(table).filter(col("raw").substr_bytes(1, 2).eq(Value::Bytes(vec![0xFF, 0xFF]))).select("n", count()),
        |table| Query::table(table).order_by(col("body").left(120).desc().nulls_last()).order_by(col("id")).select("id", col("id")).select("lower", col("body").left(40).lower()),
    ];
    for (n, question) in questions.into_iter().enumerate() {
        let want = all(&db, plain(question)).await?;
        assert!(!want.is_empty());
        for (table, _) in &TABLES[1..] {
            assert!(all(&db, question(table)).await? == want, "question {n} of {table}");
        }
    }
    db.admin().check().await?;
    Ok(())
}

// R-T10, R-W6, R-W4
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_start_follows_its_value_through_every_write() -> DbResult<()> {
    let (_server, db) = fresh().await;
    for (table, keep) in TABLES {
        db.create_table(notes(table, keep)).await?;
    }
    let head = |table: &'static str| Query::table(table).select("id", col("id")).select("head", col("body").left(60)).select("bytes", col("raw").substr_bytes(1, 30)).select("octets", col("body").octet_length());
    let first = "α".repeat(2_500);
    let second = "ω".repeat(4_000);
    let mut want = Vec::new();
    for (step, (table, _)) in (0..).zip(TABLES.iter().cycle().take(TABLES.len() * 9)) {
        let step = step / TABLES.len();
        match step {
            // A large value, then another in its place, then the first again.
            0 => drop(db.insert(Insert::into(table).set("id", 1).set("body", first.clone()).set("raw", vec![1u8; 3_000])).await?),
            1 => drop(db.update(Update::table(table).key([1]).set("body", second.clone())).await?),
            // Another column changes: the row is written again around the
            // reference it holds.
            2 => drop(db.update(Update::table(table).key([1]).set("title", "t".repeat(300))).await?),
            // A value small enough for the row, a large one again, none.
            3 => drop(db.update(Update::table(table).key([1]).set("body", "small").set("raw", vec![2u8; 10])).await?),
            4 => drop(db.update(Update::table(table).key([1]).set("body", first.clone()).set("raw", vec![3u8; 5_000])).await?),
            5 => drop(db.update(Update::table(table).key([1]).set("body", Value::Null)).await?),
            // An upsert that replaces the value from the row it proposed,
            // and one that keeps it.
            6 => drop(db.insert(Insert::into(table).set("id", 1).set("body", second.clone()).on_conflict_update(PRIMARY).update_excluded(["body"])).await?),
            7 => drop(db.insert(Insert::into(table).set("id", 1).set("body", "unused").on_conflict_update(PRIMARY).update("title", "kept")).await?),
            // A row copied by a query that reads it whole.
            _ => drop(db.insert(Insert::into(table).columns(["id", "title", "body", "raw"]).from_query(Query::table(table).key([1]).select("id", col("id") + 10).columns(["title", "body", "raw"]))).await?),
        }
        let got = all(&db, head(table)).await?;
        if *table == "plain" {
            want = got;
            assert!(!want.is_empty());
        } else {
            assert_eq!(got, want, "step {step} in {table}");
        }
    }
    // What the last steps left, read back whole.
    for (table, _) in TABLES {
        let rows = db.query(Query::table(table)).await?;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.get::<String>("body").unwrap() == second && row.get::<Vec<u8>>("raw").unwrap() == vec![3u8; 5_000]));
    }
    // A deleted row takes its value with it, start or no start.
    for (table, _) in TABLES {
        assert_eq!(db.delete(Delete::from(table).returning_expr("head", col("body").left(2))).await?.len(), 2);
    }
    assert!(db.admin().check().await?.contains("0 rows"));
    Ok(())
}

// R-T10, R-S1, R-O1, R-X10
#[tokio::test(flavor = "multi_thread")]
async fn the_setting_is_part_of_the_definition_wherever_that_is_kept() -> DbResult<()> {
    let server = TestServer::start().await;
    let db = server.client("test").await;
    db.create_table(notes("kept", 100)).await?;
    db.create_table(notes("plain", 0)).await?;
    let rows = fill(&db, "kept").await?;
    fill(&db, "plain").await?;

    // A column added to a table with rows: they read NULL, and what is
    // written from then on keeps its start.
    let summary = ColumnDef::new("summary", Type::Text).nullable().keep_start(64);
    db.add_column("kept", summary.clone()).await?;
    db.add_column("plain", ColumnDef::new("summary", Type::Text).nullable()).await?;
    for table in ["kept", "plain"] {
        db.update(Update::table(table).key([5]).set("summary", "ß".repeat(2_000))).await?;
        db.insert(Insert::into(table).set("id", 1_000).set("summary", "s".repeat(9_000)).set("body", "b".repeat(9_000))).await?;
    }
    let read = |table: &'static str| {
        Query::table(table).select("id", col("id")).select("summary", col("summary").left(50)).select("body", col("body").left(80)).select("raw", col("raw").substr_bytes(1, 60)).select("all", col("summary").length())
    };
    let want = all(&db, read("plain")).await?;
    assert_eq!(want.len() as i64, rows + 1);
    assert!(all(&db, read("kept")).await? == want);

    // It is in the definition a client reads back.
    let described = |tables: Vec<TableDef>| -> Vec<(String, u32)> {
        let kept = tables.into_iter().find(|t| t.name == "kept").expect("the table");
        kept.columns.iter().map(|c| (c.name.clone(), c.keep_start)).collect()
    };
    let expected: Vec<(String, u32)> = [("id", 0), ("title", 0), ("body", 100), ("raw", 100), ("summary", 64)].into_iter().map(|(name, keep)| (name.to_string(), keep)).collect();
    assert_eq!(described(db.describe().await?), expected);

    // It is in the catalog a checkpoint stores, which a server that was
    // stopped starts from. (That it is in the log too, for one that was
    // killed first, is `engine/tests/recovery.rs`'s to show: its table
    // keeps a start.)
    let dump = {
        let mut out = Vec::new();
        server.admin().await.admin().dump(&[], &mut out).await?;
        out
    };
    drop(db);
    let dir = server.restart_dir().await;
    let server = TestServer::start_with(dir, |_| {}).await;
    let db = server.client("test").await;
    assert_eq!(described(db.describe().await?), expected);
    assert!(all(&db, read("kept")).await? == want);
    // Written to and started once more, it is still there...
    db.admin().checkpoint().await?;
    db.insert(Insert::into("kept").set("id", 2_000).set("body", "after".repeat(1_000))).await?;
    db.delete(Delete::from("kept").key([2_000])).await?;
    drop(db);
    let dir = server.restart_dir().await;
    let server = TestServer::start_with(dir, |_| {}).await;
    let db = server.client("test").await;
    assert_eq!(described(db.describe().await?), expected);
    assert!(all(&db, read("kept")).await? == want);
    db.admin().check().await?;

    // ...and a dump, restored on another server.
    let other = TestServer::start().await;
    other.admin().await.admin().restore(&mut dump.as_slice(), false).await?;
    let restored = Client::connect_url(&other.url("test")).await?;
    assert_eq!(described(restored.describe().await?), expected);
    assert!(all(&restored, read("kept")).await? == want);
    restored.admin().check().await?;
    Ok(())
}

// R-T10, R-S2
#[tokio::test(flavor = "multi_thread")]
async fn a_start_is_kept_only_where_it_can_be() -> DbResult<()> {
    let (_server, db) = fresh().await;
    // Only text and bytes have a start to keep, and a row has only so
    // much room.
    for (ty, keep) in [(Type::Int, 16), (Type::Json, 16), (Type::list(Type::Text), 16), (Type::Uuid, 16), (Type::Text, 1_025), (Type::Bytes, 100_000)] {
        let column = ColumnDef::new("c", ty.clone()).nullable().keep_start(keep);
        let e = db.create_table(TableDef::new("t").column(ColumnDef::new("id", Type::Int)).column(column.clone()).primary_key(["id"])).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest, "{ty:?} keeping {keep}: {e}");
        db.create_table_if_not_exists(TableDef::new("u").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
        assert_eq!(db.add_column("u", column).await.unwrap_err().code, ErrorCode::InvalidRequest);
    }
    assert!(db.describe_table("t").await?.is_none());

    // A row whose key leaves it little room keeps what fits of a start,
    // or none of it, and reads as any other. The key here is 1,800 bytes
    // of the 1,900 a key may be.
    for (table, keep) in [("tight", 1_024), ("tight_plain", 0)] {
        let body = ColumnDef::new("body", Type::Text);
        db.create_table(TableDef::new(table).column(ColumnDef::new("name", Type::Text)).column(if keep > 0 { body.keep_start(keep) } else { body }).column(ColumnDef::new("raw", Type::Bytes).keep_start(1_024)).primary_key(["name"])).await?;
        for n in 0..4 {
            let name = format!("{n}{}", "k".repeat(1_799));
            db.insert(Insert::into(table).set("name", name).set("body", "日本".repeat(1_000 + n)).set("raw", vec![n as u8; 3_000])).await?;
        }
    }
    for n in [0i64, 1, 5, 30, 60, 500] {
        let read = |table: &'static str| Query::table(table).select("head", col("body").left(n)).select("bytes", col("raw").substr_bytes(1, n)).select("size", col("body").octet_length());
        let want = all(&db, read("tight_plain")).await?;
        assert_eq!(want.len(), 4);
        assert_eq!(all(&db, read("tight")).await?, want, "the first {n}");
    }
    db.admin().check().await?;
    Ok(())
}
