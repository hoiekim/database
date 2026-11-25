//! Running the server: databases and their passwords, dump and restore,
//! restarts, and the checks an operator leans on. R-O1 to R-O4.

mod common;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use database_protocol::message::{decode_response, encode_request, Request, Response, TxnKind, PROTOCOL_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn pairs(db: &Client) -> DbResult<()> {
    // Two tables that every transaction changes together: `left.n` and
    // `right.n` for the same id always add up to 100.
    for name in ["left", "right"] {
        db.create_table(TableDef::new(name).column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("n", Type::Int)).column(ColumnDef::new("note", Type::Text).default("")).primary_key(["id"])).await?;
    }
    db.transaction(|tx| async move {
        for id in 0..200 {
            tx.insert(Insert::into("left").set("id", id).set("n", 50)).await?;
            tx.insert(Insert::into("right").set("id", id).set("n", 50)).await?;
        }
        Ok::<_, DbError>(())
    })
    .await
}

async fn assert_paired(db: &Client) -> DbResult<()> {
    let tx = db.begin_read().await?;
    let left: Vec<(i64, i64)> = tx.query(Query::table("left").columns(["id", "n"])).await?.map()?;
    let right: Vec<(i64, i64)> = tx.query(Query::table("right").columns(["id", "n"])).await?.map()?;
    assert_eq!(left.len(), 200);
    for ((id, a), (other, b)) in left.iter().zip(&right) {
        assert_eq!((id, a + b), (other, 100), "rows of one transaction are together");
    }
    Ok(())
}

// R-O1: a dump taken while writers run is one moment of the database, and
// restores into an empty server with its password.
#[tokio::test(flavor = "multi_thread")]
async fn a_dump_taken_under_load_restores_whole() -> DbResult<()> {
    let (server, db) = fresh().await;
    pairs(&db).await?;
    db.create_table(TableDef::new("docs").column(ColumnDef::new("id", Type::Int).auto_increment()).column(ColumnDef::new("body", Type::Text)).column(ColumnDef::new("meta", Type::Json).nullable()).primary_key(["id"])).await?;
    let long = "A long body, with ünïcödé, repeated. ".repeat(60_000);
    db.insert(Insert::into("docs").set("body", long.as_str()).set("meta", serde_json::json!({"zz": 1, "a": [1, 2, {"k": null}]}))).await?;
    db.insert(Insert::into("docs").set("body", "short")).await?;
    let other = server.client("second").await;
    other.create_table(TableDef::new("left").column(ColumnDef::new("id", Type::Text)).primary_key(["id"])).await?;
    other.insert(Insert::into("left").set("id", "only in the second database")).await?;

    // Writers move amounts between the two tables while the dump runs.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = tokio::spawn({
        let (db, stop) = (db.clone(), stop.clone());
        async move {
            let mut n = 0i64;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let (id, amount) = (n % 200, n % 9 - 4);
                db.transaction(|tx| async move {
                    tx.update(Update::table("left").key([id]).set("n", col("n") + amount).set("note", "x".repeat((id % 50) as usize))).await?;
                    tx.update(Update::table("right").key([id]).set("n", col("n") - amount)).await?;
                    Ok::<_, DbError>(())
                })
                .await
                .unwrap();
            }
            n
        }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let admin = server.admin().await;
    let mut dump = Vec::new();
    let bytes = admin.admin().dump(&[], &mut dump).await?;
    assert_eq!(bytes as usize, dump.len());
    tokio::time::sleep(Duration::from_millis(30)).await;
    stop.store(true, Ordering::Relaxed);
    assert!(writer.await.unwrap() > 10, "writes went on during the dump");

    // Into a new, empty server.
    let fresh_server = TestServer::start().await;
    let fresh_admin = fresh_server.admin().await;
    fresh_admin.admin().restore(&mut dump.as_slice(), false).await?;
    assert_eq!(fresh_admin.admin().list_databases().await?, ["second", "test"]);
    // The databases' passwords came with it.
    let restored = Client::connect_url(&fresh_server.url("test")).await?;
    assert_paired(&restored).await?;
    let docs = restored.query(Query::table("docs")).await?;
    assert_eq!(docs.len(), 2);
    assert_eq!(docs.first().unwrap().get::<String>("body")?, long);
    assert_eq!(docs.first().unwrap().get::<Json>("meta")?.to_string(), r#"{"a":[1,2,{"k":null}],"zz":1}"#);
    // The numbering goes on where it was.
    let next: i64 = restored.insert(Insert::into("docs").set("body", "new").returning(["id"])).await?.one()?.get("id")?;
    assert_eq!(next, 3);
    let second = Client::connect_url(&fresh_server.url("second")).await?;
    assert_eq!(second.query(Query::table("left")).await?.len(), 1);
    assert!(fresh_admin.admin().check().await?.contains("database test"));
    // And it is all still there after a restart.
    let dir = fresh_server.restart_dir().await;
    let again = TestServer::start_with(dir, |_| {}).await;
    let restored = Client::connect_url(&again.url("test")).await?;
    assert_paired(&restored).await?;
    assert_eq!(restored.query(Query::table("docs")).await?.len(), 3);
    Ok(())
}

