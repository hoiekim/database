# database

A small database server in Rust, for applications that would otherwise
run a Postgres far larger than they need.

It has no SQL. An application talks to it through a typed client: a
table, a query and a write are values the application builds, and a
query names the index it reads. That is what keeps it small: there is no
parser, no planner and no optimizer, and the server does exactly what it
is asked. The client below is the Rust one; `clients/` has the same
vocabulary for TypeScript and for Python, and `docs/PROTOCOL.md` is the
wire protocol, for writing one in any other language.

```rust
use database_client::*;

let db = Client::connect_url("database://shop:secret@database:5454").await?;

db.create_table_if_not_exists(
    TableDef::new("mails")
        .column(ColumnDef::new("mail_id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("subject", Type::Text).default(""))
        .column(ColumnDef::new("date", Type::Timestamp).default_now())
        .column(ColumnDef::new("read", Type::Bool).default(false))
        .primary_key(["mail_id"])
        .index(IndexDef::new("mails_user_date").on(["user_id"]).on_desc("date").on(["mail_id"])),
).await?;

// The newest 50 mails of a user: 50 index entries read, 50 rows fetched.
let page = db.query(
    Query::table("mails").index("mails_user_date", KeyRange::eq([user_id])).limit(50),
).await?;

// Several statements as one, as if nothing else wrote while it ran.
db.transaction(|tx| async move {
    let unread = tx.query_one(Query::table("mails").index("mails_user_date", KeyRange::eq([user_id]))
        .select("n", count().filter(col("read").eq(false)))).await?.get::<i64>("n")?;
    tx.update(Update::table("users").key([user_id]).set("unread", unread)).await?;
    Ok::<_, DbError>(())
}).await?;
```

## What it does

- **What an ordinary web application asks of Postgres**, written down as
  74 requirements (`docs/REQUIREMENTS.md`), each covered by tests that
  cite it. Tables with defaults, checks and foreign keys; unique,
  partial, expression and multi-valued indexes; joins, subqueries and
  aggregates; upserts and compare-and-swap; JSON; exact decimals; trees;
  full-text search with ranking and excerpts.
- **ACID transactions.** A commit is on disk before the client hears of
  it; a crash at any moment loses no acknowledged commit and keeps no
  part of any other. Readers see a snapshot and never wait; one writer
  commits at a time per database, so there are no row locks and no
  deadlocks. A transaction runs beside other writers until it commits,
  and the client runs it again by itself if one of them changed what it
  had read.
- **Backup and restore**, in place of `pg_dumpall`: `database dump`
  streams every database from a consistent snapshot while the server
  runs; `database restore` loads one, and changes nothing unless the
  whole stream is good.
- **Small.** One process, a 3 MB binary, 3 MB of memory when idle, a
  page cache with a fixed budget, no background work when nothing
  changes. Dependencies: an async runtime, a CRC, a JSON library.

## What it does not

SQL, a planner, stored functions, views, replication, TLS, parallel
writers, regular expressions, collations. `docs/CLIENT.md` ends with the
full list and what to do instead.

## Compared with Postgres

`bench/` runs hot paths common to web applications against this server
and against Postgres 16 on the same data. Both sync every commit to
disk; Postgres gets prepared statements and `shared_buffers=64MB`, this
server its default 32 MB cache. One run on a 4-core machine:

| | this | Postgres 16 |
|---|---:|---:|
| Memory idle | 3.5 MB | 46.0 MB |
| Memory after the run | 45.9 MB | 152.3 MB |
| CPU for the whole run | 58.9 s | 120.6 s |
| Data files on disk | 641 MB | 1,142 MB |

| Workload | this ÷ Postgres |
|---|---:|
| Count over 5,000 rows | 3.08× |
| Flag update by key | 2.53× |
| Full-text search | 2.51× |
| Session lookup (16 connections) | 1.06× |
| Transaction of several requests, all on one row (8 connections) | 0.98× |
| Durable upsert (8 connections) | 0.97× |
| Durable insert (1 connection) | 0.93× |
| Transaction of several requests (8 connections) | 0.91× |
| Durable insert (16 connections) | 0.86× |
| Session lookup (1 connection) | 0.85× |
| Unlogged upsert (8 connections) | 0.80× |
| Bulk load of 100,000 mails | 0.73× |
| Mailbox page: newest 50, with a preview of each body | 0.70× |

