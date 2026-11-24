//! Embedded SQLite: the same workloads against a database file opened in
//! the benchmark's own process, with no server and no socket between.
//!
//! Built only with the `sqlite` feature, which compiles SQLite itself:
//!
//! ```text
//! cargo build --release -p database-bench --features sqlite
//! ./target/release/bench --only ours,sqlite
//! ```
//!
//! The file is in WAL mode with `synchronous=FULL`, so every commit is
//! synced before it returns, as every other target's is. (`fullfsync` is
//! set too: on macOS that is the sync Rust's `sync_data` makes, and
//! elsewhere it changes nothing.) Statements are prepared once per
//! connection, and a writing transaction begins with `BEGIN IMMEDIATE`.
//!
//! A worker is a thread. Reads get a connection per worker. Writes are
//! measured two ways, because both are in use:
//!
//! - **one write connection** behind a lock, shared by every worker: how
//!   an application usually runs embedded SQLite from many threads;
//! - **a connection each**, with a busy timeout of ten seconds: writers
//!   then wait for each other inside SQLite, and one that waits out the
//!   timeout gives up. Those are counted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Barrier, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, ErrorCode, TransactionBehavior};

use crate::{event, mix, session_of, token, transfer_between, user_id, Timing, SESSIONS, USERS};

/// How long a writer waits for another's lock before it gives up.
pub const BUSY: Duration = Duration::from_secs(10);

/// `expires_at` of every session, in seconds: far in the future.
const FAR: i64 = 4_000_000_000;

const SCHEMA: &str = "
CREATE TABLE users (user_id BLOB PRIMARY KEY, username TEXT NOT NULL UNIQUE, disabled INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id BLOB NOT NULL REFERENCES users ON DELETE CASCADE, token_hash BLOB NOT NULL UNIQUE, expires_at INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE events (id INTEGER PRIMARY KEY, at INTEGER NOT NULL DEFAULT (unixepoch()), actor TEXT NOT NULL, action TEXT NOT NULL, detail TEXT NOT NULL);
CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL);
CREATE TABLE ledger (id INTEGER PRIMARY KEY, from_id INTEGER NOT NULL, to_id INTEGER NOT NULL, amount INTEGER NOT NULL);
";

/// How workers that write reach the database.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Writers {
    /// Through one connection, one at a time.
    Shared,
    /// Each through a connection of its own.
    Each,
}

pub struct Embedded {
    path: PathBuf,
}

fn locked(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(failure, _) if matches!(failure.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

impl Embedded {
    /// Creates the database in `dir` and loads the users, the sessions
    /// and the accounts: the same rows as every other target gets.
    pub fn create(dir: &Path) -> rusqlite::Result<Embedded> {
        let db = Embedded { path: dir.join("bench.db") };
        let mut conn = db.open()?;
        conn.execute_batch(SCHEMA)?;
        let tx = conn.transaction()?;
        for u in 0..USERS {
            tx.execute("INSERT INTO users (user_id, username) VALUES (?1, ?2)", params![user_id(u).0.to_vec(), format!("user{u}")])?;
        }
        for n in 0..SESSIONS {
            tx.execute("INSERT INTO sessions VALUES (?1, ?2, ?3, ?4)", params![format!("ses_{n:08}"), user_id(n % USERS).0.to_vec(), token(n), FAR])?;
        }
        for id in 0..1000 {
            tx.execute("INSERT INTO accounts VALUES (?1, 100000)", params![id])?;
        }
        tx.commit()?;
        Ok(db)
    }

    fn open(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(BUSY)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA foreign_keys=ON;")?;
        Ok(conn)
    }

    /// Moves the log into the database file, so that the files can be
    /// measured.
    pub fn checkpoint(&self) -> rusqlite::Result<()> {
        self.open()?.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
    }

    /// Runs workload `index` from `workers` threads for `total`
    /// operations. Returns the timing of those that were done, and how
    /// many gave up on a locked database instead.
    pub fn run(&self, index: usize, workers: usize, total: u64, writers: Writers) -> (Timing, u64) {
        let reads = matches!(index, 0 | 1);
        let shared = (!reads && writers == Writers::Shared).then(|| Mutex::new(self.open().expect("SQLite: a connection")));
        let (next, gave_up) = (AtomicU64::new(0), AtomicU64::new(0));
        // Every worker has its connection before the clock starts.
        let ready = Barrier::new(workers + 1);
        let (mut latencies, seconds) = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut own = shared.is_none().then(|| self.open().expect("SQLite: a connection"));
                        ready.wait();
                        let mut latencies = Vec::new();
                        loop {
                            let n = next.fetch_add(1, Ordering::Relaxed);
                            if n >= total {
                                return latencies;
                            }
                            let at = Instant::now();
                            let done = match (&shared, &mut own) {
                                (Some(shared), _) => operation(&mut shared.lock().unwrap(), index, n),
                                (None, Some(own)) => operation(own, index, n),
                                (None, None) => unreachable!("a worker without the shared connection has its own"),
                            };
                            match done {
                                Ok(()) => latencies.push(at.elapsed()),
                                Err(e) if locked(&e) => {
                                    gave_up.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => panic!("SQLite: {e}"),
                            }
                        }
                    })
                })
                .collect();
            ready.wait();
            let started = Instant::now();
            let latencies: Vec<Duration> = handles.into_iter().flat_map(|h| h.join().expect("a worker")).collect();
            (latencies, started.elapsed().as_secs_f64())
        });
        latencies.sort();
        let done = latencies.len();
        let at = |part: usize| latencies.get(done * part / 100).copied().unwrap_or_default();
        (Timing { ops: done as u64, seconds, p50: at(50), p99: at(99) }, gave_up.into_inner())
    }
}

