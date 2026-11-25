//! What a statement cost: how many index entries and rows its way in
//! made the server read, and which way that was, named by the statement
//! or chosen by the server (R-O7). Asked for with `explain`, and written
//! to the server's log for a statement that was slow.

mod common;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::*;
use database_protocol::message::{decode_response, encode_request, Request, Response, CHOSEN_WAY_VERSION, OLDEST_PROTOCOL_VERSION, PROTOCOL_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 1,000 notes of 20 owners, 50 each, with an index by owner and day and
/// a full-text index by owner.
async fn notes(db: &Client) -> DbResult<()> {
    db.create_table(TableDef::new("owners").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("name", Type::Text)).primary_key(["id"])).await?;
    db.create_table(
        TableDef::new("notes")
            .column(ColumnDef::new("id", Type::Int))
            .column(ColumnDef::new("owner", Type::Int))
            .column(ColumnDef::new("day", Type::Int))
            .column(ColumnDef::new("body", Type::Text))
            .primary_key(["id"])
            .index(IndexDef::new("notes_owner_day").on(["owner", "day"]))
            .index(IndexDef::new("notes_search").on(["owner"]).full_text(TextConfig::Simple, [(col("body"), 1.0)])),
    )
    .await?;
    let mut owners = Insert::into("owners").columns(["id", "name"]);
    for id in 0..20 {
        owners = owners.values([val(id), val(format!("owner {id}"))]);
    }
    db.insert(owners).await?;
    for chunk in 0..4 {
        let mut insert = Insert::into("notes").columns(["id", "owner", "day", "body"]);
        for id in chunk * 250..(chunk + 1) * 250 {
            let word = if id % 100 == 0 { "rare" } else { "common" };
            insert = insert.values([val(id), val(id % 20), val(id / 20), val(format!("note {id} is {word}"))]);
        }
        db.insert(insert).await?;
    }
    Ok(())
}

/// The server as a process of its own, ended with the test whatever way
/// the test ends.
struct Process(Child);

