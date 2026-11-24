//! The same workloads against this database and against Postgres, and
//! some of them against the SQLite family.
//!
//! ```text
//! cargo build --release
//! cargo run --release -p database-bench            # this and Postgres, side by side
//! cargo run --release -p database-bench -- --only ours
//! cargo run --release -p database-bench --features sqlite -- --only ours,sqlite,rqlite,libsql
//! ```
//!
//! Each server runs as its own process on its own empty directory under
//! `BENCH_DIR` (default: the system's temporary directory), with commits
//! synced to disk. Postgres is started as a small deployment would run
//! it (`shared_buffers=64MB`), as the user `postgres` when the benchmark
//! runs as root. The workloads are hot paths common to web applications:
//! a session lookup, a durable insert, a rate-limit upsert, a mailbox
//! page, a flag update, a small transaction, a filtered count, a search.
//!
//! `--only` takes the targets to run, by name and separated by commas:
//! `ours`, `postgres`, `sqlite` (embedded, see `sqlite.rs`), `rqlite`
//! and `libsql` (servers, see `family.rs`), or `all`. The SQLite family
//! runs the session lookup, the durable insert and the transfers.
//! `limits.rs` runs each server on a few CPUs and under a memory limit.
//!
//! What is printed: operations per second and latency for each workload,
//! then what each server cost: memory, CPU time, and disk.
//!
//! Both clients prepare each statement once per connection and send its
//! values after that. `BENCH_SOCKET=1` has both servers listen on a Unix
//! domain socket and both clients connect through it, as an application
//! on the server's own host could.

mod family;
mod http;
mod limits;
#[cfg(feature = "sqlite")]
mod sqlite;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use database_client::*;
use tokio_postgres::types::ToSql;

use limits::limits;

const USERS: i64 = 20;
const MAILS_PER_USER: i64 = 5_000;
const SESSIONS: i64 = 20_000;
/// Connections each server is given: as many as the busiest workload uses.
const CONNECTIONS: usize = 16;

// ------------------------------------------------------------ processes

fn pids_of(root: u32) -> Vec<u32> {
    // The server process and every descendant: Postgres forks a process
    // per connection and several helpers.
    let mut all = vec![root];
    let mut at = 0;
    while at < all.len() {
        let parent = all[at];
        at += 1;
        if let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) {
            for task in tasks.flatten() {
                if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                    all.extend(children.split_whitespace().filter_map(|c| c.parse::<u32>().ok()));
                }
            }
        }
    }
    all
}

