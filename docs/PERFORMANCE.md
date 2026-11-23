# Performance against Postgres

What was measured, where this server is ahead of Postgres and where it
is behind, why, and what could be done about each gap. The numbers are
from `bench/`; the explanations come from tracing and profiling the
server while the benchmark ran, and say so where they are inference
rather than measurement.

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
  the server. One run; figures move by about a tenth between runs.

| Workload | this, per second | median | 99th | Postgres 16, per second | median | 99th | this ÷ Postgres |
|---|---:|---:|---:|---:|---:|---:|---:|
| bulk load: 100,000 mails of 4 kB, 250 per statement (4 connections), statements | 6 | 664.8 ms | 1572.6 ms | 14 | 257.0 ms | 734.4 ms | 0.39× |
| session lookup with user (1 connection) | 15,370 | 60 µs | 140 µs | 17,222 | 52 µs | 119 µs | 0.89× |
| session lookup with user (16 connections) | 69,886 | 189 µs | 725 µs | 76,854 | 181 µs | 683 µs | 0.91× |
| durable insert (1 connection) | 3,964 | 197 µs | 910 µs | 4,852 | 181 µs | 482 µs | 0.82× |
| durable insert (16 connections) | 19,253 | 756 µs | 2022 µs | 21,887 | 656 µs | 2511 µs | 0.88× |
| rate-limit upsert, durable (8 connections) | 12,964 | 535 µs | 1584 µs | 12,161 | 536 µs | 2408 µs | 1.07× |
| rate-limit upsert, unlogged (8 connections) | 25,474 | 309 µs | 576 µs | 44,900 | 155 µs | 564 µs | 0.57× |
| mailbox page: newest 50 of 5,000 (4 connections) | 7,115 | 495 µs | 1518 µs | 10,575 | 336 µs | 864 µs | 0.67× |
| flag update by key (4 connections) | 7,858 | 425 µs | 2660 µs | 3,813 | 549 µs | 2498 µs | 2.06× |
| transfer transaction: 2 reads, 3 writes (8 connections) | 1,608 | 4723 µs | 10.6 ms | 5,005 | 1429 µs | 4106 µs | 0.32× |
| count unread of a user's 5,000 mails (4 connections) | 379 | 10.1 ms | 18.7 ms | 159 | 23.0 ms | 93.2 ms | 2.38× |
| full-text search, top 20 (4 connections) | 5,053 | 697 µs | 1957 µs | 2,216 | 1723 µs | 3629 µs | 2.28× |

| | this | Postgres 16 |
|---|---:|---:|
| Memory when idle, before any request | 3.3 MB | 46.3 MB |
| Memory after the run | 99.4 MB | 155.0 MB |
| CPU time for the whole run | 90.0 s | 120.3 s |
| Data files on disk after a checkpoint | 625.6 MB | 1,152.4 MB |
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

## Where it is ahead, and why

- **Counting 5,000 rows of a user** (2.4×) and **updating a flag**
  (2.1×). Postgres compresses a 4 kB body to 1.7 kB and then keeps it
  inside the row, so its rows are 2 kB each: counting a user's unread
  mails walks 10 MB of rows, and setting a flag writes a new 2 kB
  version of the row and logs it. Here the body lives outside the row,
  which is then about 200 bytes. (The same choice is what loses the
  mailbox page, below.)
- **Full-text search** (2.3×). The index is per user, and it holds each
  row's score, so ranking needs nothing but the index. Postgres ranks
  from the stored search vector, which it must fetch for each candidate.
- **Memory, CPU and disk.** One process with a fixed page cache instead
  of a process per connection; no second copy of the text as a search
  vector.

## Where it is behind

### 1. A transaction of several requests (0.32×)

*The workload:* eight connections each run `begin`, two reads, two
updates, one insert, `commit`: seven round trips.

*What happens here.* One writer runs at a time per database, and a
transaction holds that turn from `begin` to `commit`, including the time
its requests spend on the wire and in the client. A round trip takes
about 60 µs (the median of a point lookup on one connection), so seven
of them hold the turn for some 420 µs, and the commit's sync adds 150 or
so. That predicts about 1,750 transactions a second however many
connections ask; 1,610 were measured.