impl Process {
    /// Asks the server to stop and waits until it has.
    fn stop(&mut self) {
        Command::new("kill").args(["-TERM", &self.0.id().to_string()]).status().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the server as its own process on an empty directory, with
/// `DATABASE_SLOW_MS` set to `slow` if that is given, and a client on a
/// new database named `test`. The server's log is gathered to be read
/// once it has stopped.
async fn logged(dir: &TempDir, slow: Option<&str>) -> (Process, std::thread::JoinHandle<Vec<String>>, Client) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_database"));
    command.arg("serve").env("DATABASE_DATA", dir.path()).env("DATABASE_LISTEN", format!("127.0.0.1:{port}")).env("DATABASE_ADMIN_PASSWORD", ADMIN_PASSWORD).env_remove("DATABASE_SLOW_MS").stderr(Stdio::piped());
    if let Some(slow) = slow {
        command.env("DATABASE_SLOW_MS", slow);
    }
    let mut server = Process(command.spawn().expect("the server binary starts"));
    let log = server.0.stderr.take().unwrap();
    let lines = std::thread::spawn(move || BufReader::new(log).lines().map_while(Result::ok).collect::<Vec<String>>());
    let admin = loop {
        let mut config = Config::new("127.0.0.1", port, "", ADMIN_PASSWORD);
        config.connect_timeout = Duration::from_secs(2);
        match Client::connect(config).await {
            Ok(admin) => break admin,
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    };
    admin.admin().create_database("test", PASSWORD).await.expect("the database is created");
    let mut config = Config::new("127.0.0.1", port, "test", PASSWORD);
    config.name = "the test".to_string();
    let db = Client::connect(config).await.expect("the client connects");
    (server, lines, db)
}

fn read(e: Explained) -> (u64, u64, u64) {
    (e.index_entries, e.rows_read, e.rows_returned)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_says_what_its_way_in_made_the_server_read() -> DbResult<()> {
    // R-O7
    let (_server, db) = fresh().await;
    notes(&db).await?;

    // A row by its key: one row read.
    assert_eq!(read(db.explain(Query::table("notes").key([7])).await?), (0, 1, 1));
    assert_eq!(read(db.explain(Query::table("notes").key([7_000])).await?), (0, 0, 0));
    // A filter over the whole table reads the whole table, however few
    // rows it keeps.
    assert_eq!(read(db.explain(Query::table("notes").whole().filter(col("owner").eq(3).and(col("day").eq(10)))).await?), (0, 1_000, 1));
    // The same rows through the index: the entries of the range, and a
    // row for each. The same again where no way in is named and the
    // server reads that range of its own accord.
    assert_eq!(read(db.explain(Query::table("notes").index("notes_owner_day", KeyRange::eq([3, 10]))).await?), (1, 1, 1));
    assert_eq!(read(db.explain(Query::table("notes").filter(col("owner").eq(3).and(col("day").eq(10)))).await?), (1, 1, 1));
    assert_eq!(read(db.explain(Query::table("notes").index("notes_owner_day", KeyRange::eq([3]))).await?), (50, 50, 50));
    // A range wider than what the filter keeps shows as the difference.
    assert_eq!(read(db.explain(Query::table("notes").index("notes_owner_day", KeyRange::eq([3])).filter(col("day").ge(45))).await?), (50, 50, 5));

    // A limit stops the read where no sort is asked for, and not where
    // one is: the whole range is read to be sorted.
    assert_eq!(read(db.explain(Query::table("notes").index("notes_owner_day", KeyRange::eq([3])).limit(10)).await?), (10, 10, 10));
    assert_eq!(read(db.explain(Query::table("notes").index("notes_owner_day", KeyRange::eq([3])).order_by(col("body")).limit(10)).await?), (50, 50, 10));
    assert_eq!(read(db.explain(Query::table("notes").limit(3)).await?), (0, 3, 3));

    // A join reads a row of the other table for each row so far; a
    // subquery in a filter likewise.
    let joined = Query::table("notes").alias("n").index("notes_owner_day", KeyRange::eq([3])).join(Query::table("owners").alias("o").key([col("n.owner")])).select("name", col("o.name"));
    assert_eq!(read(db.explain(joined).await?), (50, 100, 50));
    let with_subquery = Query::table("owners").filter(exists(Query::table("notes").index("notes_owner_day", KeyRange::eq([col("owners.id"), val(0)]))));
    assert_eq!(read(db.explain(with_subquery).await?), (20, 40, 20));

    // An aggregate returns one row for all it read.
    assert_eq!(read(db.explain(Query::table("notes").index("notes_owner_day", KeyRange::eq([3])).select("n", count())).await?), (50, 50, 1));
    // A search reads its hits from the index, then their rows.
    assert_eq!(read(db.explain(Query::table("notes").search("notes_search", [0], "rare")).await?), (10, 10, 10));
    assert_eq!(read(db.explain(Query::table("notes").search("notes_search", [0], "common")).await?), (40, 40, 40));

    // It says how long it took, and it took some time.
    let slow = db.explain(Query::table("notes").filter(col("body").like("%rare%"))).await?;
    assert_eq!(read(slow.clone()), (0, 1_000, 10));
    assert!(slow.took > Duration::ZERO && slow.took < Duration::from_secs(10), "{:?}", slow.took);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_says_which_way_in_it_took_and_who_chose_it() -> DbResult<()> {
    // R-O7, R-Q23
    let (_server, db) = fresh().await;
    notes(&db).await?;
    let way = |q: Query| {
        let db = &db;
        async move { db.explain(q).await.map(|e| e.way) }
    };
    // A way in the query names is shown as named.
    assert_eq!(way(Query::table("notes").key([7])).await?, r#""notes" by its primary key"#);
    assert_eq!(way(Query::table("notes").index("notes_owner_day", KeyRange::eq([3]))).await?, r#""notes" by index "notes_owner_day""#);
    assert_eq!(way(Query::table("notes").search("notes_search", [0], "rare")).await?, r#""notes" by a search of "notes_search""#);
    assert_eq!(way(Query::table("notes").whole().filter(col("owner").eq(3))).await?, r#""notes" by the whole table"#);
    // Where it names none, what the server chose, marked so: a range of
    // an index or of the primary key, a tree in its own order for a
    // sorted page, or the whole table when nothing fits.
    assert_eq!(way(Query::table("notes").filter(col("owner").eq(3))).await?, r#""notes" by index "notes_owner_day", chosen"#);
    assert_eq!(way(Query::table("notes").filter(col("id").lt(9))).await?, r#""notes" by its primary key, chosen"#);
    assert_eq!(way(Query::table("notes").filter(col("owner").eq(3)).order_by(col("day")).limit(5)).await?, r#""notes" by index "notes_owner_day", in its order, chosen"#);
    assert_eq!(way(Query::table("notes").filter(col("body").like("%rare%"))).await?, r#""notes" by the whole table, chosen"#);
    assert_eq!(way(Query::table("notes")).await?, r#""notes" by the whole table, chosen"#);
    // A join and a subquery each have a way in of their own: the
    // query's first, then each that was chosen.
    let joined = Query::table("notes").alias("n").index("notes_owner_day", KeyRange::eq([3])).join(Query::table("owners").alias("o").filter(col("o.id").eq(col("n.owner"))));
    let explained = db.explain(joined).await?;
    assert_eq!(explained.way, r#""notes" by index "notes_owner_day"; "owners" by its primary key, chosen"#);
    assert_eq!(read(explained), (50, 100, 50));
    let with_notes = Query::table("owners").filter(exists(Query::table("notes").filter(col("notes.owner").eq(col("owners.id")).and(col("notes.day").eq(0)))));
    assert_eq!(way(with_notes).await?, r#""owners" by the whole table, chosen; "notes" by index "notes_owner_day", chosen"#);
    assert_eq!(way(Query::row().select("n", scalar(Query::table("notes").filter(col("id").eq(1)).columns(["day"])))).await?, r#""notes" by its primary key, chosen"#);
    assert_eq!(way(Query::row().select("at", now())).await?, "");
    // A prepared query is chosen for each time it runs, by the values
    // it is run with: a NULL owner is no range at all.
    let by_owner = Prepared::new(Query::table("notes").filter(col("owner").eq(param(0))));
    let explained = db.explain_prepared(&by_owner, [4]).await?;
    assert_eq!((explained.way.as_str(), explained.rows_read), (r#""notes" by index "notes_owner_day", chosen"#, 50));
    let explained = db.explain_prepared(&by_owner, [Value::Null]).await?;
    assert_eq!((explained.way.as_str(), explained.rows_read), (r#""notes" by its primary key, chosen"#, 0));
    Ok(())
}

/// Sends frames as they are and reads one answer to each.
async fn raw(port: u16, frames: &[Vec<u8>]) -> Vec<Response> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut answers = Vec::new();
    for frame in frames {
        stream.write_all(&(frame.len() as u32).to_le_bytes()).await.unwrap();
        stream.write_all(frame).await.unwrap();
        let mut len = [0u8; 4];
        stream.read_exact(&mut len).await.unwrap();
        let mut answer = vec![0u8; u32::from_le_bytes(len) as usize];
        stream.read_exact(&mut answer).await.unwrap();
        answers.push(decode_response(&answer).unwrap());
    }
    answers
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_from_before_the_way_was_told_is_answered_as_it_always_was() -> DbResult<()> {
    // R-O7
    let (server, db) = fresh().await;
    notes(&db).await?;
    let hello = |version: u32| encode_request(&Request::Hello { version, database: "test".into(), password: PASSWORD.into(), client: "raw".into() }, 0);
    let explain = encode_request(&Request::Explain(Box::new(Request::Query(Query::table("notes").filter(col("owner").eq(3))))), 0);
    for version in OLDEST_PROTOCOL_VERSION..=PROTOCOL_VERSION {
        let answers = raw(server.port, &[hello(version), explain.clone()]).await;
        let Response::Rows { set, .. } = &answers[1] else { panic!("version {version}: {:?}", answers[1]) };
        // The query is chosen for whoever sends it: 50 entries, 50 rows.
        assert_eq!(set.rows[0][..3], [Value::Int(50), Value::Int(50), Value::Int(50)], "version {version}");
        if version >= CHOSEN_WAY_VERSION {
            assert_eq!(set.columns, ["index_entries", "rows_read", "rows_returned", "micros", "way"], "version {version}");
            assert_eq!(set.rows[0][4], Value::Text(r#""notes" by index "notes_owner_day", chosen"#.into()), "version {version}");
        } else {
            assert_eq!(set.columns, ["index_entries", "rows_read", "rows_returned", "micros"], "version {version}");
            assert_eq!(set.rows[0].len(), 4, "version {version}");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_told_not_to_choose_reads_the_whole_table_for_every_query_that_names_no_way_in() -> DbResult<()> {
    // R-Q23
    let server = TestServer::start_with(TempDir::new(), |config| config.plan = false).await;
    let db = server.client("test").await;
    notes(&db).await?;
    let untuned = Query::table("notes").filter(col("owner").eq(3));
    let explained = db.explain(untuned.clone()).await?;
    assert_eq!(explained.way, r#""notes" by the whole table"#);
    assert_eq!(read(explained), (0, 1_000, 50));
    assert_eq!(read(db.explain(untuned.clone().order_by(col("day")).limit(5)).await?), (0, 1_000, 5));
    // An update finds its rows the same way.
    assert_eq!(db.update(Update::table("notes").filter(col("owner").eq(3)).set("day", 0)).await?.affected(), 50);
    // A way in that is named is read as named, as everywhere.
    assert_eq!(read(db.explain(Query::table("notes").index("notes_owner_day", KeyRange::eq([4]))).await?), (50, 50, 50));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_statement_is_chosen_for_by_the_indexes_there_are_when_it_runs() -> DbResult<()> {
    // R-Q23, R-X11
    let server = TestServer::start().await;
    server.client("test").await;
    // One connection, so that the statement is kept by it throughout.
    let db = server.client_with("test", |config| config.pool_size = 1).await;
    notes(&db).await?;
    let by_day = Prepared::new(Query::table("notes").filter(col("day").eq(param(0))).columns(["id"]));
    let run = |day: i64| {
        let (db, by_day) = (&db, &by_day);
        async move {
            let ids: Vec<i64> = db.execute(by_day, [day]).await?.column()?;
            let explained = db.explain_prepared(by_day, [day]).await?;
            Ok::<_, DbError>((ids, explained.index_entries, explained.rows_read, explained.way))
        }
    };
    let ids: Vec<i64> = (200..220).collect();
    assert_eq!(run(10).await?, (ids.clone(), 0, 1_000, r#""notes" by the whole table, chosen"#.to_string()));
    // An index made after the statement was prepared serves it the next
    // time it runs; dropped, the statement reads the whole table again.
    // It answers the same throughout.
    db.create_index("notes", IndexDef::new("notes_day").on(["day"])).await?;
    assert_eq!(run(10).await?, (ids.clone(), 20, 20, r#""notes" by index "notes_day", chosen"#.to_string()));
    db.drop_index("notes", "notes_day").await?;
    assert_eq!(run(10).await?, (ids.clone(), 0, 1_000, r#""notes" by the whole table, chosen"#.to_string()));
    // A column the filter reads, dropped and made again as another
    // type: the statement is looked at afresh, and fails as the whole
    // table read does, on a text compared with an integer.
    db.drop_index("notes", "notes_owner_day").await?;
    db.drop_column("notes", "day").await?;
    db.add_column("notes", ColumnDef::new("day", Type::Text).default("ten")).await?;
    db.create_index("notes", IndexDef::new("notes_day").on(["day"])).await?;
    assert_eq!(db.execute(&by_day, [10]).await.unwrap_err().code, ErrorCode::TypeMismatch);
    assert_eq!(db.execute(&by_day, ["ten"]).await?.len(), 1_000);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn explaining_runs_only_queries_and_changes_nothing() -> DbResult<()> {
    // R-O7
    let server = TestServer::start().await;
    server.client("test").await;
    let db = server.client_with("test", |config| config.pool_size = 1).await;
    notes(&db).await?;
    // A prepared query, with its values; the first time and after.
    let by_owner = Prepared::new(Query::table("notes").index("notes_owner_day", KeyRange::eq([param(0)])).filter(col("day").lt(param(1))));
    for _ in 0..2 {
        assert_eq!(read(db.explain_prepared(&by_owner, [3, 10]).await?), (50, 50, 10));
    }
    assert_eq!(db.execute(&by_owner, [3, 10]).await?.len(), 10, "and it runs as usual");

    // Inside a transaction it reads what the transaction sees, also as
    // the transaction's first statement.
    db.transaction(|tx| async move {
        assert_eq!(read(tx.explain(Query::table("notes").key([1])).await?), (0, 1, 1));
        tx.delete(Delete::from("notes").index("notes_owner_day", KeyRange::eq([3]))).await?;
        assert_eq!(read(tx.explain(Query::table("notes").index("notes_owner_day", KeyRange::eq([3]))).await?), (0, 0, 0));
        Err::<(), _>(DbError::invalid("only looking"))
    })
    .await
    .unwrap_err();
    let by_owner_3 = by_owner.clone();
    db.read(|tx| async move {
        assert_eq!(read(tx.explain_prepared(&by_owner_3, [3, 50]).await?), (50, 50, 50));
        Ok::<_, DbError>(())
    })
    .await?;

    // A write is not run to be explained: explaining it would do it.
    for write in [
        Request::Delete(Delete::from("notes").key([1])),
        Request::Update(Update::table("notes").key([1]).set("day", 0)),
        Request::Insert(Insert::into("owners").set("id", 99).set("name", "x")),
        Request::Describe,
        Request::Commit,
    ] {
        let e = db.call(Request::Explain(Box::new(write.clone()))).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest, "{write:?}: {e}");
    }
    assert_eq!(db.query_one(Query::table("notes").select("n", count())).await?.get::<i64>("n")?, 1_000);
    assert!(db.get("owners", [99]).await?.is_none());
    // An error of the query is the error of explaining it.
    let e = db.explain(Query::table("nowhere")).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_run_again_does_not_count_twice() -> DbResult<()> {
    // R-O7
    let (server, db) = fresh().await;
    notes(&db).await?;
    let other = server.client("test").await;
    // An optimistic transaction reads 250 rows in five queries. Another
    // writer commits beside it. Its next statement is too large to run
    // beside the others, so the transaction becomes the writer first:
    // its five queries are run again on the newest version, to see that
    // they answer as they did.
    let tx = db.begin_optimistic().await?;
    for _ in 0..5 {
        assert_eq!(tx.query(Query::table("notes").index("notes_owner_day", KeyRange::eq([3]))).await?.len(), 50);
    }
    other.insert(Insert::into("owners").set("id", 50).set("name", "a newcomer")).await?;
    let large = Query::table("notes").key([7]).filter(col("body").ne("x".repeat(70_000)));
    // What that second run read is not this statement's to report.
    assert_eq!(read(tx.explain(large).await?), (0, 1, 1));
    assert_eq!(read(tx.explain(Query::table("notes").key([8])).await?), (0, 1, 1));
    tx.commit().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_statement_is_written_to_the_log_with_what_it_read() -> DbResult<()> {
    // R-O7
    // The server as its own process, told that everything is slow, with
    // its log read back.
    let dir = TempDir::new();
    let (mut server, lines, db) = logged(&dir, Some("0")).await;
    notes(&db).await?;
    db.query(Query::table("notes").filter(col("body").eq("secret-filter-value"))).await?;
    db.query(Query::table("notes").filter(col("owner").eq(6)).limit(7)).await?;
    db.query(Query::table("notes").whole().filter(col("owner").eq(6)).limit(9)).await?;
    db.update(Update::table("notes").filter(col("owner").eq(7).and(col("day").lt(2))).set("body", "secret-other-value")).await?;
    db.query(Query::table("notes").alias("n").index("notes_owner_day", KeyRange::eq([3])).join(Query::table("owners").alias("o").key([col("n.owner")])).order_by(col("n.day")).limit(5)).await?;
    db.update(Update::table("notes").index("notes_owner_day", KeyRange::eq([4])).set("body", "secret-new-value")).await?;
    db.transaction(|tx| async move {
        tx.delete(Delete::from("notes").key([1])).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    let tx = db.begin().await?;
    tx.query(Query::table("owners").key([1])).await?;
    tx.rollback().await?;
    db.batch(Batch::new().add(Query::table("notes").key([2])).add(Query::table("owners"))).await?;
    let by_body = Prepared::new(Query::table("notes").index("notes_owner_day", KeyRange::eq([param(0)])).filter(col("body").eq(param(1))));
    db.execute(&by_body, [Value::from(5), Value::from("secret-parameter-value")]).await?;
    // A statement can name what does not exist, and the name is then
    // whatever the client sent: here, one that tries to end the line and
    // begin another.
    let forged = "notes\ndatabase: slow: 9 ms, database \"forged\"";
    assert!(db.query(Query::table(forged)).await.is_err());
    assert!(db.query(Query::table("notes").index(forged, KeyRange::eq([1]))).await.is_err());
    assert!(db.insert(Insert::into(forged).columns(["id"]).values([val(1)])).await.is_err());
    drop(db);
    // Asked to stop, the server ends its log.
    server.stop();
    let lines = lines.join().unwrap();
    let slow: Vec<&String> = lines.iter().filter(|line| line.starts_with("database: slow: ")).collect();
    let find = |what: &str| slow.iter().find(|line| line.contains(what)).unwrap_or_else(|| panic!("no line about {what:?} in {slow:#?}"));

    // Who, what, and what it read.
    // A query that names no way in is described so, and what the
    // server chose for it ends the line.
    let scan = find("query \"notes\" by the server's choice, filtered:");
    assert!(scan.contains("database \"test\", client \"the test\""), "{scan}");
    assert!(scan.ends_with("read 0 index entries and 1000 rows; chose \"notes\" by the whole table"), "{scan}");
    let chosen = find("query \"notes\" by the server's choice, filtered, limit 7");
    assert!(chosen.ends_with("read 50 index entries and 7 rows; chose \"notes\" by index \"notes_owner_day\""), "{chosen}");
    let asked = find("query \"notes\" by the whole table, filtered, limit 9");
    assert!(asked.ends_with("read 0 index entries and 167 rows"), "{asked}");
    let untuned = find("update \"notes\" by the server's choice, filtered");
    assert!(untuned.contains("read 2 index entries") && untuned.ends_with("; chose \"notes\" by index \"notes_owner_day\""), "{untuned}");
    let joined = find("query \"notes\" by index \"notes_owner_day\", 1 joined, sorted, limit 5");
    assert!(joined.ends_with("read 50 index entries and 100 rows"), "{joined}");
    let update = find("update \"notes\" by index \"notes_owner_day\"");
    assert!(update.contains("read 50 index entries"), "{update}");
    find("delete from \"notes\" by its primary key");
    let batch = find("batch of 2 statements");
    assert!(batch.ends_with("read 0 index entries and 21 rows; chose \"owners\" by the whole table"), "{batch}");
    find("insert into \"notes\", 250 rows");
    let prepared = find("query \"notes\" by index \"notes_owner_day\", filtered");
    assert!(prepared.ends_with("read 50 index entries and 50 rows"), "{prepared}");
    // A transaction's begin and its commit can be what was slow (the wait
    // for the write lock, the wait for the disk), and have lines of
    // their own.
    find(": begin: ");
    find(": commit: ");
    // A name is written quoted and escaped, so it stays inside its line:
    // every line of the log is one the server wrote.
    find(r#"query "notes\ndatabase: slow: 9 ms, database \"forged\"" by the server's choice"#);
    find(r#"by index "notes\ndatabase: slow: 9 ms, database \"forged\"""#);
    find(r#"insert into "notes\ndatabase: slow: 9 ms, database \"forged\"", 1 rows"#);
    assert!(lines.iter().all(|line| !line.contains("database \"forged\"") && !line.starts_with("database: slow: 9 ms")), "{lines:#?}");
    // Never a statement's values: not what it filters by, not what it
    // writes, not what a prepared one is run with.
    for line in &lines {
        assert!(!line.contains("secret") && !line.contains("note 7 is"), "{line}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_logged_unless_asked_for() -> DbResult<()> {
    // R-O7
    // The same statements against a server that was not told what slow
    // is: it calls nothing slow.
    let dir = TempDir::new();
    let (mut server, lines, db) = logged(&dir, None).await;
    notes(&db).await?;
    db.query(Query::table("notes").filter(col("day").eq(10))).await?;
    db.transaction(|tx| async move {
        tx.delete(Delete::from("notes").key([1])).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    assert_eq!(read(db.explain(Query::table("notes").limit(1)).await?), (0, 1, 1));
    drop(db);
    server.stop();
    let lines = lines.join().unwrap();
    assert!(lines.iter().any(|line| line.starts_with("database: serving")), "the log was read: {lines:#?}");
    assert!(lines.iter().all(|line| !line.contains("slow")), "{lines:#?}");
    Ok(())
}