// R-O1: restore never half-happens.
#[tokio::test(flavor = "multi_thread")]
async fn a_restore_replaces_only_when_asked_and_only_from_a_whole_dump() -> DbResult<()> {
    let (server, db) = fresh().await;
    pairs(&db).await?;
    let admin = server.admin().await;
    let mut dump = Vec::new();
    admin.admin().dump(&["test"], &mut dump).await?;

    // Changes after the dump.
    db.delete(Delete::from("left").range(KeyRange::new(Bound::included([100]), Bound::Unbounded))).await?;
    assert_eq!(db.query(Query::table("left")).await?.len(), 100);

    // Not over an existing database unless told to.
    let err = admin.admin().restore(&mut dump.as_slice(), false).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AlreadyExists);
    assert_eq!(db.query(Query::table("left")).await?.len(), 100);

    // Not from a dump that is cut short or damaged, even with replace.
    let cut = &dump[..dump.len() - 1];
    assert!(admin.admin().restore(&mut &cut[..], true).await.is_err());
    let mut flipped = dump.clone();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 0x01;
    assert!(admin.admin().restore(&mut flipped.as_slice(), true).await.is_err());
    assert!(admin.admin().restore(&mut &b"not a dump at all"[..], true).await.is_err());
    assert_eq!(db.query(Query::table("left")).await?.len(), 100);
    assert_eq!(admin.admin().list_databases().await?, ["test"], "nothing half-built is left behind");

    // With replace, the dump's state comes back.
    admin.admin().restore(&mut dump.as_slice(), true).await?;
    // A connection opened on the replaced database is told to reconnect.
    let err = db.query(Query::table("left")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    let db = Client::connect_url(&server.url("test")).await?;
    assert_eq!(db.query(Query::table("left")).await?.len(), 200);
    assert_paired(&db).await?;
    Ok(())
}

