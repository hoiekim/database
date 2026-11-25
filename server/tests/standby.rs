//! A standby and its primary as two server processes, with the
//! connection between them cut on purpose and each of them killed.
//!
//! The standby reaches its primary through a relay in this test, which
//! can drop every connection that passes through it and refuse new
//! ones. What a standby must then be is checked from outside, as a
//! client sees it: a copy that holds whole commits only, never fewer
//! than it held before, and all of them once the two can talk again.
//! The same on a simulated disk, with a kill before every operation, is
//! `engine/tests/standby.rs`.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use tokio::net::{TcpListener, TcpStream};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Process {
    child: Child,
    port: u16,
}

impl Process {
    /// Starts the server binary on `dir` and `port` with these settings
    /// besides, and waits until it answers.
    async fn start(dir: &Path, port: u16, settings: &[(&str, String)]) -> Process {
        let mut command = Command::new(env!("CARGO_BIN_EXE_database"));
        command.arg("serve").env("DATABASE_DATA", dir).env("DATABASE_LISTEN", format!("127.0.0.1:{port}")).env("DATABASE_ADMIN_PASSWORD", ADMIN_PASSWORD).stderr(Stdio::null());
        for name in ["DATABASE_PRIMARY", "DATABASE_ARCHIVE", "DATABASE_KEEP_LOG_MB", "DATABASE_SOCKET"] {
            command.env_remove(name);
        }
        for (name, value) in settings {
            command.env(name, value);
        }
        let process = Process { child: command.spawn().expect("the server binary starts"), port };
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
        config.pool_size = 2;
        Client::connect(config).await
    }

    async fn stats(&self) -> String {
        self.connect("").await.unwrap().admin().stats().await.unwrap()
    }

