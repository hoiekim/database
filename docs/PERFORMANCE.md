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

### What a request costs: prepared statements, one request for a transaction, a Unix socket

*What was wrong.* For a small request the engine is the lesser part of
the cost. The rest is the trip through the network stack, decoding the
request into a tree of expressions (and encoding it, in the client),
and for a transaction one round trip per statement plus one to begin
and one to commit.

*What changed.* Four things, none of them in the engine:

- **Prepared statements** (`Prepared`, `param(n)`, `Db::execute`). A
  connection is sent a statement once and keeps it decoded; after that
  a request is the statement's number and its values, which the
  evaluator reads from its context.
- **A transaction begins with its first statement.** `transaction`,
  `exclusive` and `read` send the begin with the first statement: a
  transfer is six round trips, as in Postgres, where it was seven.
- **A batch** (`Client::batch`) sends a transaction whole when its
  statements need no answers from each other. It is one round trip, and
  it runs once, as a job that holds the write turn: nothing is run a
  second time at the commit, and nothing conflicts. `Batch::expect`
  makes a statement a condition of the rest.
- **A Unix domain socket** beside the TCP port, for a client on the
  server's own host.

And where conflicts are the rule, the client now goes straight to the
write lock after a transaction's first conflict (see 4 below).

The benchmark prepares this server's statements as it does Postgres',
and runs the transfers a second time sent whole: a batch here, and for
Postgres one statement of chained `WITH` updates, which is the nearest
it has. `BENCH_SOCKET=1` reaches both servers through Unix sockets.

What the server executes for one request, counted by a profiler with
one connection, before and after (the counts do not depend on the
machine or on what else it is doing):

| Request | instructions before | after | |
|---|---:|---:|---:|
| Session lookup with user | 54,700 | 37,800 | 0.69× |
| Rate-limit upsert, unlogged | 67,100 | 52,600 | 0.78× |
| Durable insert | 63,800 | 62,000 | 0.97× |
| Transfer as a transaction of 2 reads and 3 writes | 202,500 | 178,500 | 0.88× |
| The same transfer sent whole | | 106,700 | 0.53× |

A statement gains by how much of it is expressions: the lookup's join
and filters and the upsert's `when(..)` are trees that are no longer
rebuilt, where the insert was three values to begin with. The transfer
gains a request less and five statements not decoded; sent whole it
also loses its fork, its digests and five requests' worth of reading
and answering. (With one connection nothing is run twice at the commit.
With eight, nearly every transfer was: that second run is what a batch
also saves, and the count above does not show it.)

What the socket saves depends on the system. A round trip that asks
for nothing (a ping) was measured at 23 to 25 µs over TCP on the
loopback address and 8 to 11 µs over a Unix socket on macOS, and at 10
to 11 µs against 9 µs on Linux: a third of a point lookup on the one,
a few percent on the other.

*What is left.* The table at the top of this file was measured before
these changes and has not been run again on that machine. Of a prepared
lookup's 37,800 instructions about 26,000 are the query itself; a sixth
of all of them is the allocator, which is what "fewer allocations per
row" under 1 is about. Encoding the rows is untouched.

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

*Done: the start of a large text kept in its row.* A column can keep
the first bytes of its large values in the row (`ColumnDef::keep_start`,
`DESIGN.md` "Rows"), and the benchmark's mails keep 256 bytes of their
text. The page's fifty previews are then read from the fifty rows, and
no body is looked up.

What that does was counted, not timed: the instructions the server
executes per request, from one connection, on the benchmark's 100,000
mails, under callgrind, as the difference between a run of 3N requests
and one of N, divided by 2N. Callgrind counts the same whatever else
the machine is doing. The ratios above were not measured again.

| Request | Before | Start kept | |
|---|---:|---:|---:|
| Mailbox page: newest 50, with a preview of each body | 1,036,000 | 878,000 | −15% |
| Count over a user's 5,000 mails | 41.2 million | 45.4 million | +10% |
| Flag update by key | 78,000 | 86,000 | +10% |
| Session lookup (another table) | 54,500 | 54,600 | |

A second run of each, with other N, came within 0.5% for the page and
the lookup, 2.5% for the count and 5% for the flag update.

The page loses a sixth of its work. The count and the flag update pay
for it, as this option's cost said they would: a mail's row grows by
the 256 bytes and their count, and the data file after the load from
628 MB to 665 MB. The mails' rows no longer fit in the 32 MB cache
beside the indexes, so a count reads pages from the file again: 3.5
million of its 4.3 million added instructions, and a system call for
each page, which callgrind does not count. With a cache the rows fit
in, the price is smaller; a table that is counted more than it is paged
through should keep less, or none.

