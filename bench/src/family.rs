//! SQLite as a server: rqlite and libSQL's server (`sqld`), each reached
//! over HTTP with JSON.
//!
//! Each is an external program, found through `BENCH_RQLITED` or
//! `BENCH_SQLD` or on the `PATH`, and started on an empty directory as
//! its own process, as this server and Postgres are. They run the
//! workloads the SQLite family is compared on: the session lookup, the
//! durable insert and the two transfers.
//!
//! - **rqlite** (`rqlited -node-id 1`, a single node) is asked through
//!   `/db/query?level=none` and `/db/execute`. It has no transaction that
//!   spans requests, so a transfer is sent whole as one request, with the
//!   balance check folded into the first `UPDATE`.
//! - **libSQL's server** (`sqld -d <dir>`) is asked through Hrana over
//!   HTTP (`/v2/pipeline`). A transfer is an interactive transaction in
//!   two requests: `BEGIN IMMEDIATE` and the reads, then the writes and
//!   `COMMIT`.
//!
//! Keys that are bytes elsewhere travel as hex text here, because
//! rqlite's JSON parameters carry no bytes. One connection, kept open,
//! per worker.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::http::Http;
use crate::limits::limits;
use crate::{bench_dir, free_port, hex, token, user_id, Server, CONNECTIONS, SESSIONS, USERS};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Rqlite,
    Libsql,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Rqlite => "rqlite",
            Kind::Libsql => "libSQL server",
        }
    }
}

/// One statement and its parameters.
type Statement<'a> = (&'a str, Vec<Value>);

/// `expires_at` of every session, in seconds: far in the future.
const FAR: i64 = 4_000_000_000;

const SESSION: &str = "SELECT s.id, u.username FROM sessions s JOIN users u ON u.user_id = s.user_id WHERE s.token_hash = ? AND s.expires_at > unixepoch() AND NOT u.disabled";
const INSERT: &str = "INSERT INTO events (actor, action, detail) VALUES (?, 'login.ok', json(?))";
const BALANCE: &str = "SELECT balance FROM accounts WHERE id = ?";
const DEBIT: &str = "UPDATE accounts SET balance = balance - 100 WHERE id = ?";
const CREDIT: &str = "UPDATE accounts SET balance = balance + 100 WHERE id = ?";
const LEDGER: &str = "INSERT INTO ledger (from_id, to_id, amount) VALUES (?, ?, 100)";

/// The schema of `bench/src/sqlite.rs`, with text where that has bytes.
const SCHEMA: [&str; 5] = [
    "CREATE TABLE users (user_id TEXT PRIMARY KEY, username TEXT NOT NULL UNIQUE, disabled INTEGER NOT NULL DEFAULT 0)",
    "CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users ON DELETE CASCADE, token_hash TEXT NOT NULL UNIQUE, expires_at INTEGER NOT NULL)",
    "CREATE TABLE events (id INTEGER PRIMARY KEY, at INTEGER NOT NULL DEFAULT (unixepoch()), actor TEXT NOT NULL, action TEXT NOT NULL, detail TEXT NOT NULL)",
    "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL)",
    "CREATE TABLE ledger (id INTEGER PRIMARY KEY, from_id INTEGER NOT NULL, to_id INTEGER NOT NULL, amount INTEGER NOT NULL)",
];

/// A parameter as Hrana wants it.
fn hrana_arg(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({"type": "text", "value": text}),
        Value::Number(number) => json!({"type": "integer", "value": number.to_string()}),
        _ => json!({"type": "null"}),
    }
}

/// The body of a Hrana pipeline request: the statements on the stream
/// `baton` names, or on a new one, which is closed after them if `close`.
fn hrana_pipeline(baton: Option<&str>, statements: &[Statement], close: bool) -> Value {
    let mut requests: Vec<Value> = statements.iter().map(|(sql, args)| json!({"type": "execute", "stmt": {"sql": sql, "args": args.iter().map(hrana_arg).collect::<Vec<_>>()}})).collect();
    if close {
        requests.push(json!({"type": "close"}));
    }
    json!({"baton": baton, "requests": requests})
}