    /// Runs the `database` command against this server.
    fn command(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_database")).args(args).env("DATABASE_LISTEN", format!("127.0.0.1:{}", self.port)).env("DATABASE_ADMIN_PASSWORD", ADMIN_PASSWORD).env_remove("DATABASE_SOCKET").output().expect("the command runs");
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

/// What stands between a standby and its primary here: it passes bytes
/// both ways, until it is told to cut.
struct Relay {
    port: u16,
    /// Raised to drop every connection now open; they see it and end.
    cuts: Arc<AtomicI64>,
    /// While set, new connections are closed as they come.
    down: Arc<AtomicBool>,
}

impl Relay {
    async fn to(primary: u16) -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (cuts, down) = (Arc::new(AtomicI64::new(0)), Arc::new(AtomicBool::new(false)));
        let (cut, is_down) = (cuts.clone(), down.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut from, _)) = listener.accept().await else { return };
                if is_down.load(Ordering::Relaxed) {
                    continue;
                }
                let cut = cut.clone();
                tokio::spawn(async move {
                    let Ok(mut to) = TcpStream::connect(("127.0.0.1", primary)).await else { return };
                    let began = cut.load(Ordering::Relaxed);
                    let passing = tokio::io::copy_bidirectional(&mut from, &mut to);
                    tokio::pin!(passing);
                    loop {
                        tokio::select! {
                            _ = &mut passing => return,
                            _ = tokio::time::sleep(Duration::from_millis(5)) => {
                                // Dropped where they are, in the middle
                                // of whatever was passing.
                                if cut.load(Ordering::Relaxed) != began {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        Relay { port, cuts, down }
    }

    fn cut(&self) {
        self.cuts.fetch_add(1, Ordering::Relaxed);
    }

    fn down(&self, down: bool) {
        self.down.store(down, Ordering::Relaxed);
        self.cut();
    }
}

async fn schema(db: &Client) -> DbResult<()> {
    db.create_table_if_not_exists(
        TableDef::new("entries").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("batch", Type::Int)).column(ColumnDef::new("payload", Type::Text)).primary_key(["id"]).index(IndexDef::new("entries_batch").on(["batch", "id"])),
    )
    .await?;
    db.create_table_if_not_exists(TableDef::new("batches").column(ColumnDef::new("batch", Type::Int)).column(ColumnDef::new("rows", Type::Int)).primary_key(["batch"])).await
}

/// Writes batches, each one transaction of a row in `batches` and
/// several in `entries`, until told to stop or the server goes away.
async fn writer(db: Client, next: Arc<AtomicI64>, stop: Arc<AtomicBool>, acknowledged: Arc<Mutex<Vec<(i64, i64)>>>) {
    while !stop.load(Ordering::Relaxed) {
        let id = next.fetch_add(1, Ordering::Relaxed);
        let rows = id % 5 + 1;
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
            Ok(()) => acknowledged.lock().unwrap().push((id, rows)),
            Err(_) => return,
        }
    }
}

/// One batch of one row, by itself.
async fn one_batch(db: &Client, id: i64) -> DbResult<()> {
    db.transaction(|tx| async move {
        tx.insert(Insert::into("entries").set("id", id * 100).set("batch", id).set("payload", "own")).await?;
        tx.insert(Insert::into("batches").set("batch", id).set("rows", 1)).await?;
        Ok(())
    })
    .await
}

/// The batches a database holds, read at one moment: each must be there
/// whole, its row in `batches` and exactly its rows in `entries`. A
/// commit that was half applied would show here.
async fn batches(db: &Client) -> DbResult<HashMap<i64, i64>> {
    let tx = db.begin_read().await?;
    let stored: HashMap<i64, i64> = tx.query(Query::table("batches")).await?.iter().map(|r| (r.get("batch").unwrap(), r.get("rows").unwrap())).collect();
    let counts = tx.query(Query::table("entries").index("entries_batch", KeyRange::all()).group_by([col("batch")]).select("batch", col("batch")).select("n", count())).await?;
    tx.commit().await?;
    assert_eq!(counts.len(), stored.len(), "entries without their batch, or a batch without entries");
    for row in &counts {
        assert_eq!(stored.get(&row.get::<i64>("batch")?), Some(&row.get::<i64>("n")?), "a batch is partly there");
    }
    Ok(stored)
}

/// Waits until the standby's copy of `test` holds what the primary's
/// does, and returns it.
async fn caught_up(primary: &Process, standby: &Process, what: &str) -> HashMap<i64, i64> {
    let began = Instant::now();
    let mut last;
    loop {
        let at_primary = batches(&primary.connect("test").await.expect("the primary answers")).await.expect("the primary reads");
        match standby.connect("test").await {
            Ok(db) => match batches(&db).await {
                Ok(at_standby) if at_standby == at_primary => return at_primary,
                Ok(at_standby) => last = format!("it holds {} batches of {}", at_standby.len(), at_primary.len()),
                Err(e) => last = e.message,
            },
            Err(e) => last = e.message,
        }
        assert!(began.elapsed() < Duration::from_secs(60), "{what}: the standby did not catch up: {last}\n{}", standby.stats().await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Loads the primary from four connections while a reader watches the
/// standby, until `more` batches were acknowledged; then `disturb` is
/// called, with the load still running where it can.
async fn load(primary: &Process, standby: Option<&Process>, next: &Arc<AtomicI64>, acknowledged: &Arc<Mutex<Vec<(i64, i64)>>>, more: usize) -> (Arc<AtomicBool>, Vec<tokio::task::JoinHandle<()>>) {
    let stop = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::new();
    for _ in 0..4 {
        tasks.push(tokio::spawn(writer(primary.connect("test").await.unwrap(), next.clone(), stop.clone(), acknowledged.clone())));
    }
    if let Some(standby) = standby {
        // What the standby shows while the log arrives: whole batches,
        // and never fewer than it showed before.
        let (db, stop) = (standby.connect("test").await.unwrap(), stop.clone());
        tasks.push(tokio::spawn(async move {
            let mut seen = 0;
            while !stop.load(Ordering::Relaxed) {
                let Ok(held) = batches(&db).await else { return };
                assert!(held.len() >= seen, "the standby went back from {seen} batches to {}", held.len());
                seen = held.len();
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
        }));
    }
    let before = acknowledged.lock().unwrap().len();
    let waiting = Instant::now();
    while acknowledged.lock().unwrap().len() < before + more {
        assert!(waiting.elapsed() < Duration::from_secs(60), "the load does not commit");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (stop, tasks)
}

async fn ended(stop: Arc<AtomicBool>, tasks: Vec<tokio::task::JoinHandle<()>>) {
    stop.store(true, Ordering::Relaxed);
    for task in tasks {
        task.await.expect("a writer, or the reader of the standby, found something wrong");
    }
}

// R-O11, R-O12, R-O14
#[tokio::test(flavor = "multi_thread")]
async fn a_standby_holds_whole_commits_through_cuts_and_kills_and_all_of_them_afterwards() -> DbResult<()> {
    let (primary_dir, standby_dir) = (TempDir::new(), TempDir::new());
    let primary_port = free_port();
    let primary_settings = [("DATABASE_KEEP_LOG_MB", "64".to_string())];
    let mut primary = Process::start(primary_dir.path(), primary_port, &primary_settings).await;
    primary.connect("").await?.admin().create_database("test", PASSWORD).await?;
    schema(&primary.connect("test").await?).await?;
    let next = Arc::new(AtomicI64::new(1));
    let acknowledged: Arc<Mutex<Vec<(i64, i64)>>> = Arc::default();
    // Some rows before there is a standby: it begins from a base.
    let (stop, tasks) = load(&primary, None, &next, &acknowledged, 20).await;
    ended(stop, tasks).await;

    let relay = Relay::to(primary_port).await;
    let standby_settings = [("DATABASE_PRIMARY", format!("127.0.0.1:{}", relay.port))];
    let mut standby = Process::start(standby_dir.path(), free_port(), &standby_settings).await;
    caught_up(&primary, &standby, "begun from a base").await;

    // It answers reads with the password the database has on the
    // primary, and takes no writes.
    let copy = standby.connect("test").await?;
    let refused = copy.insert(Insert::into("batches").set("batch", -1).set("rows", 0)).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::ReadOnly, "{}", refused.message);
    assert_eq!(copy.transaction(|tx| async move { tx.insert(Insert::into("batches").set("batch", -1).set("rows", 0)).await.map(|_| ()) }).await.unwrap_err().code, ErrorCode::ReadOnly);
    drop(copy);

    for round in 0..5 {
        let (stop, tasks) = load(&primary, Some(&standby), &next, &acknowledged, 30).await;
        match round {
            // The connection is cut in the middle of the stream.
            0 => relay.cut(),
            // The primary cannot be reached for a while, and commits go on.
            1 => {
                relay.down(true);
                tokio::time::sleep(Duration::from_millis(1500)).await;
                relay.down(false);
            }
            // The standby is killed in the middle of applying.
            2 => {
                ended(stop, tasks).await;
                standby.kill();
                let (stop, tasks) = load(&primary, None, &next, &acknowledged, 20).await;
                ended(stop, tasks).await;
                standby = Process::start(standby_dir.path(), free_port(), &standby_settings).await;
                caught_up(&primary, &standby, "after the standby was killed").await;
                continue;
            }
            // The primary is killed, and comes back.
            3 => {
                primary.kill();
                ended(stop, tasks).await;
                // Its standby still reads, as of some commit.
                batches(&standby.connect("test").await?).await?;
                primary = Process::start(primary_dir.path(), primary_port, &primary_settings).await;
                caught_up(&primary, &standby, "after the primary was killed").await;
                continue;
            }
            // The primary is stopped in good order, and comes back.
            _ => {
                ended(stop, tasks).await;
                let mut child = std::mem::replace(&mut primary, Process { child: Command::new("true").spawn().unwrap(), port: 0 });
                Command::new("kill").args(["-TERM", &child.child.id().to_string()]).status().unwrap();
                child.child.wait().unwrap();
                primary = Process::start(primary_dir.path(), primary_port, &primary_settings).await;
                caught_up(&primary, &standby, "after the primary was stopped and started").await;
                continue;
            }
        }
        ended(stop, tasks).await;
        caught_up(&primary, &standby, &format!("round {round}")).await;
    }

    // Everything acknowledged is on the primary, and so on the standby.
    let held = caught_up(&primary, &standby, "at the end").await;
    for (id, rows) in acknowledged.lock().unwrap().iter() {
        assert_eq!(held.get(id), Some(rows), "batch {id} was acknowledged and is gone");
    }
    standby.connect("test").await?.admin().check().await?;
    // Each says how it stands with the other, once the standby has
    // found the primary that was just started again.
    let waiting = Instant::now();
    loop {
        let (at_standby, at_primary) = (standby.stats().await, primary.stats().await);
        if at_standby.contains("standby copy test") && at_standby.contains("it has all of it") && at_standby.contains("standby: \"test\": following") && at_primary.contains("1 standbys are being sent it") {
            break;
        }
        assert!(waiting.elapsed() < Duration::from_secs(20), "{at_standby}\n{at_primary}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

// R-O13
#[tokio::test(flavor = "multi_thread")]
async fn a_promoted_standby_takes_writes_and_is_never_again_a_copy_of_the_primary_it_left() -> DbResult<()> {
    let (primary_dir, standby_dir) = (TempDir::new(), TempDir::new());
    let (primary_port, standby_port) = (free_port(), free_port());
    let primary_settings = [("DATABASE_KEEP_LOG_MB", "64".to_string())];
    let standby_settings = [("DATABASE_PRIMARY", format!("127.0.0.1:{primary_port}"))];
    let primary = Process::start(primary_dir.path(), primary_port, &primary_settings).await;
    primary.connect("").await?.admin().create_database("test", PASSWORD).await?;
    schema(&primary.connect("test").await?).await?;
    let next = Arc::new(AtomicI64::new(1));
    let acknowledged: Arc<Mutex<Vec<(i64, i64)>>> = Arc::default();
    let standby = Process::start(standby_dir.path(), standby_port, &standby_settings).await;
    let (stop, tasks) = load(&primary, Some(&standby), &next, &acknowledged, 40).await;
    ended(stop, tasks).await;
    let held = caught_up(&primary, &standby, "before the primary is lost").await;

    // Started without its primary, as any server is started, it is a
    // standby still: it reads, and refuses writes.
    standby.kill();
    let standby = Process::start(standby_dir.path(), standby_port, &[]).await;
    let copy = standby.connect("test").await?;
    assert_eq!(batches(&copy).await?, held);
    assert_eq!(copy.insert(Insert::into("batches").set("batch", -1).set("rows", 0)).await.unwrap_err().code, ErrorCode::ReadOnly);
    drop(copy);

    // The primary is lost, and the standby promoted.
    primary.kill();
    let (promoted, said) = standby.command(&["promote"]);
    assert!(promoted && said.contains("test: promoted at"), "{said}");
    let (again, said) = standby.command(&["promote"]);
    assert!(!again && said.contains("nothing to promote"), "{said}");
    let own = standby.connect("test").await?;
    assert_eq!(batches(&own).await?, held);
    one_batch(&own, 1_000_001).await?;

    // The old primary comes back and goes on by itself; the promoted
    // server is started as its standby once more, by a mistake in its
    // settings. It does not take the old primary's log over what it
    // has committed since: its database is its own, and it says so.
    let primary = Process::start(primary_dir.path(), primary_port, &primary_settings).await;
    one_batch(&primary.connect("test").await?, 2_000_002).await?;
    standby.kill();
    let standby = Process::start(standby_dir.path(), standby_port, &standby_settings).await;
    let own = standby.connect("test").await?;
    let waiting = Instant::now();
    while !standby.stats().await.contains("is a database of this server's own") {
        assert!(waiting.elapsed() < Duration::from_secs(20), "{}", standby.stats().await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    one_batch(&own, 1_000_003).await?;
    let now = batches(&own).await?;
    assert!(now.contains_key(&1_000_001) && now.contains_key(&1_000_003) && !now.contains_key(&2_000_002));
    assert_eq!(now.len(), held.len() + 2);

    // The other way round: the old primary made a standby of the new
    // one. Its own database of that name is left as it is, until the
    // operator drops it; then it is copied from the new primary.
    primary.kill();
    let old = Process::start(primary_dir.path(), primary_port, &[("DATABASE_PRIMARY", format!("127.0.0.1:{standby_port}"))]).await;
    let waiting = Instant::now();
    while !old.stats().await.contains("is a database of this server's own") {
        assert!(waiting.elapsed() < Duration::from_secs(20), "{}", old.stats().await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(batches(&old.connect("test").await?).await?.contains_key(&2_000_002));
    old.connect("").await?.admin().drop_database("test").await?;
    let held = caught_up(&standby, &old, "the old primary as a standby of the new").await;
    assert!(held.contains_key(&1_000_003) && !held.contains_key(&2_000_002));
    Ok(())
}

// R-O12
#[tokio::test(flavor = "multi_thread")]
async fn a_standby_behind_what_its_primary_keeps_reads_the_archive_or_begins_again() -> DbResult<()> {
    for with_archive in [false, true] {
        let (primary_dir, standby_dir, archive) = (TempDir::new(), TempDir::new(), TempDir::new());
        let primary_port = free_port();
        // A primary that keeps no log past its checkpoints.
        let mut primary_settings = Vec::new();
        let mut standby_settings = vec![("DATABASE_PRIMARY", format!("127.0.0.1:{primary_port}"))];
        if with_archive {
            primary_settings.push(("DATABASE_ARCHIVE", archive.path().display().to_string()));
            standby_settings.push(("DATABASE_ARCHIVE", archive.path().display().to_string()));
        }
        let primary = Process::start(primary_dir.path(), primary_port, &primary_settings).await;
        let admin = primary.connect("").await?;
        admin.admin().create_database("test", PASSWORD).await?;
        schema(&primary.connect("test").await?).await?;
        let next = Arc::new(AtomicI64::new(1));
        let acknowledged: Arc<Mutex<Vec<(i64, i64)>>> = Arc::default();
        let (stop, tasks) = load(&primary, None, &next, &acknowledged, 20).await;
        ended(stop, tasks).await;
        if with_archive {
            // So that the archive has it all, and the standby begins there.
            admin.admin().checkpoint().await?;
        }
        let standby = Process::start(standby_dir.path(), free_port(), &standby_settings).await;
        caught_up(&primary, &standby, "begun").await;
        // The standby is away while the primary commits and checkpoints
        // past what the standby has.
        standby.kill();
        for _ in 0..3 {
            let (stop, tasks) = load(&primary, None, &next, &acknowledged, 15).await;
            ended(stop, tasks).await;
            admin.admin().checkpoint().await?;
        }
        let standby = Process::start(standby_dir.path(), free_port(), &standby_settings).await;
        let held = caught_up(&primary, &standby, if with_archive { "from the archive" } else { "from a new base" }).await;
        assert!(held.len() >= 65);
        standby.connect("test").await?.admin().check().await?;
        // A standby keeps no archive of its own.
        if with_archive {
            let histories = std::fs::read_dir(archive.path()).unwrap().count();
            assert_eq!(histories, 1, "one history: the primary's");
        }
    }
    Ok(())
}