*Done: fewer allocations per row.* A row read from a table allocated
three times (its bytes, where its columns start, a cell for each
value), its key a fourth, and a value read from it was made, kept, and
copied again on its way out. A row now keeps where its columns start
in itself (up to fifteen columns), makes a number or a text from its
bytes when asked, and keeps only what costs more to make than to copy
(a document, a list, a large value). The key of a row is lent to what
reads it, not copied, since only a write keeps it. A bound that asks
for one key, or every key with the same start, is evaluated and encoded
once rather than as both ends. Counted as above, on the same data, with
the start kept:

| Request | Start kept | Fewer allocations | | Allocations per request |
|---|---:|---:|---:|---:|
| Mailbox page | 878,000 | 738,000 | −16% | 606 to 354 |
| Count over a user's 5,000 mails | 45.4 million | 36.5 million | −20% | 21,700 to 5,930 |
| Flag update by key | 86,000 | 81,000 | −6% | 72 to 63 |
| Session lookup | 54,600 | 48,600 | −11% | 71 to 58 |

Most of it is the row: 136,000 of the page's 140,000 and 8.1 million
of the count's 8.9 million. Against main as it was before both changes
the page does 29% less work, the count 11% less and the lookup 11%
less; the flag update is where it was, within the noise of its runs.

The allocator was not changed. What it costs after a burst of writes is
threads waiting for each other's locks, which is time, not
instructions: callgrind runs one thread at a time and cannot show it,
and this machine, shared with other work, could not time it. The C
library's per-thread caches are set by its environment variables when
a process starts, not by `mallopt`, so the server cannot enlarge them
for itself. What was done takes away most of what threads contended
for: a count makes a quarter of the allocations it made.

What is left of the options:

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Read a row's bytes where they lie** in their page, instead of copying them out: the one allocation a row still costs | A count makes about one allocation per row; this would make it none | A few percent of a count; little for a page of fifty | Moderate: the row's lifetime runs through the executor, and the page must stay pinned while the row is read |
| **An allocator with larger per-thread caches** | Threads contend for its locks less | Whatever is still lost after writes, which has to be timed again first | A dependency, where there are three; or `GLIBC_TUNABLES=glibc.malloc.tcache_count=...`, which only the container image can set |
| **A preview column** kept by the application | No large value is read | A fifth, with nothing to build | None in the server; a column to maintain |

### 2. Unlogged upserts from many connections (0.80×)

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Keep volatile tables in memory only**: they are empty after a restart anyway | No page copies and no version per write; such a table could take a lock of its own and be written beside the database's writer | Probably past Postgres for this workload | Moderate; such a table would no longer roll back with a transaction or be read from a snapshot, which has to be acceptable for what is kept in it |
| **One version for a batch of jobs**: the thread that runs waiting writes back to back begins and commits once for all of them | About 4 µs of the 17 | About a fifth at eight connections, nothing at one | Small to moderate: a failed statement must undo only itself |

A prepared form of the request, which was a third option here, is done:
an upsert is 52,600 instructions in the server where it was 67,100 (see
"What a request costs").

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
- **Large requests share 4 MB of memory** to be decoded and answered
  in, taken once a request's bytes have all come, and what each held is
  given back to the system as it is answered (`session/budget.rs`).

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
beside as well (six runs), and 112 to 125 MB with the budget (six runs).
With the budget and a trim once a second instead of five times, it was
144 to 159 MB (three runs). The share was first taken before a request's
bytes were read, so that a fourth connection waited with its statement
still in its socket; but then a client that announced a large frame and
sent it slowly, or not at all, held the budget meanwhile, and every
other connection's large request waited for it. The share is now taken
once the bytes have come, before they are decoded: the bytes of a
request waiting for the budget are in memory, as they were before there
was one, and what the budget bounds is the decoding, the cutting into
words and the wait for the turn. Measured again, alternating the two
builds on the same machine (six runs each), the peak was 113 to 124 MB
with the share taken first and 117 to 130 MB with it taken after the
bytes; the 130 MB run is the only one over 125.

On this machine, shared with other work, the load ran at a median of
7.3 statements a second before and 9.1 after, in runs that moved
between 5.6 and 8.9 before and 5.5 and 14.0 after; those rates say
little. The server's CPU time for the load was 47 to 56 s before and 43
to 55 s after; the trims take part of what was saved (40 to 46 s
without the budget).

