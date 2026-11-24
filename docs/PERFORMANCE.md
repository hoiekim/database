# Performance against Postgres

What was measured, where this server is ahead of Postgres and where it
is behind, why, and what could be done about each gap that remains. The
numbers are from `bench/`; the explanations come from tracing and
profiling the server while the benchmark ran, and say so where they are
inference rather than measurement.

## The run

`bench/` starts each server as its own process on an empty directory,
loads the same data, and runs the same workloads through each server's
Rust client: this one's, and `tokio-postgres` with prepared statements.

- **Data:** 100,000 mails of 4 kB for 20 users, with a full-text index
  and an index by user and date; 20,000 sessions; 1,000 accounts.
- **Both servers** sync every commit to disk and get 16 connections.
- **Postgres 16** runs with `shared_buffers=64MB`, as a small deployment
  would, and otherwise its defaults. **This server** runs with its
  default 32 MB page cache.
- **Machine:** 4 cores, with the benchmark client on the same cores as
  the server.
- **Runs differ.** On this machine a workload's rate moves by a tenth
  from run to run, and now and then by more. The table is one run of
  both servers; three runs of this server and four of Postgres were
  made, and the last column gives the ratio of their medians.

| Workload | this, per second | median | 99th | Postgres 16, per second | median | 99th | this ÷ Postgres | of medians |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| bulk load: 100,000 mails of 4 kB, 250 per statement (4 connections), statements | 10.3 | 360.2 ms | 1140.6 ms | 14.1 | 238.4 ms | 756.3 ms | 0.73× | 0.76× |
| session lookup with user (1 connection) | 15,383 | 55 µs | 214 µs | 18,072 | 51 µs | 92 µs | 0.85× | 0.88× |
| session lookup with user (16 connections) | 76,622 | 168 µs | 660 µs | 72,442 | 187 µs | 711 µs | 1.06× | 0.96× |
| durable insert (1 connection) | 4,426 | 187 µs | 650 µs | 4,741 | 182 µs | 505 µs | 0.93× | 0.95× |
| durable insert (16 connections) | 18,945 | 676 µs | 4059 µs | 22,154 | 645 µs | 2282 µs | 0.86× | 0.89× |
| rate-limit upsert, durable (8 connections) | 16,477 | 432 µs | 1646 µs | 17,019 | 436 µs | 1207 µs | 0.97× | 1.10× |
| rate-limit upsert, unlogged (8 connections) | 43,249 | 167 µs | 503 µs | 53,981 | 125 µs | 531 µs | 0.80× | 0.75× |
| mailbox page: newest 50 of 5,000 (4 connections) | 8,523 | 399 µs | 1329 µs | 12,154 | 291 µs | 798 µs | 0.70× | 0.69× |
| flag update by key (4 connections) | 9,457 | 358 µs | 2520 µs | 3,734 | 596 µs | 4317 µs | 2.53× | 2.34× |
| transfer transaction: 2 reads, 3 writes (8 connections) | 6,314 | 1190 µs | 2778 µs | 6,970 | 1084 µs | 2774 µs | 0.91× | 0.95× |
| transfer transaction, all into one account (8 connections) | 1,212 | 6350 µs | 12.5 ms | 1,234 | 5390 µs | 22.7 ms | 0.98× | 0.99× |
| count unread of a user's 5,000 mails (4 connections) | 478 | 7774 µs | 17.2 ms | 155 | 24.5 ms | 46.7 ms | 3.08× | 3.09× |
| full-text search, top 20 (4 connections) | 5,750 | 583 µs | 2095 µs | 2,291 | 1676 µs | 3273 µs | 2.51× | 3.24× |

| | this | Postgres 16 |
|---|---:|---:|
| Memory when idle, before any request | 3.5 MB | 46.0 MB |
| Memory after the run | 45.9 MB | 152.3 MB |
| Most memory held during the run (the bulk load) | 131 to 150 MB | not measured |
| CPU time for the whole run | 58.9 s | 120.6 s |
| Data files on disk after a checkpoint | 640.8 MB | 1,142.3 MB |
| Log files on disk after a checkpoint | 0 MB | 1,024.0 MB |

Three things to hold against these figures:

- The full-text index here is kept per user (`on(["user_id"])`), which
  the search workload uses. The Postgres index is one GIN index over
  all users, the usual arrangement there; a search then filters by user
  afterwards. Part of the search result is that difference in schema.
- Postgres stores a search vector with word positions for each mail
  (6.4 kB a mail in this data), which this server does not keep. That is
  two thirds of its data files.
