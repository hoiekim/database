//! The same workloads against this database and against Postgres.
//!
//! ```text
//! cargo build --release
//! cargo run --release -p database-bench            # both, side by side
//! cargo run --release -p database-bench -- --only ours
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
//! What is printed: operations per second and latency for each workload,
//! then what each server cost: memory, CPU time, and disk.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use database_client::*;
use tokio_postgres::types::ToSql;

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
            } else if name.contains(".wal-") {
                log += meta.len();
            } else {
                data += meta.len();
            }
        }
    }
    (data, log)
}

struct Server {
    child: Child,
    dir: PathBuf,
    as_postgres: bool,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Asked to stop first, so that a server run under a profiler
        // can write what it gathered; killed if it does not.
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        for _ in 0..600 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    let port = free_port();
    let child = Command::new(binary)
        .arg("serve")
        .env("DATABASE_DATA", &dir)
        .env("DATABASE_LISTEN", format!("127.0.0.1:{port}"))
        .env("DATABASE_ADMIN_PASSWORD", "bench-admin")
        .env("DATABASE_CACHE_MB", cache_mb.to_string())
        // BENCH_SERVER_LOG=file keeps what the server says.
        .stderr(match std::env::var_os("BENCH_SERVER_LOG") {
            Some(path) => Stdio::from(std::fs::File::create(path).expect("the server's log file")),
            None => Stdio::null(),
        })
        .spawn()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}. Build it first: cargo build --release", binary.display()));
    let server = Server { child, dir, as_postgres: false };
    for _ in 0..200 {
        if let Ok(admin) = Client::connect(Config::new("127.0.0.1", port, "", "bench-admin")).await {
            admin.admin().create_database_if_not_exists("bench", "bench").await.unwrap();
            let mut config = Config::new("127.0.0.1", port, "bench", "bench");
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
    let run = |program: &Path, args: &[&str]| -> Command {
        // Postgres refuses to run as root.
        if root {
            let mut c = Command::new("runuser");
            c.args(["-u", "postgres", "--"]).arg(program).args(args);
            c
        } else {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
    };
    let _ = run(Path::new("rm"), &["-rf", dir.to_str()?]).status();
    std::fs::create_dir_all(&dir).ok()?;
    if root {
        Command::new("chown").arg("postgres").arg(&dir).status().ok()?;
    }
    let data = dir.join("data");
    let init = run(&bin.join("initdb"), &["-D", data.to_str()?, "--auth=trust", "--no-sync", "-E", "UTF8", "--locale=C.UTF-8"]).stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?;
    if !init.success() {
        return None;
    }
    let port = free_port();
    let child = run(
        &bin.join("postgres"),
        &["-D", data.to_str()?, "-p", &port.to_string(), "-c", "listen_addresses=127.0.0.1", "-c", "shared_buffers=64MB", "-c", "unix_socket_directories=", "-c", "max_connections=100"],
    )
    .stderr(Stdio::null())
    .spawn()
    .ok()?;
    let server = Server { child, dir, as_postgres: root };
    let user = if root { "postgres".to_string() } else { std::env::var("USER").unwrap_or_else(|_| "postgres".to_string()) };
    let target = format!("host=127.0.0.1 port={port} user={user} dbname=postgres");
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
            // The mailbox page shows the first 200 characters of each
            // body: they stay in the row, and the page reads no body.
            .column(ColumnDef::new("text", Type::Text).keep_start(256))
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

/// Loads the mails: the bulk-load workload.
async fn load_mails(target: &Target) -> Timing {
    let total = (USERS * MAILS_PER_USER / 250) as u64;
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
];

async fn run_workload(target: &Target, index: usize) -> Timing {
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
    let target = target.clone();
    measure(workers, total, move |worker, n| {
        let target = target.clone();
        async move {
            let r = mix(n + 7 * index as u64);
            match index {
                0 | 1 => {
                    let hash = token((r % SESSIONS as u64) as i64);
                    match &target {
                        Target::Ours(db) => {
                            let q = Query::table("sessions")
                                .alias("s")
                                .index("sessions_token", KeyRange::eq([hash]))
                                .join(Query::table("users").alias("u").key([col("s.user_id")]).filter(col("u.disabled").eq(false)))
                                .filter(col("s.expires_at").gt(now()))
                                .select("id", col("s.id"))
                                .select("username", col("u.username"));
                            assert_eq!(db.query(q).await.unwrap().len(), 1);
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
                    let (actor, action, detail) = (format!("usr_{}", r % 1000), "login.ok", format!(r#"{{"device":"d{}","ip":"10.0.0.{}"}}"#, r % 50, r % 250));
                    match &target {
                        Target::Ours(db) => {
                            let json: Json = detail.parse().unwrap();
                            db.insert(Insert::into("events").set("actor", actor).set("action", action).set("detail", json)).await.unwrap();
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
                            let expired = col("window_start").le(now() - seconds(60));
                            let upsert = Insert::into(table)
                                .set("key", key)
                                .set("window_start", now())
                                .set("hits", 1)
                                .on_conflict_update(PRIMARY)
                                .update("window_start", when(expired.clone(), now(), col("window_start")))
                                .update("hits", when(expired, 1, col("hits") + 1))
                                .returning(["hits"]);
                            db.insert(upsert).await.unwrap();
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
                            let q = Query::table("mails").index("mails_user_date", KeyRange::eq([user])).limit(50).columns(["mail_id", "subject", "date", "from_text", "read", "saved"]).select("preview", col("text").left(200));
                            assert_eq!(db.query(q).await.unwrap().len(), 50);
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
                            let updated = db.update(Update::table("mails").key([id]).set("read", true).set("updated", now())).await.unwrap();
                            assert_eq!(updated.affected(), 1);
                        }
                        Target::Pg(_) => {
                            assert_eq!(target.pg(worker).execute("UPDATE mails SET read = true, updated = now() WHERE mail_id = $1::text::uuid", &[&pg_uuid(id)]).await.unwrap(), 1);
                        }
                    }
                }
                8 | 11 => {
                    // Between two accounts picked at random, or, to see
                    // transactions that are in each other's way, always
                    // into the same one.
                    let (from, to) = if index == 8 { ((r % 1000) as i64, ((r >> 10) % 1000) as i64) } else { ((1 + r % 999) as i64, 0) };
                    if from == to {
                        return;
                    }
                    match &target {
                        Target::Ours(db) => {
                            db.transaction(|tx| async move {
                                let a: Decimal = tx.get("accounts", [from]).await?.unwrap().get("balance")?;
                                let _b: Decimal = tx.get("accounts", [to]).await?.unwrap().get("balance")?;
                                if a.is_negative() {
                                    return Ok(());
                                }
                                tx.update(Update::table("accounts").key([from]).set("balance", col("balance") - 1)).await?;
                                tx.update(Update::table("accounts").key([to]).set("balance", col("balance") + 1)).await?;
                                tx.insert(Insert::into("ledger").set("from_id", from).set("to_id", to).set("amount", 1)).await?;
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
                            let q = Query::table("mails").index("mails_user_date", KeyRange::eq([user])).select("total", count()).select("unread", count().filter(col("read").eq(false)));
                            assert_eq!(db.query_one(q).await.unwrap().get::<i64>("total").unwrap(), MAILS_PER_USER);
                        }
                        Target::Pg(_) => {
                            let row = target.pg(worker).query_one("SELECT count(*), count(*) FILTER (WHERE NOT read) FROM mails WHERE user_id = $1::text::uuid", &[&pg_uuid(user)]).await.unwrap();
                            assert_eq!(row.get::<_, i64>(0), MAILS_PER_USER);
                        }
                    }
                }
                _ => {
                    let user = user_id((r % USERS as u64) as i64);
                    let words = format!("term{} term{}", r % 5000, (r >> 16) % 5000);
                    match &target {
                        Target::Ours(db) => {
                            let q = Query::table("mails").search("mails_search", [user], words).select("mail_id", col("mail_id")).select("subject", col("subject")).select("rank", Expr::Rank).order_by(Expr::Rank.desc()).limit(20);
                            assert!(db.query(q).await.unwrap().len() <= 20);
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
    name: &'static str,
    idle_memory: u64,
    memory: u64,
    cpu: f64,
    disk: (u64, u64),
    /// `None` for what was not run.
    load: Option<Timing>,
    timings: Vec<Option<Timing>>,
}

async fn run(name: &'static str, server: Server, target: Target, loaded: bool) -> Report {
    let pid = server.child.id();
    // What the server holds before it has done anything.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idle_memory = memory(pid);
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
    let memory = memory(pid);
    let cpu = cpu_seconds(pid);
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
    Report { name, idle_memory, memory, cpu, disk, load, timings }
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
    println!("| | {} |", reports.iter().map(|r| r.name).collect::<Vec<_>>().join(" | "));
    println!("|---|{}", "---:|".repeat(reports.len()));
    println!("| Memory when idle, before any request | {} |", reports.iter().map(|r| mb(r.idle_memory)).collect::<Vec<_>>().join(" | "));
    println!("| Memory after the run | {} |", reports.iter().map(|r| mb(r.memory)).collect::<Vec<_>>().join(" | "));
    println!("| CPU time for the whole run | {} |", reports.iter().map(|r| format!("{:.1} s", r.cpu)).collect::<Vec<_>>().join(" | "));
    println!("| Data files on disk after a checkpoint | {} |", reports.iter().map(|r| mb(r.disk.0)).collect::<Vec<_>>().join(" | "));
    println!("| Log files on disk after a checkpoint | {} |", reports.iter().map(|r| mb(r.disk.1)).collect::<Vec<_>>().join(" | "));
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let only = args.iter().position(|a| a == "--only").and_then(|i| args.get(i + 1)).cloned();
    let binary = args.iter().position(|a| a == "--binary").and_then(|i| args.get(i + 1)).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("target/release/database"));
    let mut reports = Vec::new();
    if only.as_deref() != Some("postgres") {
        let (server, client) = start_ours(&binary, 32).await;
        // Data kept from an earlier run is used as it is.
        let loaded = !client.describe().await.unwrap().is_empty();
        if !loaded {
            setup_ours(&client).await.unwrap();
        }
        reports.push(run("database", server, Target::Ours(client), loaded).await);
    }
    if only.as_deref() != Some("ours") {
        match start_postgres().await {
            Some((server, pg)) => reports.push(run("Postgres 16", server, Target::Pg(Arc::new(pg)), false).await),
            None => eprintln!("Postgres is not installed here, or would not start: skipping it"),
        }
    }
    print(&reports);
}