Merging beside the writer leaves the data file larger. Stopped as soon
as the load was over, the server's file held 803 MB: 83,729 pages in
use and 14,316 free, where it held 665 MB, 73,968 in use and 7,221
free (`database check`). The pages in use over those were full-text
segments not yet merged, 29 in the index against 8: the merging thread
takes the cheapest run that is due first, so the large runs wait while
the load goes on, and the stop gave up the merge under way. An engine
now looks for due segments when it opens a database. Started again on
that file, it made 7 merges, and then held 73,960 pages in use, as many
as before, in a file of 900 MB with 35,881 pages free.

The rest is free pages, which later writes take before they grow the
file. A merge writes its tree before the segments it replaces are
freed, and it holds the version it reads while it runs, so that no page
freed meanwhile is given out again. Loading the same 100,000 mails
twice into one database through the engine (one writer at a time, no
server), the file grew by 1,549 MB and then by 251 MB, where before it
grew by 598 MB and then by 612 MB: 1,800 MB and 1,210 MB in all, with
83,388 and 11,780 pages free.

| What is left | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Build indexes after loading**, for `restore`: load the rows, then write each index bottom-up from sorted entries with the tree `Builder` | A restore pays for neither pending rows nor merges | Several times faster restores; nothing for ordinary inserts | Moderate; not begun |
| **Cut words ahead inside transactions** | Only a large insert that is its own transaction is cut ahead now | The same for loads made in transactions | Small |
| **Hold only what a merge reads**: keep the pages of the segments being merged from being given out again, not every page freed meanwhile | Writers reuse freed pages while a merge runs | Fewer free pages after a load (how many is not measured) | Moderate: the store keeps whole versions, not sets of pages |
| **Fewer merges dropped**: a writer that merges past 40 segments changes the segments a merge beside it is reading | In the engine-only loads above, 4 of 14 and 10 of 30 merges were dropped and their trees freed | Less work, fewer free pages | Small to moderate: the writer could merge only segments newer than those being merged |
| **Trim less, at less cost**: return only the pools the large requests used | The CPU time the trims take | A few seconds of the load's CPU | Small; the C library trims all pools at once |

### 4. Transactions of several requests (0.91×)