- Postgres keeps the log segments it has used, to write over them again:
  1,024 MB in this run. This server deletes its log at a checkpoint. The
  table counts data and log apart.

## What changed since the first measurements

The first version of this document found four workloads well behind
Postgres, each a consequence of the design. Each was traced again, and
the design changed where the trace said to.

| Workload | this ÷ Postgres, then | now |
|---|---:|---:|
| Transfer transaction (8 connections) | 0.32× | 0.91× |
| Bulk load of 100,000 mails | 0.39× | 0.73× |
| Unlogged upsert (8 connections) | 0.57× | 0.80× |
| Mailbox page, with a preview of each body | 0.67× | 0.70× (0.8× on a server that has only read) |
| Durable insert (1 and 16 connections) | 0.82×, 0.88× | 0.93×, 0.86× |
| Count over 5,000 rows | 2.38× | 3.08× |
| Full-text search | 2.28× | 2.51× |

Two changes to the benchmark itself belong beside those figures. The
transfer workload picked its accounts with a number that repeats in a
pattern, so that every third transfer debited the same account; it now
picks them well mixed, as it was meant to, and a second transfer
workload makes every transaction contend for one account on purpose.
Both servers ran the old pattern and both run the new ones.

### A transaction of several requests: 0.32× to 0.91×

*What was wrong.* A transaction held the database's one write turn from
`begin` to `commit`, round trips included. Sampled during the workload,
the server was idle in nine thread-samples of ten: one transaction at a
time was waiting for its client.

*What changed.* `Client::transaction` now runs its work as an optimistic
transaction: on a private copy of the database, without the turn. At the
commit it takes the turn. If nothing was committed meanwhile, the copy
becomes the next version as it stands; if something was, the server runs
the transaction's requests again on the newest version and commits if
each answers as it did (`DESIGN.md`, "Transactions"). With eight
connections nearly every commit is of the second kind: in one run 5,976
of 6,000 transfers were run again by the server and committed, 17
committed as they stood, and 147 commits conflicted, their transactions
run again by the client.

*What is left.* A transfer here is seven round trips (begin, two reads,
three writes, commit) to Postgres' six, and its five statements run
twice on the server.

### Transactions that contend for one row: 0.98×

Eight connections all crediting one account are in each other's way in
any database. Postgres makes each wait for the row's lock. Here each
runs, finds at its commit that the row it read has changed, and is run
again by the client: twice beside the others, then as the only writer.
In one run of 6,000 such transactions 11,992 commits conflicted: nearly
every transaction ran twice for nothing and committed on its third run.
The two servers end level, each limited by one transaction at a time
holding that row: here through its round trips as the only writer, in
Postgres through its commit's sync. What this server wastes is the CPU
and the round trips of the runs that conflict.

### Unlogged upserts from eight connections: 0.57× to 0.80×

*What was wrong.* Two things, found one after the other. Writers took
the turn through an asynchronous lock, so each statement waited for the
task before it to be woken and scheduled: the server ran at about a
core and a half of four. And once that was gone, the statement itself
was the limit: 18 µs of work under the turn on an idle server, 22 µs
with eight connections, and 27 µs with its begin and commit, which
alone caps the rate at 37,000 a second.

*What changed.* A database's turn is now kept by a gate that runs
waiting small writes back to back on the thread that holds it
(`DESIGN.md`, "The server"); a write that arrives behind another waits
a few microseconds on its own thread rather than be put to sleep. A
commit that put nothing in the log is visible at once, without a round
through the log's waiters. And the statement does less: a change to one
entry of a tree page moves bytes within the page instead of rebuilding
the page, an upsert reads its row once instead of five times and builds
it once instead of four, and the maps of pages and of column names use
hashes fit for their keys. By the profiler's count an upsert went from
73,000 instructions to 47,000; by the clock from 18 µs to 11 µs.

*What is left.* 17 µs under the turn per upsert with eight connections,
of which 13 are the statement. Postgres runs eight of them at once on
four cores.

### The mailbox page: 0.67× to 0.70×, or 0.8×

*What was wrong.* Not the disk: the pages the workload reads fit in the
cache. Every query passes through the same few pages at the top of each
tree, and looking a page up took a lock: four threads spent 12% of
their samples blocked on the cache's locks. The first 200 characters of
each body were gathered one character at a time, a fifth of the
query's instructions.

*What changed.* A lookup in the cache only reads its shard; a query
keeps the upper pages it has met at hand; a point lookup, and the start
of a large value, are read in the page where they lie; a reader finds
out without a lock whether a page is among those not yet written, and
whether freed pages can be reused; and a prefix of a text is cut by
position.