// R-O2: each database has its own password and its own tables.
#[tokio::test(flavor = "multi_thread")]
async fn databases_are_separate_and_guarded() -> DbResult<()> {
    let server = TestServer::start().await;
    let admin = server.admin().await;
    admin.admin().create_database("shop", "shop-secret").await?;
    admin.admin().create_database("blog", "blog-secret").await?;
    assert_eq!(admin.admin().create_database("shop", "x").await.unwrap_err().code, ErrorCode::AlreadyExists);
    admin.admin().create_database_if_not_exists("shop", "ignored").await?;
    assert!(admin.admin().create_database("Not Valid", "x").await.is_err());
    assert_eq!(admin.admin().list_databases().await?, ["blog", "shop"]);

    let url = |database: &str, password: &str| format!("database://{database}:{password}@127.0.0.1:{}", server.port);
    let shop = Client::connect_url(&url("shop", "shop-secret")).await?;
    let blog = Client::connect_url(&url("blog", "blog-secret")).await?;
    // The same name, two tables.
    shop.create_table(TableDef::new("users").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    blog.create_table(TableDef::new("users").column(ColumnDef::new("name", Type::Text)).primary_key(["name"])).await?;
    shop.insert(Insert::into("users").set("id", 1)).await?;
    assert_eq!(blog.query(Query::table("users")).await?.len(), 0);

    // Wrong password, another database's password, no such database:
    // all the same refusal.
    for (database, password) in [("shop", "wrong"), ("shop", "blog-secret"), ("nope", "shop-secret"), ("", "shop-secret")] {
        let err = Client::connect_url(&url(database, password)).await.err().expect("refused");
        assert_eq!(err.code, ErrorCode::AccessDenied, "{database}");
    }
    // The administrator may enter any database.
    assert_eq!(Client::connect_url(&url("shop", ADMIN_PASSWORD)).await?.query(Query::table("users")).await?.len(), 1);

    // A database's password does not administer the server.
    assert_eq!(shop.admin().create_database("other", "x").await.unwrap_err().code, ErrorCode::AccessDenied);
    assert_eq!(shop.admin().drop_database("blog").await.unwrap_err().code, ErrorCode::AccessDenied);
    assert_eq!(shop.admin().list_databases().await.unwrap_err().code, ErrorCode::AccessDenied);
    let mut sink = Vec::new();
    assert_eq!(shop.admin().dump(&["blog"], &mut sink).await.unwrap_err().code, ErrorCode::AccessDenied);
    assert_eq!(shop.admin().restore(&mut &b""[..], true).await.unwrap_err().code, ErrorCode::AccessDenied);
    // It may look after its own database, and dump it.
    shop.admin().checkpoint().await?;
    assert!(shop.admin().check().await?.contains("table users: 1 rows"));
    assert!(shop.admin().stats().await?.contains("database shop"));
    assert!(shop.admin().dump(&[], &mut sink).await? > 0);

    // A changed password takes effect for new connections.
    admin.admin().set_password("shop", "rotated").await?;
    assert!(Client::connect_url(&url("shop", "shop-secret")).await.is_err());
    assert!(Client::connect_url(&url("shop", "rotated")).await.is_ok());

    // A dropped database is gone, with its password.
    admin.admin().drop_database("blog").await?;
    assert_eq!(blog.query(Query::table("users")).await.unwrap_err().code, ErrorCode::Unavailable);
    assert!(Client::connect_url(&url("blog", "blog-secret")).await.is_err());
    admin.admin().drop_database_if_exists("blog").await?;
    assert_eq!(admin.admin().drop_database("blog").await.unwrap_err().code, ErrorCode::NotFound);
    // Its name can be used again, empty.
    admin.admin().create_database("blog", "new").await?;
    assert!(Client::connect_url(&url("blog", "new")).await?.describe().await?.is_empty());
    Ok(())
}

// R-X10, R-O4: everything is there after a clean stop and start, and the
// files check out.
#[tokio::test(flavor = "multi_thread")]
async fn a_clean_restart_keeps_everything() -> DbResult<()> {
    let (server, db) = fresh().await;
    pairs(&db).await?;
    db.create_index("left", IndexDef::new("left_n").on(["n", "id"])).await?;
    db.add_column("left", ColumnDef::new("added", Type::Timestamp).default_now()).await?;
    let stamp: Timestamp = db.get("left", [5]).await?.unwrap().get("added")?;
    let before = db.describe().await?;
    db.close();

    let dir = server.restart_dir().await;
    // A clean stop leaves nothing in the log.
    let log: u64 = std::fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains(".wal-")).map(|e| e.metadata().unwrap().len()).sum();
    assert!(log < 100, "the log is empty after a clean stop ({log} bytes)");

    let server = TestServer::start_with(dir, |_| {}).await;
    let db = Client::connect_url(&server.url("test")).await?;
    assert_eq!(db.describe().await?, before);
    assert_paired(&db).await?;
    assert_eq!(db.get("left", [5]).await?.unwrap().get::<Timestamp>("added")?, stamp, "a column added later keeps the value its rows were given");
    assert_eq!(db.query(Query::table("left").index("left_n", KeyRange::eq([50]))).await?.len(), 200);
    let report = db.admin().check().await?;
    assert!(report.contains("table left: 200 rows, 1 indexes"), "{report}");
    Ok(())
}

// The memory held for pages is in the stats, where it is kept apart
// (`memory::pages`): never less than the pages the cache holds.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[tokio::test(flavor = "multi_thread")]
async fn the_stats_say_how_much_memory_pages_hold() -> DbResult<()> {
    let (server, db) = fresh().await;
    pairs(&db).await?;
    let stats = server.admin().await.admin().stats().await?;
    let number = |label: &str, nth: usize| -> f64 {
        let line = stats.lines().find(|l| l.starts_with(label)).unwrap_or_else(|| panic!("no line {label:?} in {stats}"));
        line.split(|c: char| !c.is_ascii_digit() && c != '.').filter(|s| !s.is_empty() && *s != ".").nth(nth).unwrap().parse().unwrap()
    };
    let (mapped, free) = (number("memory for pages:", 0), number("memory for pages:", 1));
    let cached = number("cache:", 0) * 8192.0 / 1_048_576.0;
    assert!(mapped > 0.0 && free <= mapped && mapped - free >= cached - 0.1, "{stats}");
    Ok(())
}