Three of the four options that stood here are done (see "What a
request costs"):

- **`begin` travels with the first statement**: six round trips where
  there were seven. If the begin fails the server leaves the connection
  with a dead transaction, so that the statements meant for it cannot
  run outside one.
- **A batch**: one round trip, and nothing to run twice. For the
  contended workload it is more than that: a batch never conflicts,
  because it is the writer while it runs, so eight connections crediting
  one account go as fast as eight crediting different ones.
- **Straight to the write lock after one conflict, where conflicts are
  the rule.** `optimistic_runs` sets how many runs beside other writers
  a transaction gets (2 unless set). And without being told, a client
  counts how its transactions' first runs end: while more than a third
  of them conflict, one that conflicts is not run beside the others
  again, where it would mostly conflict again, but as the only writer.
  In the workload where every transfer credits the same account that
  halves the conflicts (one per transaction, not two); where conflicts
  are rare nothing changes.

| Option | What it changes | Expected effect | Cost |
|---|---|---|---|
| **Wait instead of running again**, for rows that have just conflicted: a transaction that is about to read such a row waits for the one that holds it | Contended transactions queue for the row, as in Postgres, without holding up the writers of other rows | The runs that conflict; and transactions on other rows no longer wait behind the contended ones | Large: this is row locking, with its waits and its deadlocks to time out |

### 5. Point lookups and durable inserts (0.85× to 1.06×)

Within a tenth, sometimes ahead. A lookup's cost was mostly outside the
engine: decoding the request into a query value and encoding the rows.
A prepared statement now skips the first (see "What a request costs");
what is left outside the engine is encoding the rows and the trip
through the network stack, which a client on the server's own host
shortens with the Unix socket. A durable insert on one
connection is the disk's sync and little else on both servers.

## The SQLite family

`bench/` runs six of the workloads against embedded SQLite, rqlite and
libSQL's server as well: the session lookup on 1 and 16 connections, the
durable insert on 1 and 16, and the two transfers. The data and the row
picks are those of the other targets (20 users, 20,000 sessions, 1,000
accounts; the same `mix`, `token` and `user_id`). The other workloads
are not run against them: the mails and their search were not set up
three more ways.

How each is run, and what that does to a comparison:

- **Embedded SQLite** (`--only sqlite`; `rusqlite` 0.32 with the SQLite
  it bundles) is opened in the benchmark's own process: no server and
  no socket, so a read costs no round trip at all, and it has no
  process whose memory and CPU could be measured apart. WAL mode with
  `synchronous=FULL`, so that every commit is synced before it returns
  (and `fullfsync=ON`, which on macOS makes that the sync this server
  makes, and elsewhere changes nothing). Statements are prepared once
  per connection; a writing transaction begins with `BEGIN IMMEDIATE`.
  A worker is a thread. Readers get a connection each. Writers are
  measured two ways, a column each:
  - **one write connection** behind a lock, which is how an application
    usually runs embedded SQLite from many threads;
  - **a connection each**, with a busy timeout of 10 s. Writers then
    wait for each other inside SQLite, and one that waits out the
    timeout gives up; the report counts those. This column runs a
    quarter of each workload's operations, because on a disk with a
    slow sync it takes minutes over them, and only the workloads where
    several connections write.
- **rqlite** (`--only rqlite`) is one node, `rqlited -node-id 1`, with
  its defaults: every write is an entry in its Raft log, which is
  synced before the answer, while its SQLite file is written with
  `synchronous=OFF`, and foreign keys are not enforced. Reads ask with
  `level=none`: the node answers from its own state without asking
  whether it still leads, as every other target answers. rqlite has no
  transaction that spans requests, so its transfer is sent whole as one
  request, with the balance check folded into the first `UPDATE`: one
  round trip, where this server's transfer is seven and has its
  statements run twice. That is the batch request listed under 4 above.
- **libSQL's server** (`--only libsql`) is `sqld -d <dir>`, asked
  through Hrana over HTTP. Its transfer is an interactive transaction
  in two requests: `BEGIN IMMEDIATE` and the reads, then the writes and
  `COMMIT`. When the comparison was first made (version 0.24.32) it ran
  SQLite with `synchronous=NORMAL` and refused `PRAGMA synchronous`
  from clients: a commit was then not synced on its own, and its
  durable insert is not the same work as the others'.

Both servers are reached over HTTP with JSON, on one connection kept
open per worker. Keys that are bytes elsewhere travel as hex text,
because rqlite's JSON parameters carry no bytes. A request that gets no
answer within `BENCH_HTTP_TIMEOUT` seconds (120) ends that target's run:
the report says which workload did not complete, and keeps what came
before.

The embedded SQLite and rqlite targets have been run from this
repository, on Linux (rqlite 10.7.4). The libSQL target has not: it was
written from the program the comparison was first made with, and only
its requests have been tested, not their answers from a real `sqld`.

## At the small end, and the large

Where CPU per request decides throughput, on one vCPU with a small
memory limit, the benchmark can put each server it starts under those
limits (Linux only):

```sh
BENCH_SERVER_CPUS=1 BENCH_SERVER_MEMORY_MB=256 ./target/release/bench
```

- `BENCH_SERVER_CPUS=n` pins each server, with `taskset`, to the first
  `n` CPUs the benchmark may use, and moves the benchmark's own client
  to the others, so that the client's work is not taken out of the
  server's share. (An ordinary run has both on the same cores.)
- `BENCH_SERVER_MEMORY_MB=m` starts each server in a memory cgroup of
  its own, with that limit and no swap (`memory.max` on cgroup v2,
  `memory.limit_in_bytes` on v1), and removes it afterwards. That is
  what a container's memory limit is: the server's own memory and the
  file cache of its files both count, the kernel drops cache to stay
  under, and a server whose own memory goes over is killed. The report
  says so if it happens. Making a cgroup takes root on the host, or a
  container that may write to its cgroups. Where none can be made, the
  run says that the limit is **not enforced** and goes on without it.
- The report's **most memory held during the run** is the server's own
  high-water mark (`VmHWM`) for a server that is one process, and for
  Postgres the largest of two samples a second of all its processes,
  shared pages counted once. With no enforced limit it is the figure to
  read against the limit.

Embedded SQLite runs inside the benchmark, on the client's CPUs and
under no memory limit.

The large end needs nothing set: run it on a machine with 16 cores or
more. `BENCH_SERVER_CPUS=16` there keeps the client off the server's
cores. One writer per database is expected to cost more there on the
workloads that write.

### One CPU: commits that share syncs

On one CPU the server's pool has one thread. A commit that syncs the log
on that thread holds up every other connection while it does: none of
their requests is read and none of their records is written. Synced
there, 16 connections making durable inserts took one sync per commit
(16,056 `fdatasync` calls for 16,000 commits, counted with
`strace -c -f`) and reached about 4,200 a second against Postgres'
12,500, though the server used a fraction of its CPU. So the commit that
syncs first hands the thread's other connections to another thread
before it syncs (see "The server" in `DESIGN.md`). Without that and with
it, measured on a shared machine with the server on CPU 0 and the client
on the other three, each figure from `BENCH_DETAILS=1` (syncs) and
`/proc` (CPU time and context switches of all the server's threads), on
a fresh database:

| `BENCH_SERVER_CPUS=1` | synced on the thread | handed over |
|---|---:|---:|
| durable insert, 16 connections | 3,500-5,900/s | 22,800-30,600/s |
| syncs of the log, 16,000 inserts on 16 connections | 16,056 (`strace`) | 3,440-3,550, with 2,000 lone inserts, a sync each |
| server CPU per insert, 16 connections | 57-63 µs | 27-32 µs |
| context switches per insert, 16 connections | 2.0-2.1 | 0.7-1.0 |
| durable insert, 1 connection: p50 (four runs of 20,000) | 184-245 µs | 192-235 µs |
| CPU per insert, 1 connection (the same runs) | 69-87 µs | 69-84 µs |
| context switches per insert, 1 connection | 3.1 | 4.1 |

A lone commit pays one more context switch, made while the disk works,
and no measurable time. Against Postgres in the same runs (two runs, on
a database without the bulk load before it), the one-CPU workloads that
were behind are ahead: durable inserts on 16 connections 2.0-2.3×,
durable upserts 1.5-2.1×, the transfer transaction 1.9-2.0×, the
transfer sent whole 2.4-2.6×, a durable insert on one connection
1.1-1.3×, a session lookup on one connection 0.95-1.01×. On four cores
nothing is handed over: context switches per request are the same with
the change and without, and a lone durable insert takes the same 62,000 instructions
in the server (callgrind, the difference between runs of 2,000 and
10,000 inserts).

What remains is the quarter of a minute after the bulk load. The
engine's thread that merges full-text segments goes on with the segments
the load left due, and on one CPU it takes a third to a half of it from
the requests: during the session lookups that follow the load, the
request threads spent 300-370 ms of each second waiting for the CPU
while it ran, and under 15 ms once it was done
(`/proc/<pid>/task/*/schedstat`). The workloads that run in that window
in the benchmark's order came out, in one run against Postgres, at 0.71×
(session lookup, one connection), 0.85× (durable insert, one connection)
and 0.66× (durable insert, 16 connections); the same workloads on a
fresh database are at par or ahead. Running the merging thread at the
lowest priority (`nice` 19) cut the requests' wait to 70-85 ms a second,
but starved the merges during the bulk load itself: 1-2 merges beside
the writers instead of 67-70, the writers merging for themselves
instead, and the load slower (6.8-7.0 statements a second against
7.6-7.7, p99 1.4-2.6 s against 0.8-1.2 s). It was not kept.

## Running it

```sh
cargo build --release
./target/release/bench                 # this server and Postgres, if Postgres is installed
./target/release/bench --only ours
./target/release/bench --only ours,postgres   # --only takes several, or all
BENCH_DETAILS=1 ./target/release/bench                   # also: each server's own counts
BENCH_WORKLOADS="transfer,upsert" ./target/release/bench # only the workloads named so
BENCH_REPEAT="mailbox:30" BENCH_WORKLOADS=mailbox ./target/release/bench --only ours   # one workload, longer, to profile
BENCH_KEEP=1 BENCH_WORKLOADS=mailbox ./target/release/bench --only ours   # keep this server's loaded data for the next run
BENCH_CONNECTIONS=1 BENCH_WORKLOADS=upsert ./target/release/bench --only ours
BENCH_SOCKET=1 ./target/release/bench  # both servers through Unix domain sockets, not TCP
BENCH_SERVER_LOG=server.log ./target/release/bench --only ours
BENCH_SERVER_CPUS=1 BENCH_SERVER_MEMORY_MB=256 ./target/release/bench   # each server on one CPU, under 256 MB
```

`database stats` (and `BENCH_DETAILS=1`) says how the commits of
optimistic transactions went: as they stood, run again, or in conflict.

The SQLite family, on the workloads it runs:

```sh
cargo build --release -p database-server -p database-bench --features database-bench/sqlite
BENCH_WORKLOADS="session,durable insert,transfer" ./target/release/bench --only ours,sqlite,rqlite,libsql
```

Embedded SQLite is compiled in only with the `sqlite` feature, which an
ordinary build leaves off because it compiles SQLite's C source.
`rqlited` and `sqld` are found on the `PATH`, or where `BENCH_RQLITED`
and `BENCH_SQLD` say; a target that is not installed is skipped with a
line saying so, as Postgres is.