/// Memory the processes hold, counting shared pages once (PSS), in bytes.
fn memory(root: u32) -> u64 {
    pids_of(root)
        .iter()
        .filter_map(|pid| std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok())
        .filter_map(|text| text.lines().find(|l| l.starts_with("Pss:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .sum::<u64>()
        * 1024
}

/// CPU seconds the server and every process it has ever reaped used.
fn cpu_seconds(root: u32) -> f64 {
    let ticks: u64 = pids_of(root)
        .iter()
        .filter_map(|pid| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok())
        .map(|stat| {
            // Fields after the command name, which may hold spaces.
            let rest: Vec<&str> = stat.rsplit_once(')').map_or("", |(_, r)| r).split_whitespace().collect();
            let field = |n: usize| rest.get(n - 3).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            // utime, stime, and the same of children waited for.
            field(14) + field(15) + field(16) + field(17)
        })
        .sum();
    ticks as f64 / 100.0
}

/// The bytes of the files under `dir`, as (data, log): the write-ahead
/// log is counted apart, because Postgres keeps its used segments to
/// write over again (up to a gigabyte, as it is configured here) while
/// this server deletes its log at a checkpoint.
fn disk_bytes(dir: &Path) -> (u64, u64) {
    let (mut data, mut log) = (0, 0);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if meta.is_dir() {
                let (d, l) = disk_bytes(&entry.path());
                if name == "pg_wal" {
                    log += d + l;
                } else {
                    data += d;
                    log += l;
                }
            } else if name.contains(".wal-") || name.ends_with("-wal") || name == "raft.db" {
                // This server's log, SQLite's, and rqlite's Raft log.
                log += meta.len();
            } else {
                data += meta.len();
            }
        }
    }
    (data, log)
}

struct Server {
    /// `None` for embedded SQLite, which runs in this process.
    child: Option<Child>,
    dir: PathBuf,
    as_postgres: bool,
    /// The memory cgroup the server runs in, if it runs under a limit.
    /// Removed after the server has stopped.
    cage: Option<limits::Cage>,
    name: &'static str,
}

impl Server {
    fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            // Asked to stop first, so that a server run under a profiler
            // can write what it gathered; killed if it does not.
            let _ = Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
            for _ in 0..600 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        // Said here too, because a run whose server was killed ends in
        // a panic about a lost connection and never reaches its report.
        if let Some(kills) = self.cage.as_ref().map(limits::Cage::kills).filter(|k| *k > 0) {
            eprintln!("{}: killed {kills} time(s) for going over its memory limit", self.name);
        }
        if self.as_postgres {
            let _ = Command::new("runuser").args(["-u", "postgres", "--", "rm", "-rf"]).arg(&self.dir).status();
        } else if !keep() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// `BENCH_KEEP=1` leaves this server's data where it is when the run
/// ends and starts the next run from it, loaded as it is: to try a
/// change on the reading workloads without loading the mails each time.
fn keep() -> bool {
    std::env::var_os("BENCH_KEEP").is_some()
}

/// `BENCH_SOCKET=1`: both servers are reached through a Unix domain
/// socket instead of TCP on the loopback address.
fn through_socket() -> bool {
    std::env::var_os("BENCH_SOCKET").is_some()
}

fn bench_dir(name: &str) -> PathBuf {
    let base = std::env::var("BENCH_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    if keep() && name == "ours" {
        return base.join("database-bench-ours-kept");
    }
    base.join(format!("database-bench-{name}-{}", std::process::id()))
}

async fn start_ours(binary: &Path, cache_mb: u32) -> (Server, Client) {
    let dir = bench_dir("ours");
    if !keep() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    let socket = through_socket().then(|| dir.with_extension("sock"));
    let port = free_port();
    let cage = limits().cage("ours");
    let child = limits()
        .command(cage.as_ref(), binary)
        .arg("serve")
        .env("DATABASE_DATA", &dir)
        .env("DATABASE_LISTEN", format!("127.0.0.1:{port}"))
        .env("DATABASE_ADMIN_PASSWORD", "bench-admin")
        .env("DATABASE_CACHE_MB", cache_mb.to_string())
        .envs(socket.iter().map(|socket| ("DATABASE_SOCKET", socket)))
        // BENCH_SERVER_LOG=file keeps what the server says.
        .stderr(match std::env::var_os("BENCH_SERVER_LOG") {
            Some(path) => Stdio::from(std::fs::File::create(path).expect("the server's log file")),
            None => Stdio::null(),
        })
        .spawn()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}. Build it first: cargo build --release", binary.display()));
    let server = Server { child: Some(child), dir, as_postgres: false, cage, name: "database" };
    for _ in 0..200 {
        let mut admin = Config::new("127.0.0.1", port, "", "bench-admin");
        admin.socket = socket.clone();
        if let Ok(admin) = Client::connect(admin).await {
            admin.admin().create_database_if_not_exists("bench", "bench").await.unwrap();
            let mut config = Config::new("127.0.0.1", port, "bench", "bench");
            config.socket = socket.clone();
            config.pool_size = CONNECTIONS;
            return (server, Client::connect(config).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the database server did not come up");
}

struct Pg {
    clients: Vec<Arc<PgConn>>,
}

/// One connection to Postgres. Each statement is prepared the first time
/// the connection runs it and reused after, as a driver with a statement
/// cache does: Postgres then parses and plans it once.
struct PgConn {
    client: tokio_postgres::Client,
    prepared: std::sync::Mutex<std::collections::HashMap<String, tokio_postgres::Statement>>,
}

impl PgConn {
    async fn prepared(&self, sql: &str) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
        if let Some(statement) = self.prepared.lock().unwrap().get(sql) {
            return Ok(statement.clone());
        }
        let statement = self.client.prepare(sql).await?;
        self.prepared.lock().unwrap().insert(sql.to_string(), statement.clone());
        Ok(statement)
    }

    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
        self.client.query(&self.prepared(sql).await?, params).await
    }

    async fn query_one(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<tokio_postgres::Row, tokio_postgres::Error> {
        self.client.query_one(&self.prepared(sql).await?, params).await
    }

    async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, tokio_postgres::Error> {
        self.client.execute(&self.prepared(sql).await?, params).await
    }

    async fn batch_execute(&self, sql: &str) -> Result<(), tokio_postgres::Error> {
        self.client.batch_execute(sql).await
    }
}

async fn start_postgres() -> Option<(Server, Pg)> {
    let bin = ["/usr/lib/postgresql/16/bin", "/usr/lib/postgresql/17/bin", "/usr/local/bin", "/usr/bin"].iter().map(Path::new).find(|d| d.join("initdb").exists())?;
    let root = std::fs::read_to_string("/proc/self/status").ok()?.lines().any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(1) == Some("0"));
    let dir = bench_dir("postgres");
    // `limited` is the memory cgroup to run in, for the server itself:
    // what prepares its directory runs under no limit.
    let run = |program: &Path, args: &[&str], limited: Option<Option<&limits::Cage>>| -> Command {
        // Postgres refuses to run as root.
        let first = if root { Path::new("runuser") } else { program };
        let mut c = match limited {
            Some(cage) => limits().command(cage, first),
            None => Command::new(first),
        };
        if root {
            c.args(["-u", "postgres", "--"]).arg(program);
        }
        c.args(args);
        c
    };
    let _ = run(Path::new("rm"), &["-rf", dir.to_str()?], None).status();
    std::fs::create_dir_all(&dir).ok()?;
    if root {
        Command::new("chown").arg("postgres").arg(&dir).status().ok()?;
    }
    let data = dir.join("data");
    let init = run(&bin.join("initdb"), &["-D", data.to_str()?, "--auth=trust", "--no-sync", "-E", "UTF8", "--locale=C.UTF-8"], None).stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?;
    if !init.success() {
        return None;
    }
    let port = free_port();
    let cage = limits().cage("postgres");
    let child = run(
        &bin.join("postgres"),
        &["-D", data.to_str()?, "-p", &port.to_string(), "-c", "listen_addresses=127.0.0.1", "-c", "shared_buffers=64MB", "-c", "unix_socket_directories=", "-c", "max_connections=100"],
        Some(cage.as_ref()),
    )
    // Its socket, when it is to be reached through one, goes in its own
    // directory, which the user it runs as owns. (Of a setting given
    // twice, the last holds.)
    .args(through_socket().then(|| ["-c".to_string(), format!("unix_socket_directories={}", dir.display())]).into_iter().flatten())
    .stderr(Stdio::null())
    .spawn()
    .ok()?;
    let server = Server { child: Some(child), dir, as_postgres: root, cage, name: "Postgres 16" };
    let user = if root { "postgres".to_string() } else { std::env::var("USER").unwrap_or_else(|_| "postgres".to_string()) };
    let host = if through_socket() { server.dir.to_str()?.to_string() } else { "127.0.0.1".to_string() };
    let target = format!("host={host} port={port} user={user} dbname=postgres");
    for _ in 0..200 {
        if tokio_postgres::connect(&target, tokio_postgres::NoTls).await.is_ok() {
            let mut clients = Vec::new();
            // As many connections as the busiest workload uses.
            for _ in 0..CONNECTIONS {
                let (client, connection) = tokio_postgres::connect(&target, tokio_postgres::NoTls).await.ok()?;
                tokio::spawn(connection);
                clients.push(Arc::new(PgConn { client, prepared: Default::default() }));
            }
            return Some((server, Pg { clients }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

// ------------------------------------------------------------- measuring

struct Timing {
    ops: u64,
    seconds: f64,
    p50: Duration,
    p99: Duration,
}

/// Whether a workload is to run. `BENCH_WORKLOADS="transfer,upsert"` runs
/// only those whose name holds one of the words, to study a few without
/// waiting for the rest; the mails are then loaded only if one of them
/// reads mails.
fn selected(name: &str) -> bool {
    match std::env::var("BENCH_WORKLOADS") {
        Ok(words) => words.split(',').any(|word| name.contains(word.trim())),
        Err(_) => true,
    }
}

const BULK_LOAD: &str = "bulk load: 100,000 mails of 4 kB, 250 per statement (4 connections)";
/// The workloads that read the mails.
const ON_MAILS: [usize; 4] = [6, 7, 9, 10];

/// Runs `op(worker, n)` from `workers` tasks for `total` operations.
async fn measure<F, Fut>(workers: usize, total: u64, op: F) -> Timing
where
    F: Fn(usize, u64) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let op = Arc::new(op);
    let next = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    let mut tasks = Vec::new();
    for worker in 0..workers {
        let (op, next) = (op.clone(), next.clone());
        tasks.push(tokio::spawn(async move {
            let mut latencies = Vec::new();
            loop {
                let n = next.fetch_add(1, Ordering::Relaxed);
                if n >= total {
                    return latencies;
                }
                let at = Instant::now();
                op(worker, n).await;
                latencies.push(at.elapsed());
            }
        }));
    }
    let mut latencies = Vec::new();
    for task in tasks {
        latencies.extend(task.await.unwrap());
    }
    let seconds = started.elapsed().as_secs_f64();
    latencies.sort();
    Timing { ops: total, seconds, p50: latencies[latencies.len() / 2], p99: latencies[latencies.len() * 99 / 100] }
}

fn scramble(n: u64) -> u64 {
    n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 17
}

/// A well-mixed number for each `n`: what a workload picks its rows by.
/// (`scramble` steps evenly, so numbers cut from it repeat in a pattern:
/// good enough to make data of, not to choose rows at random.)
fn mix(n: u64) -> u64 {
    let mut z = n.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn token(n: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    for i in 0..4u64 {
        out.extend_from_slice(&scramble(n as u64 * 4 + i + 1).to_le_bytes());
    }
    out
}

fn user_id(n: i64) -> Uuid {
    Uuid((0x1000_0000_0000_0000_0000_0000_0000_0000u128 + n as u128).to_be_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// What a workload's operation is about, from its well-mixed number `r`:
// here once, so that every target is asked for the same rows.

/// The session a lookup asks for.
fn session_of(r: u64) -> i64 {
    (r % SESSIONS as u64) as i64
}

/// The event a durable insert writes: its actor, its action, its detail.
fn event(r: u64) -> (String, &'static str, String) {
    (format!("usr_{}", r % 1000), "login.ok", format!(r#"{{"device":"d{}","ip":"10.0.0.{}"}}"#, r % 50, r % 250))
}

/// The accounts a transfer moves money between: two picked at random
/// (workload 8), or, to see transactions that are in each other's way,
/// always into the same one (workload 11). `None` when it picked the
/// same account twice, and so has nothing to do.
fn transfer_between(index: usize, r: u64) -> Option<(i64, i64)> {
    let (from, to) = if index == 8 { ((r % 1000) as i64, ((r >> 10) % 1000) as i64) } else { ((1 + r % 999) as i64, 0) };
    (from != to).then_some((from, to))
}

fn mail_id(user: i64, n: i64) -> Uuid {
    Uuid((0x2000_0000_0000_0000_0000_0000_0000_0000u128 + ((user as u128) << 64) + scramble(n as u64) as u128).to_be_bytes())
}

const BASE_DATE: i64 = 1_700_000_000_000_000;
const WORDS: &[&str] = &["invoice", "meeting", "report", "holiday", "payment", "update", "question", "delivery", "schedule", "account", "welcome", "reminder"];

fn subject(n: i64) -> String {
    format!("{} about the {} number {n}", WORDS[n as usize % 12], WORDS[(n as usize / 12) % 12])
}

/// About 4 kB of text: mostly words from a vocabulary of 5,000, so that a
/// search for two of them finds a handful of a user's mails, not all.
fn body(n: i64) -> String {
    let mut out = String::with_capacity(4200);
    let mut k = n as u64;
    while out.len() < 4000 {
        k = scramble(k + 1);
        if k % 16 == 0 {
            out.push_str(WORDS[((k >> 8) % 12) as usize]);
        } else {
            out.push_str(&format!("term{}", (k >> 8) % 5000));
        }
        out.push(' ');
    }
    out
}

// --------------------------------------------------------------- schemas

async fn setup_ours(db: &Client) -> DbResult<()> {
    db.create_table(
        TableDef::new("users").column(ColumnDef::new("user_id", Type::Uuid)).column(ColumnDef::new("username", Type::Text)).column(ColumnDef::new("disabled", Type::Bool).default(false)).primary_key(["user_id"]).unique("users_username", ["username"]),
    )
    .await?;
    db.create_table(
        TableDef::new("sessions")
            .column(ColumnDef::new("id", Type::Text))
            .column(ColumnDef::new("user_id", Type::Uuid))
            .column(ColumnDef::new("token_hash", Type::Bytes))
            .column(ColumnDef::new("expires_at", Type::Timestamp))
            .primary_key(["id"])
            .unique("sessions_token", ["token_hash"])
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
    )
    .await?;
    db.create_table(
        TableDef::new("events")
            .column(ColumnDef::new("id", Type::Int).auto_increment())
            .column(ColumnDef::new("at", Type::Timestamp).default_now())
            .column(ColumnDef::new("actor", Type::Text))
            .column(ColumnDef::new("action", Type::Text))
            .column(ColumnDef::new("detail", Type::Json))
            .primary_key(["id"]),
    )
    .await?;
    for (name, volatile) in [("throttle", false), ("throttle_volatile", true)] {
        let def = TableDef::new(name).column(ColumnDef::new("key", Type::Text)).column(ColumnDef::new("window_start", Type::Timestamp)).column(ColumnDef::new("hits", Type::Int)).primary_key(["key"]);
        db.create_table(if volatile { def.volatile() } else { def }).await?;
    }
    db.create_table(
        TableDef::new("mails")
            .column(ColumnDef::new("mail_id", Type::Uuid))
            .column(ColumnDef::new("user_id", Type::Uuid))
            .column(ColumnDef::new("subject", Type::Text))
            .column(ColumnDef::new("date", Type::Timestamp))
            .column(ColumnDef::new("from_text", Type::Text))
            .column(ColumnDef::new("read", Type::Bool).default(false))
            .column(ColumnDef::new("saved", Type::Bool).default(false))
            .column(ColumnDef::new("text", Type::Text))
            .column(ColumnDef::new("updated", Type::Timestamp).default_now())
            .primary_key(["mail_id"])
            .index(IndexDef::new("mails_user_date").on(["user_id"]).on_desc("date").on(["mail_id"]))
            .index(IndexDef::new("mails_search").on(["user_id"]).full_text(TextConfig::English, [(col("subject"), 1.0), (col("text"), 0.4)])),
    )
    .await?;
    db.create_table(TableDef::new("accounts").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("balance", Type::decimal(15, 2))).primary_key(["id"])).await?;
    db.create_table(TableDef::new("ledger").column(ColumnDef::new("id", Type::Int).auto_increment()).column(ColumnDef::new("from_id", Type::Int)).column(ColumnDef::new("to_id", Type::Int)).column(ColumnDef::new("amount", Type::decimal(15, 2))).primary_key(["id"])).await?;
    Ok(())
}

const PG_SCHEMA: &str = "
CREATE TABLE users (user_id uuid PRIMARY KEY, username text NOT NULL UNIQUE, disabled boolean NOT NULL DEFAULT false);
CREATE TABLE sessions (id text PRIMARY KEY, user_id uuid NOT NULL REFERENCES users ON DELETE CASCADE, token_hash bytea NOT NULL UNIQUE, expires_at timestamptz NOT NULL);
CREATE TABLE events (id bigserial PRIMARY KEY, at timestamptz NOT NULL DEFAULT now(), actor text NOT NULL, action text NOT NULL, detail jsonb NOT NULL);
CREATE TABLE throttle (key text PRIMARY KEY, window_start timestamptz NOT NULL, hits bigint NOT NULL);
CREATE UNLOGGED TABLE throttle_volatile (key text PRIMARY KEY, window_start timestamptz NOT NULL, hits bigint NOT NULL);
CREATE TABLE mails (mail_id uuid PRIMARY KEY, user_id uuid NOT NULL, subject text NOT NULL, date timestamptz NOT NULL, from_text text NOT NULL,
  read boolean NOT NULL DEFAULT false, saved boolean NOT NULL DEFAULT false, text text NOT NULL, updated timestamptz NOT NULL DEFAULT now(),
  search_vector tsvector GENERATED ALWAYS AS (setweight(to_tsvector('english', subject), 'A') || setweight(to_tsvector('english', text), 'B')) STORED);
CREATE INDEX mails_user_date ON mails (user_id, date DESC, mail_id);
CREATE INDEX mails_search ON mails USING gin (search_vector);
CREATE TABLE accounts (id bigint PRIMARY KEY, balance numeric(15,2) NOT NULL);
CREATE TABLE ledger (id bigserial PRIMARY KEY, from_id bigint NOT NULL, to_id bigint NOT NULL, amount numeric(15,2) NOT NULL);
";

// ---------------------------------------------------------------- the two

#[derive(Clone)]
enum Target {
    Ours(Client),
    Pg(Arc<Pg>),
}

impl Target {
    fn pg(&self, worker: usize) -> &PgConn {
        match self {
            Target::Pg(pg) => &pg.clients[worker % pg.clients.len()],
            Target::Ours(_) => unreachable!(),
        }
    }
}

fn pg_uuid(u: Uuid) -> String {
    u.to_string()
}

async fn load(target: &Target) {
    match target {
        Target::Ours(db) => {
            let mut users = Insert::into("users").columns(["user_id", "username"]);
            for u in 0..USERS {
                users = users.values([val(user_id(u)), val(format!("user{u}"))]);
            }
            db.insert(users).await.unwrap();
            for chunk in 0..SESSIONS / 500 {
                let mut insert = Insert::into("sessions").columns(["id", "user_id", "token_hash", "expires_at"]);
                for n in chunk * 500..(chunk + 1) * 500 {
                    insert = insert.values([val(format!("ses_{n:08}")), val(user_id(n % USERS)), val(token(n)), val(Timestamp(BASE_DATE * 2))]);
                }
                db.insert(insert).await.unwrap();
            }
            let mut accounts = Insert::into("accounts").columns(["id", "balance"]);
            for id in 0..1000 {
                accounts = accounts.values([val(id), val(1000)]);
            }
            db.insert(accounts).await.unwrap();
        }
        Target::Pg(_) => {
            let pg = target.pg(0);
            pg.batch_execute(PG_SCHEMA).await.unwrap();
            for u in 0..USERS {
                pg.execute("INSERT INTO users (user_id, username) VALUES ($1::text::uuid, $2)", &[&pg_uuid(user_id(u)), &format!("user{u}")]).await.unwrap();
            }
            for chunk in 0..SESSIONS / 500 {
                let ids: Vec<String> = (chunk * 500..(chunk + 1) * 500).map(|n| format!("ses_{n:08}")).collect();
                let users: Vec<String> = (chunk * 500..(chunk + 1) * 500).map(|n| pg_uuid(user_id(n % USERS))).collect();
                let tokens: Vec<Vec<u8>> = (chunk * 500..(chunk + 1) * 500).map(token).collect();
                pg.execute(
                    "INSERT INTO sessions (id, user_id, token_hash, expires_at) SELECT i, u::uuid, t, now() + interval '10 years' FROM unnest($1::text[], $2::text[], $3::bytea[]) AS x(i, u, t)",
                    &[&ids, &users, &tokens],
                )
                .await
                .unwrap();
            }
            pg.execute("INSERT INTO accounts SELECT g, 1000 FROM generate_series(0, 999) g", &[]).await.unwrap();
        }
    }
}

/// Loads the mails: the bulk-load workload. `BENCH_LOAD_PART=10` loads a
/// tenth of them, to study the load itself: the workloads that read the
/// mails expect them all.
async fn load_mails(target: &Target) -> Timing {
    let part = std::env::var("BENCH_LOAD_PART").ok().and_then(|n| n.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(1);
    let total = (USERS * MAILS_PER_USER / 250) as u64 / part;
    let target = target.clone();
    measure(4, total, move |worker, chunk| {
        let target = target.clone();
        async move {
            let rows: Vec<(i64, i64)> = (chunk as i64 * 250..(chunk as i64 + 1) * 250).map(|n| (n % USERS, n / USERS)).collect();
            match &target {
                Target::Ours(db) => {
                    let mut insert = Insert::into("mails").columns(["mail_id", "user_id", "subject", "date", "from_text", "text"]);
                    for (u, n) in &rows {
                        insert = insert.values([val(mail_id(*u, *n)), val(user_id(*u)), val(subject(*n)), val(Timestamp(BASE_DATE + n * 60_000_000)), val(format!("Sender {n} <sender{n}@example.com>")), val(body(*n))]);
                    }
                    db.insert(insert).await.unwrap();
                }
                Target::Pg(_) => {
                    let ids: Vec<String> = rows.iter().map(|(u, n)| pg_uuid(mail_id(*u, *n))).collect();
                    let users: Vec<String> = rows.iter().map(|(u, _)| pg_uuid(user_id(*u))).collect();
                    let subjects: Vec<String> = rows.iter().map(|(_, n)| subject(*n)).collect();
                    let offsets: Vec<f64> = rows.iter().map(|(_, n)| (BASE_DATE / 1_000_000 + n * 60) as f64).collect();
                    let from: Vec<String> = rows.iter().map(|(_, n)| format!("Sender {n} <sender{n}@example.com>")).collect();
                    let bodies: Vec<String> = rows.iter().map(|(_, n)| body(*n)).collect();
                    target
                        .pg(worker)
                        .execute(
                            "INSERT INTO mails (mail_id, user_id, subject, date, from_text, text) SELECT i::uuid, u::uuid, s, to_timestamp(d), f, b FROM unnest($1::text[], $2::text[], $3::text[], $4::float8[], $5::text[], $6::text[]) AS x(i, u, s, d, f, b)",
                            &[&ids, &users, &subjects, &offsets, &from, &bodies],
                        )
                        .await
                        .unwrap();
                }
            }
        }
    })
    .await
}

/// This server's statements, each prepared once: a connection is sent
/// one the first time it runs it and its values after that, as the
/// Postgres client above does with its statements.
struct Statements {
    session: Prepared,
    event: Prepared,
    /// On the durable table and on the volatile one.
    upsert: [Prepared; 2],
    mailbox: Prepared,
    flag: Prepared,
    account: Prepared,
    debit: Prepared,
    /// The debit of a transfer sent whole: only from an account that is
    /// not overdrawn, which the transaction tests after reading it.
    debit_if_funded: Prepared,
    credit: Prepared,
    ledger: Prepared,
    count: Prepared,
    search: Prepared,
}

fn statements() -> &'static Statements {
    static STATEMENTS: std::sync::OnceLock<Statements> = std::sync::OnceLock::new();
    STATEMENTS.get_or_init(|| {
        let upsert = |table: &str| {
            let expired = col("window_start").le(now() - seconds(60));
            Prepared::new(
                Insert::into(table)
                    .columns(["key", "window_start", "hits"])
                    .values([param(0), now(), val(1)])
                    .on_conflict_update(PRIMARY)
                    .update("window_start", when(expired.clone(), now(), col("window_start")))
                    .update("hits", when(expired, 1, col("hits") + 1))
                    .returning(["hits"]),
            )
        };
        Statements {
            session: Prepared::new(
                Query::table("sessions")
                    .alias("s")
                    .index("sessions_token", KeyRange::eq([param(0)]))
                    .join(Query::table("users").alias("u").key([col("s.user_id")]).filter(col("u.disabled").eq(false)))
                    .filter(col("s.expires_at").gt(now()))
                    .select("id", col("s.id"))
                    .select("username", col("u.username")),
            ),
            event: Prepared::new(Insert::into("events").columns(["actor", "action", "detail"]).values([param(0), param(1), param(2)])),
            upsert: [upsert("throttle"), upsert("throttle_volatile")],
            mailbox: Prepared::new(Query::table("mails").index("mails_user_date", KeyRange::eq([param(0)])).limit(50).columns(["mail_id", "subject", "date", "from_text", "read", "saved"]).select("preview", col("text").left(200))),
            flag: Prepared::new(Update::table("mails").key([param(0)]).set("read", true).set("updated", now())),
            account: Prepared::new(Query::table("accounts").key([param(0)])),
            debit: Prepared::new(Update::table("accounts").key([param(0)]).set("balance", col("balance") - 1)),
            debit_if_funded: Prepared::new(Update::table("accounts").key([param(0)]).filter(col("balance").ge(0)).set("balance", col("balance") - 1)),
            credit: Prepared::new(Update::table("accounts").key([param(0)]).set("balance", col("balance") + 1)),
            ledger: Prepared::new(Insert::into("ledger").columns(["from_id", "to_id", "amount"]).values([param(0), param(1), val(1)])),
            count: Prepared::new(Query::table("mails").index("mails_user_date", KeyRange::eq([param(0)])).select("total", count()).select("unread", count().filter(col("read").eq(false)))),
            search: Prepared::new(Query::table("mails").search("mails_search", [param(0)], param(1)).select("mail_id", col("mail_id")).select("subject", col("subject")).select("rank", Expr::Rank).order_by(Expr::Rank.desc()).limit(20)),
        }
    })
}

/// A transfer in one statement, for Postgres: what the batch is to this
/// server. The rows are locked in id order first, so that two transfers
/// never wait on each other; the debit happens only to an account that
/// is not overdrawn, and the rest only if the debit did.
const PG_TRANSFER: &str = "WITH held AS (SELECT id FROM accounts WHERE id IN ($1, $2) ORDER BY id FOR UPDATE), \
     debit AS (UPDATE accounts SET balance = balance - 1 WHERE id = $1 AND balance >= 0 AND (SELECT count(*) FROM held) = 2 RETURNING id), \
     credit AS (UPDATE accounts SET balance = balance + 1 WHERE id = $2 AND EXISTS (SELECT 1 FROM debit) RETURNING id) \
     INSERT INTO ledger (from_id, to_id, amount) SELECT $1, $2, 1 WHERE EXISTS (SELECT 1 FROM credit)";

type Workload = (&'static str, usize, u64);

/// The workloads: name, connections, operations.
const WORKLOADS: &[Workload] = &[
    ("session lookup with user (1 connection)", 1, 20_000),
    ("session lookup with user (16 connections)", 16, 60_000),
    ("durable insert (1 connection)", 1, 2_000),
    ("durable insert (16 connections)", 16, 16_000),
    ("rate-limit upsert, durable (8 connections)", 8, 8_000),
    ("rate-limit upsert, unlogged (8 connections)", 8, 40_000),
    ("mailbox page: newest 50 of 5,000 (4 connections)", 4, 8_000),
    ("flag update by key (4 connections)", 4, 6_000),
    ("transfer transaction: 2 reads, 3 writes (8 connections)", 8, 6_000),
    ("count unread of a user's 5,000 mails (4 connections)", 4, 800),
    ("full-text search, top 20 (4 connections)", 4, 2_000),
    (
        "transfer transaction, all into one account (8 connections)",
        8,
        6_000,
    ),
    ("transfer sent whole, as one request (8 connections)", 8, 6_000),
    ("transfer sent whole, all into one account (8 connections)", 8, 6_000),
];

/// The connections and operations of a workload in this run.
fn sized(index: usize) -> (usize, u64) {
    let (name, mut workers, mut total) = WORKLOADS[index];
    // BENCH_CONNECTIONS=1 runs every workload on that many connections.
    if let Some(n) = std::env::var("BENCH_CONNECTIONS")
        .ok()
        .and_then(|n| n.parse().ok())
    {
        workers = n;
    }
    // BENCH_REPEAT="mailbox:20" runs the workloads whose name holds
    // "mailbox" twenty times as long, to study one under a profiler.
    if let Some((what, times)) = std::env::var("BENCH_REPEAT").ok().as_deref().and_then(|spec| spec.split_once(':')) {
        if name.contains(what) {
            total *= times.parse::<u64>().unwrap_or(1);
        }
    }
    (workers, total)
}

async fn run_workload(target: &Target, index: usize) -> Timing {
    let (workers, total) = sized(index);
    let target = target.clone();
    measure(workers, total, move |worker, n| {
        let target = target.clone();
        async move {
            let r = mix(n + 7 * index as u64);
            let st = statements();
            match index {
                0 | 1 => {
                    let hash = token(session_of(r));
                    match &target {
                        Target::Ours(db) => {
                            assert_eq!(db.execute(&st.session, [hash]).await.unwrap().len(), 1);
                        }
                        Target::Pg(_) => {
                            let rows = target
                                .pg(worker)
                                .query("SELECT s.id, u.username FROM sessions s JOIN users u ON u.user_id = s.user_id WHERE s.token_hash = $1 AND s.expires_at > now() AND NOT u.disabled", &[&hash])
                                .await
                                .unwrap();
                            assert_eq!(rows.len(), 1);
                        }
                    }
                }
                2 | 3 => {
                    let (actor, action, detail) = event(r);
                    match &target {
                        Target::Ours(db) => {
                            let json: Json = detail.parse().unwrap();
                            db.execute(&st.event, [Value::from(actor), Value::from(action), Value::from(json)]).await.unwrap();
                        }
                        Target::Pg(_) => {
                            target.pg(worker).execute("INSERT INTO events (actor, action, detail) VALUES ($1, $2, $3::text::jsonb)", &[&actor, &action, &detail]).await.unwrap();
                        }
                    }
                }
                4 | 5 => {
                    let table = if index == 4 { "throttle" } else { "throttle_volatile" };
                    let key = format!("ip:10.0.{}.{}:login", r % 20, r % 250);
                    match &target {
                        Target::Ours(db) => {
                            db.execute(&st.upsert[index - 4], [key]).await.unwrap();
                        }
                        Target::Pg(_) => {
                            let sql = format!(
                                "INSERT INTO {table} AS t (key, window_start, hits) VALUES ($1, now(), 1) ON CONFLICT (key) DO UPDATE SET \
                                 window_start = CASE WHEN t.window_start <= now() - interval '60 seconds' THEN now() ELSE t.window_start END, \
                                 hits = CASE WHEN t.window_start <= now() - interval '60 seconds' THEN 1 ELSE t.hits + 1 END RETURNING hits"
                            );
                            target.pg(worker).query(&sql, &[&key]).await.unwrap();
                        }
                    }
                }
                6 => {
                    let user = user_id((r % USERS as u64) as i64);
                    match &target {
                        Target::Ours(db) => {
                            assert_eq!(db.execute(&st.mailbox, [user]).await.unwrap().len(), 50);
                        }
                        Target::Pg(_) => {
                            let rows = target
                                .pg(worker)
                                .query("SELECT mail_id, subject, date, from_text, read, saved, left(text, 200) FROM mails WHERE user_id = $1::text::uuid ORDER BY date DESC, mail_id LIMIT 50", &[&pg_uuid(user)])
                                .await
                                .unwrap();
                            assert_eq!(rows.len(), 50);
                        }
                    }
                }
                7 => {
                    let id = mail_id((r % USERS as u64) as i64, ((r >> 8) % MAILS_PER_USER as u64) as i64);
                    match &target {
                        Target::Ours(db) => {
                            assert_eq!(db.execute(&st.flag, [id]).await.unwrap().affected(), 1);
                        }
                        Target::Pg(_) => {
                            assert_eq!(target.pg(worker).execute("UPDATE mails SET read = true, updated = now() WHERE mail_id = $1::text::uuid", &[&pg_uuid(id)]).await.unwrap(), 1);
                        }
                    }
                }
                8 | 11 => {
                    let Some((from, to)) = transfer_between(index, r) else { return };
                    match &target {
                        Target::Ours(db) => {
                            db.transaction(|tx| async move {
                                let a: Decimal = tx.execute(&st.account, [from]).await?.one()?.get("balance")?;
                                let _b: Decimal = tx.execute(&st.account, [to]).await?.one()?.get("balance")?;
                                if a.is_negative() {
                                    return Ok(());
                                }
                                tx.execute(&st.debit, [from]).await?;
                                tx.execute(&st.credit, [to]).await?;
                                tx.execute(&st.ledger, [from, to]).await?;
                                Ok::<_, DbError>(())
                            })
                            .await
                            .unwrap();
                        }
                        Target::Pg(_) => {
                            // Rows locked in id order, so that two
                            // transfers never wait on each other.
                            let pg = target.pg(worker);
                            let (low, high) = (from.min(to), from.max(to));
                            let params: [&(dyn ToSql + Sync); 2] = [&low, &high];
                            pg.batch_execute("BEGIN").await.unwrap();
                            pg.query("SELECT balance::text FROM accounts WHERE id IN ($1, $2) ORDER BY id FOR UPDATE", &params).await.unwrap();
                            pg.execute("UPDATE accounts SET balance = balance - 1 WHERE id = $1", &[&from]).await.unwrap();
                            pg.execute("UPDATE accounts SET balance = balance + 1 WHERE id = $1", &[&to]).await.unwrap();
                            pg.execute("INSERT INTO ledger (from_id, to_id, amount) VALUES ($1, $2, 1)", &[&from, &to]).await.unwrap();
                            pg.batch_execute("COMMIT").await.unwrap();
                        }
                    }
                }
                9 => {
                    let user = user_id((r % USERS as u64) as i64);
                    match &target {
                        Target::Ours(db) => {
                            assert_eq!(db.execute(&st.count, [user]).await.unwrap().one().unwrap().get::<i64>("total").unwrap(), MAILS_PER_USER);
                        }
                        Target::Pg(_) => {
                            let row = target.pg(worker).query_one("SELECT count(*), count(*) FILTER (WHERE NOT read) FROM mails WHERE user_id = $1::text::uuid", &[&pg_uuid(user)]).await.unwrap();
                            assert_eq!(row.get::<_, i64>(0), MAILS_PER_USER);
                        }
                    }
                }
                12 | 13 => {
                    // The same transfers, between the same accounts, as
                    // one request: see `Client::batch`.
                    let s = mix(n + 7 * if index == 12 { 8 } else { 11 });
                    let (from, to) = if index == 12 { ((s % 1000) as i64, ((s >> 10) % 1000) as i64) } else { ((1 + s % 999) as i64, 0) };
                    if from == to {
                        return;
                    }
                    match &target {
                        Target::Ours(db) => {
                            let transfer = Batch::new().add_prepared(&st.debit_if_funded, [from]).expect(1).add_prepared(&st.credit, [to]).expect(1).add_prepared(&st.ledger, [from, to]);
                            db.batch(transfer).await.unwrap();
                        }
                        Target::Pg(_) => {
                            assert_eq!(target.pg(worker).execute(PG_TRANSFER, &[&from, &to]).await.unwrap(), 1);
                        }
                    }
                }
                _ => {
                    let user = user_id((r % USERS as u64) as i64);
                    let words = format!("term{} term{}", r % 5000, (r >> 16) % 5000);
                    match &target {
                        Target::Ours(db) => {
                            assert!(db.execute(&st.search, [Value::from(user), Value::from(words)]).await.unwrap().len() <= 20);
                        }
                        Target::Pg(_) => {
                            let rows = target
                                .pg(worker)
                                .query(
                                    "SELECT mail_id, subject, ts_rank(search_vector, q) AS rank FROM mails, plainto_tsquery('english', $2) q WHERE user_id = $1::text::uuid AND search_vector @@ q ORDER BY rank DESC LIMIT 20",
                                    &[&pg_uuid(user), &words],
                                )
                                .await
                                .unwrap();
                            assert!(rows.len() <= 20);
                        }
                    }
                }
            }
        }
    })
    .await
}

struct Report {
    name: String,
    /// The memory and CPU figures are `None` for what has no process of
    /// its own to measure: embedded SQLite runs inside the benchmark.
    idle_memory: Option<u64>,
    memory: Option<u64>,
    /// The most memory held at any moment of the run.
    peak: Option<u64>,
    cpu: Option<f64>,
    disk: (u64, u64),
    /// `None` for what was not run.
    load: Option<Timing>,
    timings: Vec<Option<Timing>>,
    /// What the figures do not say: operations that gave up, a workload
    /// that did not complete, a server killed for its memory.
    notes: Vec<String>,
}

/// What a server's process cost, measured while it still runs.
struct Cost {
    memory: u64,
    peak: u64,
    cpu: f64,
    notes: Vec<String>,
}

fn cost(server: &Server, pid: u32, peak: limits::Peak) -> Cost {
    let (memory, peak, cpu) = (memory(pid), peak.most(), cpu_seconds(pid));
    let mut notes = Vec::new();
    if let Some(cage) = &server.cage {
        let kills = cage.kills();
        if kills > 0 {
            notes.push(format!("{}: killed {kills} time(s) for going over its memory limit", server.name));
        }
    } else if let Some(limit) = limits().memory().filter(|limit| peak > *limit) {
        notes.push(format!("{}: held {} at most, over the limit of {} that was asked for and could not be enforced", server.name, mb(peak), mb(limit)));
    }
    Cost { memory, peak, cpu, notes }
}

async fn run(name: &'static str, server: Server, target: Target, loaded: bool) -> Report {
    let pid = server.pid().expect("a server of its own");
    // What the server holds before it has done anything.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idle_memory = memory(pid);
    let peak = limits::Peak::watch(pid);
    let mails = selected(BULK_LOAD) || ON_MAILS.iter().any(|index| selected(WORKLOADS[*index].0));
    let load = if loaded {
        None
    } else {
        eprintln!("{name}: loading");
        load(&target).await;
        if mails {
            Some(load_mails(&target).await)
        } else {
            None
        }
    };
    let mut timings = Vec::new();
    for (index, (workload, _, _)) in WORKLOADS.iter().enumerate() {
        if !selected(workload) {
            timings.push(None);
            continue;
        }
        eprintln!("{name}: {workload}");
        timings.push(Some(run_workload(&target, index).await));
    }
    let cost = cost(&server, pid, peak);
    // Let each write what it buffers before the files are measured.
    match &target {
        Target::Ours(db) => {
            if std::env::var_os("BENCH_DETAILS").is_some() {
                eprintln!(
                    "{name}: {}",
                    db.admin()
                        .stats()
                        .await
                        .unwrap()
                        .trim_end()
                        .replace('\n', "; ")
                );
            }
            db.admin().checkpoint().await.unwrap()
        }
        Target::Pg(_) => {
            target.pg(0).batch_execute("CHECKPOINT").await.unwrap();
            // BENCH_DETAILS=1 shows how Postgres stores the mails: how
            // much of a body stays in its row, and what each part weighs.
            if std::env::var_os("BENCH_DETAILS").is_some() {
                let sizes = "SELECT pg_relation_size('mails')::float8, (pg_table_size('mails') - pg_relation_size('mails'))::float8, pg_relation_size('mails_search')::float8, pg_relation_size('mails_user_date')::float8, \
                             (SELECT avg(octet_length(text))::float8 FROM mails), (SELECT avg(pg_column_size(text))::float8 FROM mails), (SELECT avg(pg_column_size(search_vector))::float8 FROM mails)";
                let row = target.pg(0).query_one(sizes, &[]).await.unwrap();
                let mb = |i: usize| row.get::<_, f64>(i) / 1_048_576.0;
                eprintln!(
                    "{name}: mails heap {:.0} MB, out-of-row (TOAST) {:.0} MB, search index {:.0} MB, date index {:.0} MB; a body is {:.0} bytes and takes {:.0} stored; a search vector takes {:.0}",
                    mb(0),
                    mb(1),
                    mb(2),
                    mb(3),
                    row.get::<_, f64>(4),
                    row.get::<_, f64>(5),
                    row.get::<_, f64>(6)
                );
            }
        }
    }
    let disk = disk_bytes(&server.dir);
    drop(target);
    drop(server);
    Report { name: name.to_string(), idle_memory: Some(idle_memory), memory: Some(cost.memory), peak: Some(cost.peak), cpu: Some(cost.cpu), disk, load, timings, notes: cost.notes }
}

// ------------------------------------------------------ the SQLite family

/// The workloads the SQLite family runs: the ones it was first compared
/// on. The others need what was not written for it (full-text search
/// set up three ways, the mails loaded three ways).
const FAMILY: [usize; 6] = [0, 1, 2, 3, 8, 11];

#[derive(Clone)]
enum Family {
    /// rqlite or libSQL's server.
    Remote(Arc<family::Remote>),
    /// SQLite in this process, and how its writers reach it.
    #[cfg(feature = "sqlite")]
    Embedded(Arc<sqlite::Embedded>, sqlite::Writers),
}

impl Family {
    /// Whether a workload is run against this member of the family.
    #[cfg_attr(not(feature = "sqlite"), allow(unused_variables))]
    fn runs(&self, index: usize) -> bool {
        match self {
            // Not after a request has failed or gone unanswered.
            Family::Remote(remote) => remote.failure().is_none(),
            #[cfg(feature = "sqlite")]
            Family::Embedded(_, sqlite::Writers::Shared) => true,
            // A connection each differs from one shared connection only
            // where several workers write.
            #[cfg(feature = "sqlite")]
            Family::Embedded(_, sqlite::Writers::Each) => sized(index).0 > 1 && !matches!(index, 0 | 1),
        }
    }
}

/// Runs one workload. Returns its timing, if it completed, and what
/// there is to say beside the timing.
async fn run_family_workload(family: &Family, index: usize) -> (Option<Timing>, Option<String>) {
    let (workers, total) = sized(index);
    let name = WORKLOADS[index].0;
    match family {
        Family::Remote(remote) => {
            remote.fresh().await;
            let asked = remote.clone();
            let timing = measure(workers, total, move |worker, n| {
                let remote = asked.clone();
                async move {
                    if remote.failure().is_some() {
                        return;
                    }
                    let r = mix(n + 7 * index as u64);
                    let done = match index {
                        0 | 1 => remote.session(worker, &hex(&token(session_of(r)))).await,
                        2 | 3 => {
                            let (actor, _, detail) = event(r);
                            remote.insert(worker, &actor, &detail).await
                        }
                        _ => match transfer_between(index, r) {
                            Some((from, to)) => remote.transfer(worker, from, to).await,
                            None => Ok(()),
                        },
                    };
                    if let Err(why) = done {
                        remote.fail(why);
                    }
                }
            })
            .await;
            match remote.failure() {
                Some(why) => (None, Some(format!("{}: \"{name}\" did not complete, and nothing after it was run: {why}", remote.kind().name()))),
                None => (Some(timing), None),
            }
        }
        #[cfg(feature = "sqlite")]
        Family::Embedded(db, writers) => {
            let each = *writers == sqlite::Writers::Each;
            // A quarter of the operations: where a sync is slow, writers
            // that wait on each other's locks take minutes over these.
            let total = if each { (total / 4).max(1) } else { total };
            let (db, writers) = (db.clone(), *writers);
            let (timing, gave_up) = tokio::task::spawn_blocking(move || db.run(index, workers, total, writers)).await.unwrap();
            let note = each.then(|| format!("SQLite (a connection each): \"{name}\" ran {total} operations, of which {gave_up} gave up after waiting {} s for another connection's write lock", sqlite::BUSY.as_secs()));
            (Some(timing), note)
        }
    }
}

/// Runs the family's workloads against one member of it. `server` is
/// its process, or for embedded SQLite only its directory.
async fn run_family(name: &str, server: &Server, family: &Family) -> Report {
    let pid = server.pid();
    if pid.is_some() {
        // What the server holds before it has done anything.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let idle_memory = pid.map(memory);
    let peak = pid.map(limits::Peak::watch);
    let mut notes = Vec::new();
    match family {
        Family::Remote(remote) => {
            eprintln!("{name}: loading");
            if let Err(why) = remote.load().await {
                remote.fail(why.clone());
                notes.push(format!("{name}: the load did not complete, and nothing after it was run: {why}"));
            }
        }
        // Loaded when it was created.
        #[cfg(feature = "sqlite")]
        Family::Embedded(..) => {}
    }
    let mut timings: Vec<Option<Timing>> = WORKLOADS.iter().map(|_| None).collect();
    for index in FAMILY {
        let workload = WORKLOADS[index].0;
        if !selected(workload) || !family.runs(index) {
            continue;
        }
        eprintln!("{name}: {workload}");
        let (timing, note) = run_family_workload(family, index).await;
        timings[index] = timing;
        notes.extend(note);
    }
    let cost = pid.zip(peak).map(|(pid, peak)| cost(server, pid, peak));
    #[cfg(feature = "sqlite")]
    if let Family::Embedded(db, _) = family {
        db.checkpoint().expect("SQLite: a checkpoint");
    }
    let disk = disk_bytes(&server.dir);
    let (memory, peak, cpu) = (cost.as_ref().map(|c| c.memory), cost.as_ref().map(|c| c.peak), cost.as_ref().map(|c| c.cpu));
    notes.extend(cost.into_iter().flat_map(|c| c.notes));
    Report { name: name.to_string(), idle_memory, memory, peak, cpu, disk, load: None, timings, notes }
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

/// Operations a second: to one decimal place where there are few.
fn rate(per_second: f64) -> String {
    if per_second < 100.0 {
        format!("{per_second:.1}")
    } else {
        format!("{per_second:.0}")
    }
}

fn micros(d: Duration) -> String {
    if d.as_micros() >= 10_000 {
        format!("{:.1} ms", d.as_secs_f64() * 1000.0)
    } else {
        format!("{} µs", d.as_micros())
    }
}

fn print(reports: &[Report]) {
    for line in limits().describe() {
        println!("{line}");
        println!();
    }
    let header: Vec<String> = reports.iter().flat_map(|r| [format!("{} ops/s", r.name), "p50".to_string(), "p99".to_string()]).collect();
    println!("| Workload | {} |", header.join(" | "));
    println!("|---|{}", "---:|".repeat(header.len()));
    let row = |name: &str, pick: &dyn Fn(&Report) -> Option<&Timing>| {
        if reports.iter().all(|r| pick(r).is_none()) {
            return;
        }
        let cells: Vec<String> = reports
            .iter()
            .flat_map(|r| match pick(r) {
                Some(t) => [rate(t.ops as f64 / t.seconds), micros(t.p50), micros(t.p99)],
                None => ["-".to_string(), "-".to_string(), "-".to_string()],
            })
            .collect();
        println!("| {name} | {} |", cells.join(" | "));
    };
    row(BULK_LOAD, &|r| r.load.as_ref());
    for (index, (name, _, _)) in WORKLOADS.iter().enumerate() {
        row(name, &|r| r.timings[index].as_ref());
    }
    println!();
    // A dash where there is no process of the target's own to measure.
    let cells = |pick: &dyn Fn(&Report) -> Option<String>| reports.iter().map(|r| pick(r).unwrap_or_else(|| "-".to_string())).collect::<Vec<_>>().join(" | ");
    println!("| | {} |", reports.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(" | "));
    println!("|---|{}", "---:|".repeat(reports.len()));
    println!("| Memory when idle, before any request | {} |", cells(&|r| r.idle_memory.map(mb)));
    println!("| Memory after the run | {} |", cells(&|r| r.memory.map(mb)));
    println!("| Most memory held during the run | {} |", cells(&|r| r.peak.map(mb)));
    println!("| CPU time for the whole run | {} |", cells(&|r| r.cpu.map(|cpu| format!("{cpu:.1} s"))));
    println!("| Data files on disk after a checkpoint | {} |", cells(&|r| Some(mb(r.disk.0))));
    println!("| Log files on disk after a checkpoint | {} |", cells(&|r| Some(mb(r.disk.1))));
    let notes: Vec<&String> = reports.iter().flat_map(|r| &r.notes).collect();
    if !notes.is_empty() {
        println!();
        for note in notes {
            println!("- {note}");
        }
    }
}

/// What `--only` may name.
const TARGETS: [&str; 5] = ["ours", "postgres", "sqlite", "rqlite", "libsql"];

fn main() {
    // Before any thread is started: the limits may move this process to
    // other CPUs than the servers', and its threads go where it is.
    if let Err(why) = limits::Limits::init() {
        eprintln!("{why}");
        std::process::exit(2);
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("the runtime").block_on(bench());
}

async fn bench() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let only: Option<Vec<String>> = args.iter().position(|a| a == "--only").and_then(|i| args.get(i + 1)).map(|list| list.split(',').map(|name| name.trim().to_string()).collect());
    if let Some(unknown) = only.iter().flatten().find(|name| *name != "all" && !TARGETS.contains(&name.as_str())) {
        eprintln!("--only {unknown}: not a target. The targets are {}, or all; several are separated by commas", TARGETS.join(", "));
        std::process::exit(2);
    }
    // With no --only: this server and Postgres, on every workload.
    let wanted = |name: &str| match &only {
        Some(list) => list.iter().any(|o| o == name || o == "all"),
        None => matches!(name, "ours" | "postgres"),
    };
    let binary = args.iter().position(|a| a == "--binary").and_then(|i| args.get(i + 1)).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("target/release/database"));
    let mut reports = Vec::new();
    if wanted("ours") {
        let (server, client) = start_ours(&binary, 32).await;
        // Data kept from an earlier run is used as it is.
        let loaded = !client.describe().await.unwrap().is_empty();
        if !loaded {
            setup_ours(&client).await.unwrap();
        }
        reports.push(run("database", server, Target::Ours(client), loaded).await);
    }
    if wanted("postgres") {
        match start_postgres().await {
            Some((server, pg)) => reports.push(run("Postgres 16", server, Target::Pg(Arc::new(pg)), false).await),
            None => eprintln!("Postgres is not installed here, or would not start: skipping it"),
        }
    }
    if wanted("sqlite") {
        #[cfg(feature = "sqlite")]
        {
            let dir = bench_dir("sqlite");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("SQLite: its directory");
            let server = Server { child: None, dir: dir.clone(), as_postgres: false, cage: None, name: "SQLite" };
            if limits().memory().is_some() {
                eprintln!("SQLite: embedded, so it runs inside the benchmark, on its CPUs and under no memory limit");
            }
            eprintln!("SQLite: loading");
            let db = Arc::new(tokio::task::spawn_blocking(move || sqlite::Embedded::create(&dir)).await.unwrap().expect("SQLite: the load"));
            for (name, writers) in [("SQLite (one write connection)", sqlite::Writers::Shared), ("SQLite (a connection each)", sqlite::Writers::Each)] {
                reports.push(run_family(name, &server, &Family::Embedded(db.clone(), writers)).await);
            }
        }
        #[cfg(not(feature = "sqlite"))]
        eprintln!("SQLite: not in this build: cargo build --release -p database-bench --features sqlite");
    }
    for (name, kind) in [("rqlite", family::Kind::Rqlite), ("libsql", family::Kind::Libsql)] {
        if wanted(name) {
            if let Some((server, remote)) = family::start(kind).await {
                reports.push(run_family(kind.name(), &server, &Family::Remote(Arc::new(remote))).await);
            }
        }
    }
    if reports.is_empty() {
        eprintln!("nothing was run");
        std::process::exit(1);
    }
    print(&reports);
}