*What Postgres does.* It locks the two account rows and nothing else, so
the eight transactions overlap; only their log syncs are shared.

*Options:*

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **A batch request**: the whole transaction sent as one request, a list of statements with guards (`require(expr)`, values carried from one statement to the next) | The turn is held for the time the statements take to run, not for seven round trips | The rate of single-statement durable writes, which is 13,000 a second on eight connections here; also one round trip of latency instead of seven | A new request type and client API; logic must be expressible as guards. Moderate, and the most gain for the work |
| **One conditional statement** where it will do: `Update .. filter(balance >= amount)`, an upsert with `update_where` | The same, with what exists today | As above for the cases that fit | None in the server |
| **Pipelining** in the client: send the statements whose results decide nothing (here the three writes and the commit) without waiting for each answer | Four round trips instead of seven | About 1.5× | Small; client only |
| **Optimistic transactions**: read on a snapshot without the turn, take the turn at commit, and check that nothing read has changed since; retry if it has | Round trips of different transactions overlap | Near Postgres when conflicts are rare; retries when they are not | Large: the engine must track what a transaction read, and `transaction(..)` must be safe to run twice |
| **A turn per table** instead of per database | Writers to different tables run together | Nothing for this workload (all eight write the same tables); much for mixed ones | Large: it gives up "nothing changes under a writer" |

### 2. Unlogged upserts from eight connections (0.57×)

*The workload:* a rate-limit counter in a table that need not survive a
crash, bumped by eight connections.

*What happens here.* With no disk to wait for, what is left is the queue
for the turn and the fixed cost of a write: 39 µs a statement, end to
end. Each statement wakes the next connection in the queue (a task woken
on another thread), copies the path of pages from the tree's root to the
changed leaf (a changed page is rebuilt whole, 8 KiB at a time), and
publishes a new version of the database. This workload was not profiled,
so the split between those three is not known.

*What Postgres does.* Eight processes update eight rows in shared memory
under row locks, in parallel on four cores, and an unlogged table writes
no log.

*Options:*

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Run queued writes back to back**: the thread that holds the turn runs the waiting statements of other connections itself, instead of waking each in turn | One wake-up per batch instead of per statement; one version published per batch | To be measured; it removes the only cost that grows with the number of connections. Durable writes on many connections gain too | Moderate, in the server's request path |
| **Change pages in place** when the writer made them in this transaction, instead of rebuilding the page | Less copying for every write | Helps all writes and the bulk load; size unknown until profiled | Moderate, inside the tree code, which is the most tested part |
| **Keep volatile tables in memory only**: they are empty after a restart anyway | No page copies, no version to publish; could take a lock of their own and run beside the writer | Probably the largest for this workload | Moderate; such a table would no longer roll back with a transaction or be read from a snapshot, which has to be acceptable for what is kept in it |

### 3. The mailbox page (0.67×)

*The workload:* the newest 50 of a user's 5,000 mails: six columns and
the first 200 characters of the body.

*What happens here.* A body of 4 kB does not fit in its row, so it lives
in a tree of large values, and each preview is a second lookup, on a
page that is rarely in a 32 MB cache when the bodies are 400 MB: a read
from the file, a checksum, an allocation. Sampled while the workload
ran, the server spent 28% of its time reading those previews, 26%
fetching the 50 rows, and 18% sending the answer.

*What Postgres does.* It compresses the body (4,004 bytes to 1,730 in
this data) and, since it then fits, keeps it in the row. The preview is
cut from a row it has already fetched.