// R-O3: a health check and the server's clock.
#[tokio::test(flavor = "multi_thread")]
async fn the_server_answers_a_ping_and_tells_the_time() -> DbResult<()> {
    let (server, db) = fresh().await;
    db.ping().await?;
    server.admin().await.ping().await?;
    let wall = Timestamp::now();
    let told = db.now().await?;
    assert!((told.micros() - wall.micros()).abs() < 5_000_000);
    // A stopped server is an error, promptly.
    let port = server.port;
    server.stop().await;
    let mut config = Config::new("127.0.0.1", port, "test", PASSWORD);
    config.connect_timeout = Duration::from_millis(500);
    assert_eq!(Client::connect(config).await.err().unwrap().code, ErrorCode::Connection);
    assert_eq!(db.ping().await.unwrap_err().code, ErrorCode::Connection);
    Ok(())
}

async fn raw_exchange(port: u16, frames: &[Vec<u8>]) -> Vec<Option<Response>> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut answers = Vec::new();
    for frame in frames {
        stream.write_all(&(frame.len() as u32).to_le_bytes()).await.unwrap();
        stream.write_all(frame).await.unwrap();
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).await.is_err() {
            answers.push(None);
            break;
        }
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        stream.read_exact(&mut body).await.unwrap();
        answers.push(Some(decode_response(&body).unwrap()));
    }
    answers
}