*What is left.* On a server that has only read, the workload now runs
at about 10,000 a second (9,200 to 10,900 over eight runs), 0.8× of
Postgres. In the benchmark's sequence it follows the upserts and runs
at 8,500. The difference was traced to the allocator: after a burst of
writes from many connections, threads were sampled waiting for its
locks, in the allocations a row costs (about ten: its key, its bytes,
its values). With that gone, what separates the two is the second
lookup for each preview and those allocations themselves.

## Where it is ahead, and why

- **Counting 5,000 rows of a user** (3.1×) and **updating a flag**
  (2.5×). Postgres compresses a 4 kB body to 1.7 kB and then keeps it
  inside the row, so its rows are 2 kB each: counting a user's unread
  mails walks 10 MB of rows, and setting a flag writes a new 2 kB
  version of the row and logs it. Here the body lives outside the row,
  which is then about 200 bytes. (The same choice is what costs the
  mailbox page its second lookup.)
- **Full-text search** (2.5×). The index is per user, and it holds each
  row's score, so ranking needs nothing but the index. Postgres ranks
  from the stored search vector, which it must fetch for each candidate.
- **Memory, CPU and disk.** One process with a fixed page cache instead
  of a process per connection; no second copy of the text as a search
  vector. The whole run took half of Postgres' CPU time.

## Where it is still behind, and the options

### 1. The mailbox page (0.70×)

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Keep the start of a large text in its row**, as a setting of the column (say the first 256 bytes) | `left` and a short `substr` need no second lookup, which is a fifth of the query's instructions now | About a fifth | Moderate: the stored form of a row, the schema and the dump gain a field; rows grow by the prefix, which costs the count and the flag update part of their lead |
| **Fewer allocations per row**: read a row's bytes from its page instead of copying them, decode into one buffer | Less work in every read, and less for threads to contend over in the allocator | The 15% lost after writes, and some more | Moderate: the row's lifetime runs through the executor |
| **An allocator with larger per-thread caches** | The same contention, without touching the code | The 15% lost after writes | A dependency, where there are three; or a setting of the C library's, which only a container image can make |
| **A preview column** kept by the application | No large value is read | A fifth, with nothing to build | None in the server; a column to maintain |

### 2. Unlogged upserts from many connections (0.80×)

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Keep volatile tables in memory only**: they are empty after a restart anyway | No page copies and no version per write; such a table could take a lock of its own and be written beside the database's writer | Probably past Postgres for this workload | Moderate; such a table would no longer roll back with a transaction or be read from a snapshot, which has to be acceptable for what is kept in it |
| **A prepared form of a request**: sent once, then named with its values | The statement is not decoded and its tree of expressions not built for each call | A few microseconds of the 13; helps every small request | Moderate: a new request type and a cache per connection |
| **One version for a batch of jobs**: the thread that runs waiting writes back to back begins and commits once for all of them | About 4 µs of the 17 | About a fifth at eight connections, nothing at one | Small to moderate: a failed statement must undo only itself |

### 3. The bulk load (0.73×)

Building and merging full-text segments was two thirds of the single
writer's time, most of it small allocations: a copy of every word of
every pending row, a list per word, a buffer of a kilobyte per word. A
segment is now written by merging the rows' stored words as they lie,
segments whose rows are all alive are merged by joining their lists as
stored, and the tree builder fills pages in place. The load went from 6
statements a second to 10.

Since then, three things were done:

- **Text is cut into words before the turn**, on the connection's own
  thread (`session/load.rs`), into terms sorted and packed in one
  buffer, and a row's words are kept in the form the index stores them.
  The words are used only for a row whose text is the very text they
  were cut from.
- **Segments are merged beside the writer**, by a thread of the
  engine's, and put in place under the write lock only if the segments
  read are as they were. A writer still merges a mostly dead segment,
  and the cheapest run once 40 segments have gathered.
- **Large requests share 4 MB of memory**, and what each held is given
  back to the system as it is answered (`session/budget.rs`).

Counted by `valgrind --tool=callgrind` over a tenth of the load
(`BENCH_LOAD_PART=10`: 40 statements of 250 mails), in instructions of
the whole server, and of the statements' work under the turn to write:

| | whole server | under the turn |
|---|---:|---:|
| Before | 31.7 billion | 30.7 billion |
| Words cut before the turn | 24.7 billion | 18.2 billion |
| And segments merged beside the writer | 22.7 billion | 11.3 billion |
| And the memory budget | 22.5 billion | 10.6 billion |