It uses a third of the memory and half the CPU and disk, and is faster
where it reads many rows, searches text or updates in place. On point
lookups, durable writes and transactions it is within about a tenth of
Postgres, on either side from run to run. It is **behind** in three
places:

- **A page of mails with a preview of each body** makes a second lookup
  per mail, because large values live outside their rows.
- **A bulk load** is one writer keeping a full-text index current at
  every commit, where Postgres has a process per connection.
- **Unlogged upserts** from many connections are written one at a time,
  each a new version of the database.

`docs/PERFORMANCE.md` has the full table, what was traced and profiled
to explain each gap, what has been done about them so far, and the
options for what is left with what each would cost.

## Layout

```text
protocol/   values, expressions, queries, the wire format
            fixtures/vectors.json   golden vectors: messages and their bytes
client/     the client Rust applications depend on (`database-client`)
clients/    typescript/       the same client for Node, on node:net alone
            python/           and for Python, on the standard library alone
engine/     pages, trees, the log, transactions, query execution
server/     the server and the `database` command
bench/      the benchmark above
docs/       REQUIREMENTS.md   what the database has to do
            CLIENT.md         the client's contract, and SQL-to-API
            PROTOCOL.md       the wire protocol, byte for byte
            DESIGN.md         how it works and why
            OPERATIONS.md     running it: settings, backup, restore
            PERFORMANCE.md    measurements, their causes, and options
```

## Building and testing

```sh
cargo build --release          # target/release/database
cargo test                     # everything: minutes to build, seconds to run
CRASH_SEEDS=2000 cargo test -p database-engine --test recovery   # a longer hunt
FAULT_SCRIPTS=20 cargo test -p database-engine --test faults     # and another
```

The tests, by what they prove:

| | |
|---|---|
| `server/tests/scenario_*.rs` | the requirements as four kinds of application use them (an identity service, a mail store, a document workspace, a personal-finance ledger), against a real server over TCP (236 tests) |
| `server/tests/acid.rs` | atomicity, isolation, once-only races, savepoints, lock and statement timeouts |
| `server/tests/optimistic.rs` | transactions that run beside other writers: committed as they stand, run again, or in conflict; what they were told is what is stored |
| `server/tests/requests.rs` | statements the server keeps and runs with new values; a transaction sent whole as a batch, with conditions; a transaction that begins with its first statement; the Unix socket |
| `server/tests/explain.rs` | what a query read, by every way in, against what it returned; slow statements in the server's log |
| `server/tests/durability.rs` | a server killed with `SIGKILL` under load, six times over, loses nothing acknowledged; memory stays bounded with data many times the cache |
| `server/tests/operations.rs` | dump under load, restore, a damaged dump refused, passwords, restarts |
| `engine/tests/faults.rs` | a scripted workload, killed before every operation it makes on the disk, with the power cut each way that differs, and again inside the recoveries; the disk full from each operation on; each operation failing once, a sync by losing what it covered; a disk that lies about its syncs. Nothing acknowledged is lost, and what a lying disk lost is seen as far as checksums can see |
| `engine/tests/recovery.rs` | a random workload on a simulated disk that loses power at any write, keeping, dropping or tearing each unsynced write, with transactions open beside the writer; after every cut the database equals a model of what was committed and no page is lost |
| `protocol/tests/vectors.rs` | the golden vectors are what the wire format is: 105 messages, each as the bytes this crate writes, and the limits `docs/PROTOCOL.md` states |
| `clients/typescript/test`, `clients/python/tests` | each client writes those same bytes and reads them back, its builders mean what the Rust builders mean, and every kind of request works against a real server (`npm test`; `python -m unittest discover -s tests -t .`) |

## Running

```sh
DATABASE_ADMIN_PASSWORD=... DATABASE_DATA=./data ./target/release/database serve
DATABASE_ADMIN_PASSWORD=... DATABASE_PASSWORD=... ./target/release/database create shop
```

`docs/OPERATIONS.md` has the rest: the container image, backup and
restore, memory.