// The server holds up against clients that do not follow the protocol.
#[tokio::test(flavor = "multi_thread")]
async fn a_misbehaving_client_gets_an_error_and_harms_nothing() -> DbResult<()> {
    let (server, db) = fresh().await;
    pairs(&db).await?;
    let hello = |version: u32, password: &str| encode_request(&Request::Hello { version, database: "test".into(), password: password.into(), client: "raw".into() }, 0);

    // Another protocol version.
    let answers = raw_exchange(server.port, &[hello(999, PASSWORD)]).await;
    assert!(matches!(&answers[0], Some(Response::Error(e)) if e.code == ErrorCode::ProtocolMismatch));
    // Not greeting first.
    let answers = raw_exchange(server.port, &[encode_request(&Request::Ping, 0)]).await;
    assert!(matches!(&answers[0], Some(Response::Error(e)) if e.code == ErrorCode::InvalidRequest));
    // Garbage after a good greeting: an error each time, the connection stays.
    let answers = raw_exchange(server.port, &[hello(PROTOCOL_VERSION, PASSWORD), vec![0xFF; 40], vec![], encode_request(&Request::Ping, 0)]).await;
    assert!(matches!(answers[0], Some(Response::Hello { .. })));
    assert!(matches!(answers[1], Some(Response::Error(_))));
    assert!(matches!(answers[2], Some(Response::Error(_))));
    assert!(matches!(answers[3], Some(Response::Ok)));
    // Statements that make no sense.
    assert_eq!(db.query(Query::table("nowhere")).await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(db.query(Query::table("left").index("no_such_index", KeyRange::all())).await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(db.query(Query::table("left").filter(col("nope").eq(1))).await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(db.query(Query::table("left").filter(col("n").eq("text"))).await.unwrap_err().code, ErrorCode::TypeMismatch);
    assert_eq!(db.query(Query::table("left").key([1, 2])).await.unwrap_err().code, ErrorCode::InvalidRequest);
    assert_eq!(db.insert(Insert::into("left").set("id", "one").set("n", 1)).await.unwrap_err().code, ErrorCode::TypeMismatch);
    assert_eq!(db.insert(Insert::into("left").set("id", 999)).await.unwrap_err().code, ErrorCode::NotNullViolation);
    assert_eq!(db.update(Update::table("left").key([1]).set("id", 2)).await.unwrap_err().code, ErrorCode::InvalidRequest);
    assert_eq!(db.query(Query::table("left").select("x", col("n") / 0)).await.unwrap_err().code, ErrorCode::Arithmetic);
    let tx = db.begin().await?;
    assert_eq!(tx.rollback_to("never").await.unwrap_err().code, ErrorCode::NotFound);
    tx.rollback().await?;
    // A connection that vanishes mid-transaction frees the write lock.
    {
        let begin = encode_request(&Request::Begin { kind: TxnKind::Exclusive, lock_wait_ms: 1000 }, 0);
        let answers = raw_exchange(server.port, &[hello(PROTOCOL_VERSION, PASSWORD), begin]).await;
        assert!(matches!(answers[1], Some(Response::Ok)));
    }
    db.update(Update::table("left").key([1]).set("n", 50)).await?;
    assert_paired(&db).await?;
    db.admin().check().await?;
    Ok(())
}

// R-Q17, R-O4: a full-text index large enough to be kept in many parts,
// which are merged as it grows and cleaned as rows are deleted and
// rewritten, answers exactly as reading every row would; and still does
// after a restart.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_search_index_agrees_with_reading_every_row() -> DbResult<()> {
    let (server, db) = fresh().await;
    db.create_table(
        TableDef::new("notes")
            .column(ColumnDef::new("id", Type::Int))
            .column(ColumnDef::new("owner", Type::Int))
            .column(ColumnDef::new("body", Type::Text))
            .primary_key(["id"])
            .index(IndexDef::new("notes_search").on(["owner"]).full_text(TextConfig::Simple, [(col("body"), 1.0)])),
    )
    .await?;
    // A small generator, so the test needs nothing but itself.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |below: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % below
    };
    let body = |next: &mut dyn FnMut(u64) -> u64| (0..180).map(|_| format!("w{}", next(900))).collect::<Vec<_>>().join(" ");
    let mut model: std::collections::BTreeMap<i64, (i64, String)> = std::collections::BTreeMap::new();
    for chunk in 0..25i64 {
        let mut insert = Insert::into("notes").columns(["id", "owner", "body"]);
        for n in 0..100 {
            let (id, owner, text) = (chunk * 100 + n, n % 3, body(&mut next));
            insert = insert.values([val(id), val(owner), val(text.as_str())]);
            model.insert(id, (owner, text));
        }
        db.insert(insert).await?;
    }

    async fn agrees(db: &Client, model: &std::collections::BTreeMap<i64, (i64, String)>) -> DbResult<()> {
        for (owner, query, prefix) in [(0, "w7", false), (1, "w7 w13", false), (2, "w899", false), (0, "w12", true), (1, "w1 w88", true), (2, "nothing", false)] {
            let wanted: Vec<&str> = query.split(' ').collect();
            let expected: Vec<i64> = model
                .iter()
                .filter(|(_, (o, text))| *o == owner && wanted.iter().all(|w| text.split(' ').any(|t| if prefix { t.starts_with(w) } else { t == *w })))
                .map(|(id, _)| *id)
                .collect();
            let search = Query::table("notes").search("notes_search", [owner], query);
            let search = if prefix { search.match_prefix() } else { search };
            let mut found: Vec<i64> = column(&db.query(search.columns(["id"])).await?, "id");
            found.sort();
            assert_eq!(found, expected, "owner {owner}, {query:?}, prefix {prefix}");
        }
        Ok(())
    }
    agrees(&db, &model).await?;

    // A third of the rows go, and a third are rewritten.
    db.delete(Delete::from("notes").filter((col("id") % 3).eq(0))).await?;
    model.retain(|id, _| id % 3 != 0);
    for (id, (_, text)) in model.iter_mut().filter(|(id, _)| *id % 3 == 1) {
        *text = body(&mut next);
        db.update(Update::table("notes").key([*id]).set("body", text.as_str())).await?;
    }
    agrees(&db, &model).await?;
    // A row written again with the words it had stays as it is.
    let (id, (_, text)) = model.iter().next().unwrap();
    db.update(Update::table("notes").key([*id]).set("body", text.as_str())).await?;
    agrees(&db, &model).await?;
    let report = db.admin().check().await?;
    assert!(report.contains(&format!("table notes: {} rows, 1 indexes", model.len())), "{report}");
    db.close();

    let server = TestServer::start_with(server.restart_dir().await, |_| {}).await;
    let db = Client::connect_url(&server.url("test")).await?;
    agrees(&db, &model).await?;
    // Emptied, the index gives its room back.
    db.delete(Delete::from("notes")).await?;
    model.clear();
    agrees(&db, &model).await?;
    db.admin().check().await?;
    Ok(())
}