*Options:*

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **A preview column** kept by the application | No large value is read | Removes the 28%; nothing to build | None in the server; a column to maintain |
| **Keep the start of a large text in its row** (a per-column setting, say the first 256 bytes) | `left` and short `substr` need no second lookup | Removes the 28% for previews | Small to moderate; rows grow by the prefix, which costs the scans that are fast today |
| **Compress large values** | A 4 kB text that compresses to under 2 kB stays in its row, as in Postgres; less disk and more rows per cached page | Removes the second lookup for most mails; but rows grow to 2 kB, and the count and flag-update results above would move towards Postgres' | Moderate: a compressor (written here, or a dependency), and ranges of a compressed value are dearer to read |
| **Cheaper row reads**: borrow a row's bytes from its page instead of copying them, one allocation per row instead of four, per-shard cache counters, no lock on the dirty-page map when it is empty | Less work per row on every read path | Perhaps a tenth to a fifth on reads; each is small | Small each |
| **A larger cache** (`DATABASE_CACHE_MB=64`, what Postgres was given) | Fewer reads from the file | Little here: the bodies are six times that | None |

### 4. The bulk load (about 0.4×)

*The workload:* 100,000 mails inserted 250 to a statement from four
connections.

*What happens here.* One writer does everything. Sampled during the
load, 30% of the server's time went to merging full-text segments, 24%
to building new segments from pending rows, and 22% to cutting text
into words; the rest to the row and index trees, the log and the
network. All of it is on one thread, while the other cores have only
the benchmark's client to run.

*What Postgres does.* Each of the four connections cuts its own rows
into words on its own core, and its GIN index collects new entries in a
pending list that is merged later, in bulk.

*Options:*

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Cut text into words before taking the turn**, on the connection's own thread | 22% of the work leaves the single writer and spreads over the cores | Up to 1.3× | Moderate: an insert's index expressions must be evaluated before its transaction begins |
| **Merge segments in the background**: a merge only reads segments that no longer change, so it can run beside the writer and be swapped in under the turn | 30% of the work leaves the writer | Up to 1.4×; together with the first, up to 2× | Moderate to large: rows deleted during a merge must be reconciled when it is swapped in |
| **Build indexes after loading**, for `restore`: load the rows, then write each index bottom-up from sorted entries | A restore pays for neither pending rows nor merges | Several times faster restores; nothing for ordinary inserts | Moderate; the tree builder it needs exists |
| **A cheaper tokenizer**: no allocation per word | Less of the 22% | Small | Small |
| **Change pages in place** (as in 2) | Less copying in the row and index trees | Unknown until profiled | Moderate |

### 5. The small gaps

Point lookups (0.89–0.91×) and durable inserts (0.82–0.88×) are within
reach of ordinary tuning:

- A lookup's cost is now mostly outside the engine: decoding the request
  into a query value, encoding the rows, two reads from the socket where
  one buffered read would do. A prepared form (send the query once, then
  a handle and its parameters) would cut the first; the row-read changes
  under 3 cut the rest.
- Sixteen connections inserting queue for the turn like the upserts in
  2, and "run queued writes back to back" applies as it stands. On one
  connection there is no queue: what is left over Postgres' 181 µs is
  the fixed cost of a write here (copying the pages on the path to the
  changed leaf, publishing a version), which "change pages in place"
  addresses.

## What has already been done this way

The same method, trace first and then change, produced the current
figures:

- **Requests no longer change threads.** Every request used to be handed
  to a thread of its own; a trace showed three wake-ups around a 45 µs
  disk sync. Reads are now tried in place, writers queue without a
  thread, and a commit waits for the disk without one. On one
  connection, point lookups went from 12,800 a second to about 16,000
  and durable inserts from 2,200 to 3,000.
- **The log is grown ahead of its records.** Each commit lengthened the
  log file, and syncing a file whose length changed took four times as
  long as syncing its bytes. Durable inserts on one connection went from
  3,000 a second to between 4,000 and 4,800.
- **Full-text indexes are kept in segments.** An entry per word per row
  made the same load take 7 GB and about ten times as long.
- **The allocator is told to give memory back.** After the run the
  process held 230 MB with 60 in use; it now holds between 60 and 105.

## Running it

```sh
cargo build --release
./target/release/bench                 # both servers, if Postgres is installed
./target/release/bench --only ours
BENCH_DETAILS=1 ./target/release/bench --only postgres   # also: how Postgres stored the mails
BENCH_REPEAT="mailbox:30" ./target/release/bench --only ours   # one workload, longer, to profile
```
