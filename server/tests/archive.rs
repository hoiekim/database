//! The archive of the log, with real server processes: a server that
//! keeps its log in an archive is loaded from several connections and
//! killed with SIGKILL, several times over, and is then restored from
//! the archive to moments in the middle of the load. The same faults on
//! a simulated disk, exhaustively, are `engine/tests/archive.rs`.

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use database_protocol::time::{format_timestamp, now_micros};

struct Process {
    child: Child,
    port: u16,
}

impl Process {
    /// Starts the server binary on `dir`, keeping its log in `archive`,
    /// and waits until it answers.
    async fn start(dir: &Path, archive: &Path) -> Process {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let child = Command::new(env!("CARGO_BIN_EXE_database"))
            .arg("serve")
            .env("DATABASE_DATA", dir)
            .env("DATABASE_LISTEN", format!("127.0.0.1:{port}"))
            .env("DATABASE_ADMIN_PASSWORD", ADMIN_PASSWORD)
            .env("DATABASE_ARCHIVE", archive)
            .env("DATABASE_ARCHIVE_SECONDS", "1")
            .stderr(Stdio::null())
            .spawn()
            .expect("the server binary starts");
        let process = Process { child, port };
        for _ in 0..400 {
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

    /// Runs the `database` command against this server.
    fn command(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_database"))
            .args(args)
            .env("DATABASE_LISTEN", format!("127.0.0.1:{}", self.port))
            .env("DATABASE_ADMIN_PASSWORD", ADMIN_PASSWORD)
            .env("DATABASE_PASSWORD", PASSWORD)
            .env_remove("DATABASE_SOCKET")
            .output()
            .expect("the command runs");
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }

    fn kill(mut self) {
        self.child.kill().expect("the server can be killed");
        self.child.wait().unwrap();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A batch a writer made, and when it heard that it was committed.
#[derive(Clone, Copy, Debug)]
struct Batch {
    id: i64,
    rows: i64,
    acknowledged: i64,
}

async fn schema(db: &Client) -> DbResult<()> {
    db.create_table_if_not_exists(
        TableDef::new("entries").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("batch", Type::Int)).column(ColumnDef::new("payload", Type::Text)).primary_key(["id"]).index(IndexDef::new("entries_batch").on(["batch", "id"])),
    )
    .await?;
    db.create_table_if_not_exists(TableDef::new("batches").column(ColumnDef::new("batch", Type::Int)).column(ColumnDef::new("rows", Type::Int)).primary_key(["batch"])).await
}

/// Writes batches, each one transaction, until told to stop or the
/// server goes away. `begun` holds when each batch was first sent, and
/// `acknowledged` the batches whose commit was answered.
async fn writer(db: Client, next: Arc<AtomicI64>, stop: Arc<AtomicBool>, begun: Arc<Mutex<Vec<(i64, i64)>>>, acknowledged: Arc<Mutex<Vec<Batch>>>) {
    while !stop.load(Ordering::Relaxed) {
        let id = next.fetch_add(1, Ordering::Relaxed);
        let rows = id % 5 + 1;
        let sent = now_micros();
        begun.lock().unwrap().push((id, sent));
        let result: Result<(), DbError> = db
            .transaction(|tx| async move {
                for n in 0..rows {
                    let payload = "x".repeat(if n == 0 && id % 7 == 0 { 20_000 } else { 40 });
                    tx.insert(Insert::into("entries").set("id", id * 100 + n).set("batch", id).set("payload", payload)).await?;
                }
                tx.insert(Insert::into("batches").set("batch", id).set("rows", rows)).await?;
                Ok(())
            })
            .await;
        match result {
            Ok(()) => acknowledged.lock().unwrap().push(Batch { id, rows, acknowledged: now_micros() }),
            Err(_) => return,
        }
    }
}

/// The batches a database holds, each checked to be there whole.
async fn batches(db: &Client) -> DbResult<std::collections::HashMap<i64, i64>> {
    let stored: std::collections::HashMap<i64, i64> = db.query(Query::table("batches")).await?.iter().map(|r| (r.get("batch").unwrap(), r.get("rows").unwrap())).collect();
    let counts = db.query(Query::table("entries").index("entries_batch", KeyRange::all()).group_by([col("batch")]).select("batch", col("batch")).select("n", count())).await?;
    assert_eq!(counts.len(), stored.len(), "entries without their batch, or a batch without entries");
    for row in &counts {
        assert_eq!(stored.get(&row.get::<i64>("batch")?), Some(&row.get::<i64>("n")?), "a batch is partly there");
    }
    Ok(stored)
}

// R-O8, R-O9
#[tokio::test(flavor = "multi_thread")]
async fn a_server_killed_under_load_is_restored_from_its_archive_to_moments_of_the_load() -> DbResult<()> {
    let (dir, archive) = (TempDir::new(), TempDir::new());
    let next = Arc::new(AtomicI64::new(1));
    let begun: Arc<Mutex<Vec<(i64, i64)>>> = Arc::default();
    let acknowledged: Arc<Mutex<Vec<Batch>>> = Arc::default();
    // Moments in the middle of the load, by this machine's clock, which
    // is the server's.
    let mut moments = Vec::new();

    for round in 0..4u64 {
        let server = Process::start(dir.path(), archive.path()).await;
        if round == 0 {
            server.connect("").await?.admin().create_database("test", PASSWORD).await?;
        }
        let db = server.connect("test").await?;
        schema(&db).await?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut writers = Vec::new();
        for _ in 0..4 {
            writers.push(tokio::spawn(writer(server.connect("test").await?, next.clone(), stop.clone(), begun.clone(), acknowledged.clone())));
        }
        let before = acknowledged.lock().unwrap().len();
        for wanted in [15, 30] {
            let waiting = std::time::Instant::now();
            while acknowledged.lock().unwrap().len() < before + wanted {
                assert!(waiting.elapsed() < Duration::from_secs(60), "round {round}: the load does not commit");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            moments.push(now_micros());
        }
        if round == 1 {
            // A base in the middle of the load, and the kill soon after.
            let admin = server.connect("").await?;
            admin.admin().base(&["test"]).await?;
        }
        if round == 2 {
            // Past a second of commits, so that the server makes a
            // checkpoint of its own to copy the log, and is killed in
            // or around it.
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        server.kill();
        stop.store(true, Ordering::Relaxed);
        for writer in writers {
            writer.await.unwrap();
        }
    }

    let server = Process::start(dir.path(), archive.path()).await;
    let admin = server.connect("").await?;
    // The log the kills left in the data directory goes to the archive
    // with a checkpoint; then the archive is whole.
    admin.admin().checkpoint().await?;
    let (whole, report) = server.command(&["archive", "test"]);
    assert!(whole && report.contains("whole"), "{report}");
    let stats = admin.admin().stats().await?;
    assert!(stats.contains("archive of test") && stats.contains("whole up to log file"), "{stats}");

    let (begun, acknowledged) = (begun.lock().unwrap().clone(), acknowledged.lock().unwrap().clone());
    assert!(acknowledged.len() >= 120, "the rounds committed work");
    let live = batches(&server.connect("test").await?).await?;
    for batch in &acknowledged {
        assert_eq!(live.get(&batch.id), Some(&batch.rows), "batch {} was acknowledged and is gone", batch.id);
    }
    // The end of the archive is the database as it is.
    let report = admin.admin().recover("test", Moment::End, "then", false, PASSWORD).await?;
    assert!(report.contains("restored from test."), "{report}");
    assert_eq!(batches(&server.connect("then").await?).await?, live);

    for (i, moment) in moments.iter().enumerate() {
        // Through the command, as an operator would; every other time
        // through the client.
        if i % 2 == 0 {
            let (restored, said) = server.command(&["recover", "test", "--to", &format_timestamp(*moment), "--as", "then", "--replace"]);
            assert!(restored, "{said}");
        } else {
            admin.admin().recover("test", Moment::Time(*moment), "then", true, "").await?;
        }
        let then = server.connect("then").await?;
        let held = batches(&then).await?;
        // Every batch acknowledged by the moment is there, and none that
        // was first sent after it.
        for batch in &acknowledged {
            if batch.acknowledged <= *moment {
                assert_eq!(held.get(&batch.id), Some(&batch.rows), "moment {i}: batch {} was acknowledged before it and is not there", batch.id);
            }
        }
        for (id, sent) in &begun {
            if sent > moment {
                assert!(!held.contains_key(id), "moment {i}: batch {id} was sent after it and is there");
            }
        }
        // A moment of the load: something was committed before it, and,
        // unless the last kill followed it at once, something after.
        assert!(!held.is_empty() && (held.len() < live.len() || i + 1 == moments.len()), "moment {i} is a moment of the load");
        then.admin().check().await?;
    }
    // The database restored from is as it was throughout.
    assert_eq!(batches(&server.connect("test").await?).await?, live);
    Ok(())
}

// R-O10
#[tokio::test(flavor = "multi_thread")]
async fn commits_go_on_while_the_archive_is_away_and_reach_it_when_it_is_back() -> DbResult<()> {
    let (dir, place) = (TempDir::new(), TempDir::new());
    let (archive, away) = (place.path().join("archive"), place.path().join("away"));
    let server = Process::start(dir.path(), &archive).await;
    let admin = server.connect("").await?;
    admin.admin().create_database("test", PASSWORD).await?;
    let db = server.connect("test").await?;
    schema(&db).await?;
    let insert = |id: i64| Insert::into("batches").set("batch", id).set("rows", 0);
    for id in 0..20 {
        db.insert(insert(id)).await?;
    }
    admin.admin().checkpoint().await?;
    // The archive goes: what was a directory is a file nothing can be
    // written under.
    std::fs::rename(&archive, &away).unwrap();
    std::fs::write(&archive, b"not a directory").unwrap();
    for id in 20..60 {
        db.insert(insert(id)).await?;
        if id % 10 == 9 {
            admin.admin().checkpoint().await?;
        }
    }
    let stats = admin.admin().stats().await?;
    assert!(stats.contains("refused") && stats.contains("log files kept"), "{stats}");
    let logs = || std::fs::read_dir(dir.path()).unwrap().filter(|entry| entry.as_ref().unwrap().file_name().to_string_lossy().contains("test.wal-")).count();
    assert!(logs() >= 4, "the log files the checkpoints passed are kept: {}", logs());
    // Nothing can be restored meanwhile, and nothing is dropped.
    assert!(admin.admin().archive("test", false).await.is_err());
    assert!(admin.admin().drop_database("test").await.unwrap_err().message.contains("the only copy of its last commits"));
    // Killed and started again while the archive is still away, the
    // server has every commit and still keeps the files.
    drop(db);
    drop(admin);
    server.kill();
    let server = Process::start(dir.path(), &archive).await;
    let (admin, db) = (server.connect("").await?, server.connect("test").await?);
    assert_eq!(db.query(Query::table("batches")).await?.len(), 60);
    assert!(logs() >= 4, "{}", logs());
    // It comes back, and the next checkpoint hands it all that was kept.
    std::fs::remove_file(&archive).unwrap();
    std::fs::rename(&away, &archive).unwrap();
    for id in 60..70 {
        db.insert(insert(id)).await?;
    }
    admin.admin().checkpoint().await?;
    assert_eq!(logs(), 1);
    let report = admin.admin().archive("test", true).await?;
    assert!(report.contains("whole"), "{report}");
    admin.admin().recover("test", Moment::End, "then", false, PASSWORD).await?;
    assert_eq!(server.connect("then").await?.query(Query::table("batches")).await?.len(), 70);
    Ok(())
}

// R-O9
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_database_is_restored_from_the_archive_and_a_restore_is_refused_where_it_cannot_be_right() -> DbResult<()> {
    let archive = TempDir::new();
    let path = archive.path().to_path_buf();
    let server = TestServer::start_with(TempDir::new(), |config| {
        config.archive = Some(path);
        config.sync = true;
    })
    .await;
    let admin = server.admin().await;
    let db = server.client("shop").await;
    db.create_table(TableDef::new("orders").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("item", Type::Text)).primary_key(["id"])).await?;
    for id in 0..10 {
        db.insert(Insert::into("orders").set("id", id).set("item", "tea")).await?;
    }
    admin.admin().checkpoint().await?;
    // Each commit is listed with its position: the moment before the
    // rows are deleted is the commit before the delete.
    db.delete(Delete::from("orders").filter(col("id").ge(5))).await?;
    admin.admin().checkpoint().await?;
    let listed = admin.admin().archive("shop", true).await?;
    let positions: Vec<&str> = listed.lines().filter(|line| line.contains(" bytes") && !line.contains("base")).map(|line| line.split_whitespace().next().unwrap()).collect();
    assert_eq!(positions.len(), 12, "the table, ten rows and the delete: {listed}");
    let (generation, offset) = positions[10].split_once(':').unwrap();
    let before = Moment::Position { generation: generation.parse().unwrap(), offset: offset.parse().unwrap() };

    // Only the administrator restores; not over a database unasked; not
    // to a name without a password; not from a history that is not there.
    assert_eq!(db.admin().recover("shop", before, "shop", true, "").await.unwrap_err().code, ErrorCode::AccessDenied);
    assert_eq!(admin.admin().recover("shop", before, "shop", false, "").await.unwrap_err().code, ErrorCode::AlreadyExists);
    assert!(admin.admin().recover("shop", before, "copy", false, "").await.unwrap_err().message.contains("no password yet"));
    assert_eq!(admin.admin().recover("none", before, "copy", false, PASSWORD).await.unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(admin.admin().list_databases().await?, ["shop"]);

    // Restored over itself: connections to the old one are told, and a
    // new one, with the password the database had, finds the ten rows.
    admin.admin().recover("shop", before, "shop", true, "").await?;
    assert_eq!(db.query(Query::table("orders")).await.unwrap_err().code, ErrorCode::Unavailable);
    let db = Client::connect_url(&server.url("shop")).await?;
    assert_eq!(db.query(Query::table("orders")).await?.len(), 10);
    db.insert(Insert::into("orders").set("id", 10).set("item", "milk")).await?;

    // Dropped, it is in the archive to its last commit, under the
    // history it had; the name alone is two histories by now.
    let history = admin.admin().stats().await?.lines().find_map(|line| line.split("history ").nth(1).map(|rest| rest.split(',').next().unwrap().to_string())).expect("stats name the history");
    admin.admin().drop_database("shop").await?;
    assert!(admin.admin().recover("shop", Moment::End, "shop", false, PASSWORD).await.unwrap_err().message.contains("histories of"));
    admin.admin().recover(&history, Moment::End, "shop", false, PASSWORD).await?;
    let db = Client::connect_url(&server.url("shop")).await?;
    assert_eq!(db.query(Query::table("orders")).await?.len(), 11);
    db.admin().check().await?;

    // A server without an archive says so.
    let plain = TestServer::start().await;
    let refused = plain.admin().await.admin().base(&[]).await.unwrap_err();
    assert!(refused.message.contains("DATABASE_ARCHIVE"), "{}", refused.message);
    Ok(())
}
