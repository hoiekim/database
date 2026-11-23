# database

A small database server in Rust, built to take over from Postgres for the
four apps of the stack (auth, mail, doc, budget) and nothing else.

It has no SQL. The apps talk to it through a typed Rust client: a table,
a query and a write are Rust values, and a query names the index it reads.
That is what keeps it small: there is no parser, no planner and no
optimizer, and the server does exactly what it is asked.

```rust
use database_client::*;

let db = Client::connect_url("database://mail:secret@database:5454").await?;

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

// Several statements as one: nothing else writes while this runs.
db.transaction(|tx| async move {
    let unread = tx.query_one(Query::table("mails").index("mails_user_date", KeyRange::eq([user_id]))
        .select("n", count().filter(col("read").eq(false)))).await?.get::<i64>("n")?;
    tx.update(Update::table("users").key([user_id]).set("unread", unread)).await?;
    Ok::<_, DbError>(())
}).await?;
```

## What it does

- **Everything the four apps ask of Postgres.** Every statement each app
  issues was read and listed (654 of them, in `docs/requirements/`), boiled
  down to 70 requirements (`docs/REQUIREMENTS.md`), and each requirement
  is covered by tests named after it. Tables with defaults, checks and
  foreign keys; unique, partial, expression and multi-valued indexes;
  joins, subqueries and aggregates; upserts and compare-and-swap; JSON;
  exact decimals; trees; full-text search with ranking and excerpts.
- **ACID transactions.** A commit is on disk before the client hears of
  it; a crash at any moment loses no acknowledged commit and keeps no
  part of any other. Readers see a snapshot and never wait; one writer
  runs at a time per database, so there are no row locks, no deadlocks
  and nothing to retry.
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

`bench/` runs the apps' hot paths against this server and against
Postgres 16 on the same data: 100,000 mails of 4 kB with a full-text
index, 20,000 sessions. Both servers sync every commit to disk, both get
16 connections, Postgres gets prepared statements and the stack's
`shared_buffers=64MB`, this server its default 32 MB cache. One run on a
4-core machine, client and server sharing the cores:

| Workload (operations per second, median latency) | this | | Postgres 16 | | this ÷ Postgres |
|---|---:|---:|---:|---:|---:|
| bulk load: 100,000 mails of 4 kB, 250 per statement, 4 connections (statements) | 6 | 688.5 ms | 14 | 250.1 ms | 0.4× |
| session lookup with user (1 connection) | 16,052 | 55 µs | 18,036 | 51 µs | 0.89× |
| session lookup with user (16 connections) | 66,865 | 196 µs | 79,483 | 169 µs | 0.84× |
| durable insert (1 connection) | 4,796 | 182 µs | 4,368 | 169 µs | 1.10× |
| durable insert (16 connections) | 17,320 | 786 µs | 20,892 | 684 µs | 0.83× |
| rate-limit upsert, durable (8 connections) | 12,467 | 549 µs | 10,075 | 648 µs | 1.24× |
| rate-limit upsert, unlogged (8 connections) | 23,887 | 320 µs | 44,036 | 152 µs | 0.54× |
| mailbox page: newest 50 of 5,000 (4 connections) | 6,499 | 514 µs | 11,656 | 291 µs | 0.56× |
| flag update by key (4 connections) | 6,329 | 507 µs | 4,205 | 550 µs | 1.51× |
| transfer transaction: 2 reads, 3 writes (8 connections) | 1,567 | 4900 µs | 5,298 | 1361 µs | 0.30× |
| count unread of a user's 5,000 mails (4 connections) | 365 | 10.3 ms | 156 | 25.1 ms | 2.34× |
| full-text search, top 20 (4 connections) | 5,267 | 660 µs | 1,978 | 1831 µs | 2.66× |

| | this | Postgres 16 |
|---|---:|---:|
| Memory when idle, before any request | 3.3 MB | 46.2 MB |
| Memory after the run | 89.2 MB | 153.0 MB |
| CPU time for the whole run | 92.8 s | 121.0 s |
| Files on disk after a checkpoint | 633.2 MB | 2173.0 MB |

Faster where it reads many rows, searches text or updates in place;
within a fifth of Postgres either way on point lookups and durable
writes; and well behind in four places, each for a reason that is part
of the design:

- **Transactions of several requests** (the transfer) hold the
  database's one write turn from `begin` to `commit`, round trips
  included, where Postgres locks two rows and lets eight run at once.
- **Unlogged upserts** from eight connections likewise queue for the
  one turn; there is no disk to wait for, so the queue is all there is.
- **The mailbox page** reads 200 characters of each mail's body, which
  here lives outside the row: a second lookup per mail, on a page that
  is rarely in a 32 MB cache.
- **The bulk load** is one writer keeping a full-text index current at
  every commit.

For a sense of scale, the slowest of the four is still 1,500
multi-request transactions a second. The numbers move by a tenth or so
from run to run; memory after the run by more, between 60 and 105 MB,
depending on when the allocator last returned what was freed.

Run it yourself with `cargo build --release && ./target/release/bench`
(`--only ours` to skip Postgres).

## Layout

```text
protocol/   values, expressions, queries, the wire format
client/     the client the apps depend on (`database-client`)
engine/     pages, trees, the log, transactions, query execution
server/     the server and the `database` command
bench/      the benchmark above
docs/       REQUIREMENTS.md   what the apps need, with sources
            CLIENT.md         the client's contract, and SQL-to-API
            DESIGN.md         how it works and why
            OPERATIONS.md     running it: settings, backup, restore
```

## Building and testing

```sh
cargo build --release          # target/release/database
cargo test                     # everything: minutes to build, seconds to run
CRASH_SEEDS=2000 cargo test -p database-engine --test recovery   # a longer hunt
```

The tests, by what they prove:

| | |
|---|---|
| `server/tests/app_{auth,mail,doc,budget}.rs` | each app's statements, against a real server over TCP (236 tests) |
| `server/tests/acid.rs` | atomicity, isolation, once-only races, savepoints, lock and statement timeouts |
| `server/tests/durability.rs` | a server killed with `SIGKILL` under load, six times over, loses nothing acknowledged; memory stays bounded with data many times the cache |
| `server/tests/operations.rs` | dump under load, restore, a damaged dump refused, passwords, restarts |
| `engine/tests/recovery.rs` | a random workload on a simulated disk that loses power at any write, keeping, dropping or tearing each unsynced write; after every cut the database equals a model of what was committed |

## Running

```sh
DATABASE_ADMIN_PASSWORD=... DATABASE_DATA=./data ./target/release/database serve
DATABASE_ADMIN_PASSWORD=... DATABASE_PASSWORD=... ./target/release/database create mail
```

`docs/OPERATIONS.md` has the rest: the container image, the stack's
service entry, backup and restore, memory.