/// One operation of workload `index`, picked as every target picks it.
fn operation(conn: &mut Connection, index: usize, n: u64) -> rusqlite::Result<()> {
    let r = mix(n + 7 * index as u64);
    match index {
        0 | 1 => {
            let hash = token(session_of(r));
            let mut statement = conn.prepare_cached("SELECT s.id, u.username FROM sessions s JOIN users u ON u.user_id = s.user_id WHERE s.token_hash = ?1 AND s.expires_at > unixepoch() AND NOT u.disabled")?;
            let rows: Vec<(String, String)> = statement.query_map(params![hash], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            assert_eq!(rows.len(), 1);
        }
        2 | 3 => {
            let (actor, _, detail) = event(r);
            // BEGIN IMMEDIATE takes the write lock first, so a writer
            // that must wait goes through the busy timeout instead of
            // failing at once when it would have to turn a read lock
            // into a write lock.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.prepare_cached("INSERT INTO events (actor, action, detail) VALUES (?1, 'login.ok', json(?2))")?.execute(params![actor, detail])?;
            tx.commit()?;
        }
        8 | 11 => {
            let Some((from, to)) = transfer_between(index, r) else { return Ok(()) };
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let balance: i64 = tx.prepare_cached("SELECT balance FROM accounts WHERE id = ?1")?.query_row(params![from], |row| row.get(0))?;
            let _other: i64 = tx.prepare_cached("SELECT balance FROM accounts WHERE id = ?1")?.query_row(params![to], |row| row.get(0))?;
            if balance >= 0 {
                tx.prepare_cached("UPDATE accounts SET balance = balance - 100 WHERE id = ?1")?.execute(params![from])?;
                tx.prepare_cached("UPDATE accounts SET balance = balance + 100 WHERE id = ?1")?.execute(params![to])?;
                tx.prepare_cached("INSERT INTO ledger (from_id, to_id, amount) VALUES (?1, ?2, 100)")?.execute(params![from, to])?;
            }
            tx.commit()?;
        }
        other => unreachable!("workload {other} is not run against SQLite"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_workload_runs_both_ways_and_leaves_what_it_should() {
        let dir = std::env::temp_dir().join(format!("database-bench-sqlite-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Embedded::create(&dir).unwrap();
        let count = |sql: &str| -> i64 { db.open().unwrap().query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!((count("SELECT count(*) FROM users"), count("SELECT count(*) FROM sessions"), count("SELECT sum(balance) FROM accounts")), (USERS, SESSIONS, 100_000_000));

        // Lookups find their session whichever way writers are set up.
        for (index, workers) in [(0, 1), (1, 4)] {
            let (timing, gave_up) = db.run(index, workers, 200, Writers::Shared);
            assert_eq!((timing.ops, gave_up), (200, 0));
        }
        // Inserts and transfers, through one connection and through four.
        let mut events = 0;
        for writers in [Writers::Shared, Writers::Each] {
            for index in [2, 3, 8, 11] {
                let (timing, gave_up) = db.run(index, 4, 40, writers);
                // Nothing waits ten seconds here.
                assert_eq!((timing.ops, gave_up), (40, 0), "workload {index}, {writers:?}");
                assert!(timing.seconds > 0.0 && timing.p50 <= timing.p99);
                if index < 8 {
                    events += 40;
                }
            }
        }
        assert_eq!(count("SELECT count(*) FROM events"), events);
        // A transfer moves money and creates none; each wrote one line,
        // but for those that picked the same account twice.
        assert_eq!(count("SELECT sum(balance) FROM accounts"), 100_000_000);
        let made: Vec<(i64, i64)> = [8, 11].into_iter().flat_map(|index| (0..40u64).filter_map(move |n| transfer_between(index, mix(n + 7 * index as u64)))).collect();
        assert_eq!(count("SELECT count(*) FROM ledger"), 2 * made.len() as i64);
        let into_first: i64 = made.iter().map(|(from, to)| if *to == 0 { 100 } else if *from == 0 { -100 } else { 0 }).sum();
        assert!(into_first >= 40 * 100);
        assert_eq!(count("SELECT balance FROM accounts WHERE id = 0"), 100_000 + 2 * into_first);

        db.checkpoint().unwrap();
        assert_eq!(std::fs::metadata(dir.join("bench.db-wal")).map(|m| m.len()).unwrap_or(0), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