/// The body of an rqlite request: each statement an array of its text
/// and its parameters.
fn rqlite_statements(statements: &[Statement]) -> Value {
    Value::Array(statements.iter().map(|(sql, args)| Value::Array(std::iter::once(json!(sql)).chain(args.iter().cloned()).collect())).collect())
}

/// The first row's first column of statement `at` in a Hrana answer.
fn hrana_rows(results: &[Value], at: usize) -> &[Value] {
    results.get(at).and_then(|r| r["response"]["result"]["rows"].as_array()).map_or(&[], Vec::as_slice)
}

pub struct Remote {
    kind: Kind,
    conns: Vec<tokio::sync::Mutex<Http>>,
    /// The first failure, after which the server is asked nothing more.
    failed: Mutex<Option<String>>,
}

impl Remote {
    fn new(kind: Kind, address: &str) -> Remote {
        // BENCH_HTTP_TIMEOUT=seconds: how long one request may take.
        let patience = Duration::from_secs(std::env::var("BENCH_HTTP_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(120));
        Remote { kind, conns: (0..CONNECTIONS).map(|_| tokio::sync::Mutex::new(Http::new(address, patience))).collect(), failed: Mutex::new(None) }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Why the server is no longer asked anything, if it is not.
    pub fn failure(&self) -> Option<String> {
        self.failed.lock().unwrap().clone()
    }

    pub fn fail(&self, why: String) {
        self.failed.lock().unwrap().get_or_insert(why);
    }

    /// Closes every connection, so that a workload starts on new ones
    /// and never on one the server closed while it sat idle.
    pub async fn fresh(&self) {
        for conn in &self.conns {
            conn.lock().await.close();
        }
    }

    /// Hrana: one pipeline request. Returns each statement's result and
    /// the baton that names the stream for the next request.
    async fn pipeline(http: &mut Http, baton: Option<&str>, statements: &[Statement<'_>], close: bool) -> Result<(Vec<Value>, Option<String>), String> {
        let answer = http.post("/v2/pipeline", &hrana_pipeline(baton, statements, close)).await?;
        let results = answer["results"].as_array().cloned().unwrap_or_default();
        if results.len() < statements.len() {
            return Err(format!("libSQL server answered {} of {} statements: {answer}", results.len(), statements.len()));
        }
        if let Some(refused) = results.iter().find(|r| r["type"] != "ok") {
            return Err(format!("libSQL server: {refused}"));
        }
        Ok((results, answer["baton"].as_str().map(String::from)))
    }

    /// rqlite: statements as one request to `path`.
    async fn rqlite(http: &mut Http, path: &str, statements: &[Statement<'_>]) -> Result<Value, String> {
        let answer = http.post(path, &rqlite_statements(statements)).await?;
        let results = answer["results"].as_array().ok_or_else(|| format!("rqlite: {answer}"))?;
        if let Some(refused) = results.iter().find(|r| r.get("error").is_some()) {
            return Err(format!("rqlite: {refused}"));
        }
        Ok(answer)
    }

    /// Runs statements as one transaction.
    async fn execute(&self, http: &mut Http, statements: &[Statement<'_>]) -> Result<(), String> {
        match self.kind {
            Kind::Libsql => {
                let mut all: Vec<Statement> = vec![("BEGIN", vec![])];
                all.extend(statements.iter().cloned());
                all.push(("COMMIT", vec![]));
                Remote::pipeline(http, None, &all, true).await.map(|_| ())
            }
            Kind::Rqlite => Remote::rqlite(http, "/db/execute?transaction", statements).await.map(|_| ()),
        }
    }

    /// Creates the tables and loads the users, the sessions and the
    /// accounts: the same rows as every other target gets.
    pub async fn load(&self) -> Result<(), String> {
        let http = &mut *self.conns[0].lock().await;
        self.execute(http, &SCHEMA.map(|sql| (sql, vec![]))).await?;
        let users: Vec<Statement> = (0..USERS).map(|u| ("INSERT INTO users (user_id, username) VALUES (?, ?)", vec![json!(hex(&user_id(u).0)), json!(format!("user{u}"))])).collect();
        self.execute(http, &users).await?;
        // A transaction per 500 sessions: as 20,000 commits of their
        // own the load takes minutes, and left libSQL's server taking no
        // further writes when it was tried.
        for chunk in 0..SESSIONS / 500 {
            let sessions: Vec<Statement> = (chunk * 500..(chunk + 1) * 500).map(|n| ("INSERT INTO sessions VALUES (?, ?, ?, ?)", vec![json!(format!("ses_{n:08}")), json!(hex(&user_id(n % USERS).0)), json!(hex(&token(n))), json!(FAR)])).collect();
            self.execute(http, &sessions).await?;
        }
        let accounts: Vec<Statement> = (0..1000).map(|id| ("INSERT INTO accounts VALUES (?, 100000)", vec![json!(id)])).collect();
        self.execute(http, &accounts).await
    }

    /// The session with this token, and its user: one row.
    pub async fn session(&self, worker: usize, token: &str) -> Result<(), String> {
        let http = &mut *self.conns[worker % self.conns.len()].lock().await;
        let statement = [(SESSION, vec![json!(token)])];
        let found = match self.kind {
            Kind::Libsql => hrana_rows(&Remote::pipeline(http, None, &statement, true).await?.0, 0).len(),
            // No leader is asked whether it still leads, as no other
            // target asks anyone before it reads.
            Kind::Rqlite => Remote::rqlite(http, "/db/query?level=none", &statement).await?["results"][0]["values"].as_array().map_or(0, Vec::len),
        };
        if found == 1 {
            Ok(())
        } else {
            Err(format!("{}: the session lookup found {found} rows, not one", self.kind.name()))
        }
    }

    /// One event, committed before the answer.
    pub async fn insert(&self, worker: usize, actor: &str, detail: &str) -> Result<(), String> {
        let http = &mut *self.conns[worker % self.conns.len()].lock().await;
        let statement = [(INSERT, vec![json!(actor), json!(detail)])];
        match self.kind {
            Kind::Libsql => Remote::pipeline(http, None, &statement, true).await.map(|_| ()),
            Kind::Rqlite => Remote::rqlite(http, "/db/execute", &statement).await.map(|_| ()),
        }
    }

    /// Moves 1.00 from one account to another and writes it down.
    pub async fn transfer(&self, worker: usize, from: i64, to: i64) -> Result<(), String> {
        let http = &mut *self.conns[worker % self.conns.len()].lock().await;
        match self.kind {
            Kind::Libsql => {
                // The reads come back before the writes are sent.
                let (results, baton) = Remote::pipeline(http, None, &[("BEGIN IMMEDIATE", vec![]), (BALANCE, vec![json!(from)]), (BALANCE, vec![json!(to)])], false).await?;
                let balance: i64 = hrana_rows(&results, 1).first().and_then(|row| row[0]["value"].as_str()?.parse().ok()).ok_or_else(|| format!("libSQL server: no balance for account {from}"))?;
                let mut writes: Vec<Statement> = Vec::new();
                if balance >= 0 {
                    writes.extend([(DEBIT, vec![json!(from)]), (CREDIT, vec![json!(to)]), (LEDGER, vec![json!(from), json!(to)])]);
                }
                writes.push(("COMMIT", vec![]));
                Remote::pipeline(http, baton.as_deref(), &writes, true).await.map(|_| ())
            }
            Kind::Rqlite => {
                let debit = format!("{DEBIT} AND balance >= 0");
                Remote::rqlite(http, "/db/execute?transaction", &[(&debit, vec![json!(from)]), (CREDIT, vec![json!(to)]), (LEDGER, vec![json!(from), json!(to)])]).await.map(|_| ())
            }
        }
    }
}

/// The program named by the variable `var`, or `name` on the `PATH`.
fn find(var: &str, name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(var) {
        return Some(PathBuf::from(path));
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|path| path.is_file())
}

/// Starts the server on an empty directory and waits until it answers.
/// `None`, with the reason said, if it is not installed or does not
/// come up.
pub async fn start(kind: Kind) -> Option<(Server, Remote)> {
    let (var, program, tag) = match kind {
        Kind::Rqlite => ("BENCH_RQLITED", "rqlited", "rqlite"),
        Kind::Libsql => ("BENCH_SQLD", "sqld", "libsql"),
    };
    let Some(binary) = find(var, program) else {
        eprintln!("{}: no `{program}` on the PATH and no {var}: skipping it", kind.name());
        return None;
    };
    let dir = bench_dir(tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let address = format!("127.0.0.1:{}", free_port());
    let cage = limits().cage(tag);
    let mut command = limits().command(cage.as_ref(), &binary);
    match kind {
        Kind::Rqlite => command.args(["-node-id", "1", "-http-addr", &address, "-raft-addr", &format!("127.0.0.1:{}", free_port())]).arg(dir.join("node")),
        Kind::Libsql => command.arg("-d").arg(dir.join("data")).args(["--http-listen-addr", &address]),
    };
    let child = match command.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("{}: cannot run {}: {e}: skipping it", kind.name(), binary.display());
            return None;
        }
    };
    let server = Server { child: Some(child), dir, as_postgres: false, cage, name: kind.name() };
    let ready = match kind {
        Kind::Rqlite => "/readyz",
        Kind::Libsql => "/health",
    };
    let mut probe = Http::new(&address, Duration::from_secs(2));
    for _ in 0..300 {
        if matches!(probe.request("GET", ready, None).await, Ok((200, _))) {
            return Some((server, Remote::new(kind, &address)));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!("{}: {} did not come up: skipping it", kind.name(), binary.display());
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipeline_is_what_hrana_expects() {
        let body = hrana_pipeline(None, &[("SELECT ?", vec![json!("ab")]), ("SELECT ?, ?", vec![json!(7), json!(-1)])], true);
        assert_eq!(
            body,
            json!({"baton": null, "requests": [
                {"type": "execute", "stmt": {"sql": "SELECT ?", "args": [{"type": "text", "value": "ab"}]}},
                {"type": "execute", "stmt": {"sql": "SELECT ?, ?", "args": [{"type": "integer", "value": "7"}, {"type": "integer", "value": "-1"}]}},
                {"type": "close"},
            ]})
        );
        // A stream kept open for the second half of a transaction.
        let kept = hrana_pipeline(Some("b1"), &[("COMMIT", vec![])], false);
        assert_eq!(kept, json!({"baton": "b1", "requests": [{"type": "execute", "stmt": {"sql": "COMMIT", "args": []}}]}));
    }

    #[test]
    fn hrana_rows_are_found_or_there_are_none() {
        let results = vec![json!({"type": "ok", "response": {"type": "execute", "result": {"cols": [], "rows": [], "affected_row_count": 0}}}), json!({"type": "ok", "response": {"type": "execute", "result": {"rows": [[{"type": "integer", "value": "100000"}]]}}})];
        assert!(hrana_rows(&results, 0).is_empty());
        assert_eq!(hrana_rows(&results, 1)[0][0]["value"], "100000");
        assert!(hrana_rows(&results, 2).is_empty());
    }

    #[test]
    fn rqlite_statements_carry_their_parameters() {
        let body = rqlite_statements(&[(DEBIT, vec![json!(3)]), (LEDGER, vec![json!(3), json!(4)])]);
        assert_eq!(body, json!([[DEBIT, 3], [LEDGER, 3, 4]]));
    }

    /// A server that gives `answers`, each a JSON body, to the requests
    /// of one connection; and a `Remote` that talks to it.
    async fn canned(kind: Kind, answers: &[Value]) -> (Remote, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = Remote::new(kind, &listener.local_addr().unwrap().to_string());
        let answers = answers.iter().map(|body| format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.to_string().len())).collect();
        (remote, tokio::spawn(crate::http::tests::serve(listener, answers)))
    }

    /// What was posted: the request line's path, and the body, which is
    /// one line of JSON after the headers.
    fn posted(request: &str) -> (&str, Value) {
        let path = request.split_whitespace().nth(1).unwrap();
        (path, serde_json::from_str(&request[request.rfind('\n').unwrap() + 1..]).unwrap())
    }

    fn ok(result: Value) -> Value {
        json!({"type": "ok", "response": {"type": "execute", "result": result}})
    }

    #[tokio::test]
    async fn a_libsql_transfer_is_two_requests_on_one_stream() {
        let balance = |n: &str| ok(json!({"cols": [{"name": "balance"}], "rows": [[{"type": "integer", "value": n}]], "affected_row_count": 0}));
        let nothing = || ok(json!({"cols": [], "rows": [], "affected_row_count": 1}));
        let (remote, server) = canned(
            Kind::Libsql,
            &[
                json!({"baton": "stream-1", "base_url": null, "results": [nothing(), balance("100000"), balance("99900")]}),
                json!({"baton": null, "base_url": null, "results": [nothing(), nothing(), nothing(), nothing(), {"type": "ok", "response": {"type": "close"}}]}),
            ],
        )
        .await;
        remote.transfer(0, 5, 7).await.unwrap();
        let seen = server.await.unwrap();
        let execute = |sql: &str, args: Value| json!({"type": "execute", "stmt": {"sql": sql, "args": args}});
        let int = |n: i64| json!({"type": "integer", "value": n.to_string()});
        assert_eq!(posted(&seen[0]), ("/v2/pipeline", json!({"baton": null, "requests": [execute("BEGIN IMMEDIATE", json!([])), execute(BALANCE, json!([int(5)])), execute(BALANCE, json!([int(7)]))]})));
        assert_eq!(
            posted(&seen[1]),
            ("/v2/pipeline", json!({"baton": "stream-1", "requests": [execute(DEBIT, json!([int(5)])), execute(CREDIT, json!([int(7)])), execute(LEDGER, json!([int(5), int(7)])), execute("COMMIT", json!([])), {"type": "close"}]}))
        );
    }

    #[tokio::test]
    async fn a_statement_the_server_refuses_fails_the_operation() {
        let refused = json!({"baton": null, "results": [{"type": "error", "error": {"message": "database is locked", "code": "SQLITE_BUSY"}}, {"type": "ok", "response": {"type": "close"}}]});
        let (remote, _server) = canned(Kind::Libsql, &[refused]).await;
        let why = remote.insert(0, "usr_1", "{}").await.unwrap_err();
        assert!(why.contains("database is locked"), "{why}");

        let (remote, _server) = canned(Kind::Rqlite, &[json!({"results": [{"error": "UNIQUE constraint failed: events.id"}]})]).await;
        let why = remote.insert(0, "usr_1", "{}").await.unwrap_err();
        assert!(why.contains("UNIQUE constraint failed"), "{why}");
    }

    #[tokio::test]
    async fn an_rqlite_transfer_is_one_request_and_a_lookup_must_find_its_row() {
        let (remote, server) = canned(Kind::Rqlite, &[json!({"results": [{"rows_affected": 1}, {"rows_affected": 1}, {"last_insert_id": 1, "rows_affected": 1}]}), json!({"results": [{"columns": ["id", "username"], "types": ["text", "text"]}]})]).await;
        remote.transfer(0, 5, 7).await.unwrap();
        // No row: rqlite then sends no `values` at all.
        let none = remote.session(0, "00ff").await.unwrap_err();
        assert!(none.contains("found 0 rows"), "{none}");
        let seen = server.await.unwrap();
        assert_eq!(posted(&seen[0]), ("/db/execute?transaction", json!([[format!("{DEBIT} AND balance >= 0"), 5], [CREDIT, 7], [LEDGER, 5, 7]])));
        assert_eq!(posted(&seen[1]), ("/db/query?level=none", json!([[SESSION, "00ff"]])));
    }
}
