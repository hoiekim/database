//! Durability against a server that dies without warning: R-X10.
//!
//! These tests run the real `database` binary as its own process, load it
//! from several connections, and kill it with SIGKILL at an arbitrary
//! moment. Whatever a client was told is committed must be there when the
//! server comes back, whole; whatever was not must be absent, whole.
//!
//! A killed process loses nothing the operating system already has, so
//! this exercises recovery from the log and the commit protocol, not the
//! disk's own honesty: the engine's tests cover that with a simulated
//! power cut (`engine/tests/recovery.rs`).

mod common;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;

struct Process {
    child: Child,
    port: u16,
}

impl Process {
    /// Starts the server binary on `dir` and waits until it answers.
    async fn start(dir: &TempDir, cache_mb: u32) -> Process {
        // A port nobody holds: ask the system for one, then give it up.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let child = Command::new(env!("CARGO_BIN_EXE_database"))
            .arg("serve")
            .env("DATABASE_DATA", dir.path())
            .env("DATABASE_LISTEN", format!("127.0.0.1:{port}"))
            .env("DATABASE_ADMIN_PASSWORD", ADMIN_PASSWORD)
            .env("DATABASE_CACHE_MB", cache_mb.to_string())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server binary starts");
        let process = Process { child, port };
        for _ in 0..200 {
            if process.connect("").await.is_ok() {
                return process;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the server did not come up");
    }

    async fn connect(&self, database: &str) -> DbResult<Client> {
        let password = if database.is_empty() { ADMIN_PASSWORD } else { PASSWORD };
        let mut config = Config::new("127.0.0.1", self.port, database, password);
        config.connect_timeout = Duration::from_secs(2);
        Client::connect(config).await
    }

    fn kill(mut self) {
        self.child.kill().expect("the server can be killed");
        self.child.wait().unwrap();
    }

    /// The server's resident memory, in bytes.
    #[cfg(target_os = "linux")]
    fn resident(&self) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.child.id())).unwrap();
        let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
        line.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap() * 1024
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn schema(db: &Client) -> DbResult<()> {
    db.create_table_if_not_exists(
        TableDef::new("entries")
            .column(ColumnDef::new("id", Type::Int))
            .column(ColumnDef::new("batch", Type::Int))
            .column(ColumnDef::new("payload", Type::Text))
            .primary_key(["id"])
            .index(IndexDef::new("entries_batch").on(["batch", "id"])),
    )
    .await?;
    db.create_table_if_not_exists(TableDef::new("batches").column(ColumnDef::new("batch", Type::Int)).column(ColumnDef::new("rows", Type::Int)).primary_key(["batch"])).await
}

/// Writes batches until the server goes away: each is one transaction of
/// a row in `batches` and several in `entries`. Returns the batches whose
/// commit was acknowledged.
async fn writer(db: Client, next: Arc<AtomicI64>, stop: Arc<AtomicBool>, acknowledged: Arc<Mutex<Vec<(i64, i64)>>>) {
    while !stop.load(Ordering::Relaxed) {
        let batch = next.fetch_add(1, Ordering::Relaxed);
        let rows = batch % 7 + 1;
        let result: Result<(), DbError> = db
            .transaction(|tx| async move {
                for n in 0..rows {
                    // Some payloads large enough to live outside their row.
                    let payload = "x".repeat(if n == 0 && batch % 5 == 0 { 20_000 } else { 50 });
                    tx.insert(Insert::into("entries").set("id", batch * 100 + n).set("batch", batch).set("payload", payload)).await?;
                }
                tx.insert(Insert::into("batches").set("batch", batch).set("rows", rows)).await?;
                Ok(())
            })
            .await;
        match result {
            Ok(()) => acknowledged.lock().unwrap().push((batch, rows)),
            Err(_) => return,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_acknowledged_is_lost_when_the_server_is_killed() -> DbResult<()> {
    let dir = TempDir::new();
    let next = Arc::new(AtomicI64::new(1));
    let acknowledged: Arc<Mutex<Vec<(i64, i64)>>> = Arc::default();

    for round in 0..6u64 {
        let server = Process::start(&dir, 8).await;
        if round == 0 {
            server.connect("").await?.admin().create_database("test", PASSWORD).await?;
        }
        let db = server.connect("test").await?;
        schema(&db).await?;

        // What earlier rounds committed is all here, and whole.
        let committed = acknowledged.lock().unwrap().clone();
        let batches = db.query(Query::table("batches")).await?;
        let stored: std::collections::HashMap<i64, i64> = batches.iter().map(|r| (r.get("batch").unwrap(), r.get("rows").unwrap())).collect();
        for (batch, rows) in &committed {
            assert_eq!(stored.get(batch), Some(rows), "round {round}: batch {batch} was acknowledged and is gone");
        }
        // Every batch that is here is here entirely: its marker row and
        // exactly its entries. One the kill interrupted is not here at all.
        let counts = db
            .query(Query::table("entries").index("entries_batch", KeyRange::all()).group_by([col("batch")]).select("batch", col("batch")).select("n", count()))
            .await?;
        assert_eq!(counts.len(), stored.len(), "round {round}: entries without their batch, or a batch without entries");
        for row in &counts {
            assert_eq!(stored.get(&row.get::<i64>("batch")?), Some(&row.get::<i64>("n")?), "round {round}: a batch is partly there");
        }
        db.admin().check().await?;

        // Load it from several connections, then kill it mid-flight.
        let stop = Arc::new(AtomicBool::new(false));
        let mut writers = Vec::new();
        for _ in 0..4 {
            writers.push(tokio::spawn(writer(server.connect("test").await?, next.clone(), stop.clone(), acknowledged.clone())));
        }
        tokio::time::sleep(Duration::from_millis(150 + round * 90)).await;
        if round % 3 == 2 {
            // Sometimes mid-checkpoint.
            let admin = server.connect("test").await?;
            tokio::spawn(async move {
                let _ = admin.admin().checkpoint().await;
            });
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        server.kill();
        stop.store(true, Ordering::Relaxed);
        for writer in writers {
            writer.await.unwrap();
        }
    }
    assert!(acknowledged.lock().unwrap().len() > 100, "the rounds committed work");

    // A last start, to check the final round's commits.
    let server = Process::start(&dir, 8).await;
    let db = server.connect("test").await?;
    let stored: std::collections::HashSet<i64> = db.query(Query::table("batches").columns(["batch"])).await?.column::<i64>()?.into_iter().collect();
    for (batch, _) in acknowledged.lock().unwrap().iter() {
        assert!(stored.contains(batch), "batch {batch} was acknowledged and is gone");
    }
    db.admin().check().await?;
    Ok(())
}

// R-O5: the server's memory stays near its cache budget however much
// data passes through it.
// It reads the server's resident size from /proc, which only Linux has.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn memory_stays_bounded_while_data_grows() -> DbResult<()> {
    let dir = TempDir::new();
    let server = Process::start(&dir, 8).await;
    server.connect("").await?.admin().create_database("test", PASSWORD).await?;
    let db = server.connect("test").await?;
    schema(&db).await?;
    let idle = server.resident();

    // About 150 MB of rows, in transactions of 200.
    let payload = "payload ".repeat(180);
    for chunk in 0..500i64 {
        let mut insert = Insert::into("entries").columns(["id", "batch", "payload"]);
        for n in 0..200 {
            insert = insert.values([val(chunk * 200 + n), val(chunk), val(payload.as_str())]);
        }
        db.insert(insert).await?;
    }
    db.admin().checkpoint().await?;
    // Read it all back, in index order and by key.
    let total: i64 = db.query_one(Query::table("entries").select("n", count())).await?.get("n")?;
    assert_eq!(total, 100_000);
    for chunk in (0..500i64).step_by(7) {
        let rows = db.query(Query::table("entries").index("entries_batch", KeyRange::eq([chunk]))).await?;
        assert_eq!(rows.len(), 200);
    }
    let after = server.resident();
    let file = std::fs::metadata(dir.path().join("test.db")).unwrap().len();
    assert!(file > 120 * 1024 * 1024, "the data file holds the rows ({file} bytes)");
    // An 8 MB cache, and room for the process itself, its threads and
    // the request in hand: far less than the data.
    assert!(after < 64 * 1024 * 1024, "resident memory is {after} bytes after loading {file} bytes of data (idle: {idle})");
    Ok(())
}

// R-S10: a volatile table is empty after a restart; everything else is not.
#[tokio::test(flavor = "multi_thread")]
async fn a_volatile_table_starts_empty_and_costs_no_log() -> DbResult<()> {
    let dir = TempDir::new();
    let server = Process::start(&dir, 8).await;
    server.connect("").await?.admin().create_database("test", PASSWORD).await?;
    let db = server.connect("test").await?;
    db.create_table(TableDef::new("throttle").column(ColumnDef::new("key", Type::Text)).column(ColumnDef::new("hits", Type::Int)).primary_key(["key"]).volatile()).await?;
    db.create_table(TableDef::new("kept").column(ColumnDef::new("key", Type::Text)).primary_key(["key"])).await?;
    db.insert(Insert::into("kept").set("key", "a")).await?;
    let log = || std::fs::metadata(dir.path().join("test.wal-0000000001")).map(|m| m.len()).unwrap_or(0);
    let before = log();
    for n in 0..500 {
        db.insert(Insert::into("throttle").set("key", format!("k{}", n % 50)).set("hits", 1).on_conflict_update(PRIMARY).update("hits", col("hits") + 1).returning(["hits"])).await?;
    }
    assert_eq!(log(), before, "writes to a volatile table are not logged");
    assert_eq!(db.query(Query::table("throttle")).await?.len(), 50);
    assert_eq!(db.get("throttle", ["k7"]).await?.unwrap().get::<i64>("hits")?, 10);

    server.kill();
    let server = Process::start(&dir, 8).await;
    let db = server.connect("test").await?;
    assert_eq!(db.query(Query::table("throttle")).await?.len(), 0);
    assert_eq!(db.query(Query::table("kept")).await?.len(), 1);
    db.insert(Insert::into("throttle").set("key", "k1").set("hits", 1)).await?;
    db.admin().check().await?;
    Ok(())
}