A row's words cost 12.7 billion before, counted in a table of terms,
and 6.7 billion now, sorted and packed; 5.4 billion of that is spent
beside the turn. Merging beside the writer took 5.0 to 5.5 billion out
of the turn. Under `valgrind` the threads of the process run one at a
time, so the merging thread falls behind, and writers still merged 3.5
billion themselves past 40 segments; with the threads on cores of their
own it should fall behind less (inference, not measured). So the
writer's work for a statement went from 0.77 to 0.26 billion
instructions.

The most memory the server held during the whole bulk load (`VmHWM`,
with the default 32 MB cache): 150 to 171 MB before (six runs), 177 to
218 MB with words cut before the turn (three runs: four connections cut
at once, and hold what they cut), 153 to 190 MB with segments merged
beside as well (six runs), and 112 to 125 MB with the budget (six
runs). With the budget and a trim once a second instead of five times,
it was 144 to 159 MB (three runs).

On this machine, shared with other work, the load ran at a median of
7.3 statements a second before and 9.1 after, in runs that moved
between 5.6 and 8.9 before and 5.5 and 14.0 after; those rates say
little. The server's CPU time for the load was 47 to 56 s before and 43
to 55 s after; the trims take part of what was saved (40 to 46 s
without the budget).

Merging beside the writer has a price on disk: after the load the data
file holds 750 to 800 MB, where it held 624 to 655 MB. A merge holds the
version it reads for as long as it runs, as a reader would, and no page
freed meanwhile is given out again until it ends, so writers take new
pages from the end of the file instead. That is the likely reason; it
was not traced.

| What is left | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Build indexes after loading**, for `restore`: load the rows, then write each index bottom-up from sorted entries with the tree `Builder` | A restore pays for neither pending rows nor merges | Several times faster restores; nothing for ordinary inserts | Moderate; not begun |
| **Cut words ahead inside transactions** | Only a large insert that is its own transaction is cut ahead now | The same for loads made in transactions | Small |
| **Hold only what a merge reads**: keep the pages of the segments being merged from being given out again, not every page freed meanwhile | Writers reuse freed pages while a merge runs | The fifth the data file grew | Moderate: the store keeps whole versions, not sets of pages |
| **Trim less, at less cost**: return only the pools the large requests used | The CPU time the trims take | A few seconds of the load's CPU | Small; the C library trims all pools at once |

### 4. Transactions of several requests (0.91×)

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Send `begin` with the first statement** | Six round trips instead of seven | About a seventh | Small; the server must refuse the statement if the begin failed |
| **A batch request**: a transaction whose statements do not depend on each other's answers, sent whole | One round trip, and nothing to run twice | Several times, for transactions that fit | Moderate: a new request type |
| **Go straight to the write lock after one conflict**, where conflicts are the rule | A contended transaction runs twice, not three times | A third of the round trips in the contended workload; a few percent lost where conflicts are rare, since each then holds every writer up once | Small: a setting of the client's |
| **Wait instead of running again**, for rows that have just conflicted: a transaction that is about to read such a row waits for the one that holds it | Contended transactions queue for the row, as in Postgres, without holding up the writers of other rows | The runs that conflict; and transactions on other rows no longer wait behind the contended ones | Large: this is row locking, with its waits and its deadlocks to time out |

### 5. Point lookups and durable inserts (0.85× to 1.06×)

Within a tenth, sometimes ahead. A lookup's cost is mostly outside the
engine: decoding the request into a query value and encoding the rows.
The prepared form under 2 would cut the first. A durable insert on one
connection is the disk's sync and little else on both servers.

## Running it

```sh
cargo build --release
./target/release/bench                 # both servers, if Postgres is installed
./target/release/bench --only ours
BENCH_DETAILS=1 ./target/release/bench                   # also: each server's own counts
BENCH_WORKLOADS="transfer,upsert" ./target/release/bench # only the workloads named so
BENCH_REPEAT="mailbox:30" BENCH_WORKLOADS=mailbox ./target/release/bench --only ours   # one workload, longer, to profile
BENCH_KEEP=1 BENCH_WORKLOADS=mailbox ./target/release/bench --only ours   # keep this server's loaded data for the next run
BENCH_CONNECTIONS=1 BENCH_WORKLOADS=upsert ./target/release/bench --only ours
BENCH_SERVER_LOG=server.log ./target/release/bench --only ours
```

`database stats` (and `BENCH_DETAILS=1`) says how the commits of
optimistic transactions went: as they stood, run again, or in conflict.
