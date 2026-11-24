# How it works

A map of the code for whoever changes it next: what each part does, the
rule it keeps, and why it is built that way and not another. The
requirements are in `REQUIREMENTS.md`; the client's contract is in
`CLIENT.md`; the wire protocol is in `PROTOCOL.md`; running it is in
`OPERATIONS.md`; how it compares with Postgres, and why, is in
`PERFORMANCE.md`.

```text
protocol/   values, expressions, queries, the wire format     (no I/O)
client/     the typed async client applications use
clients/    the same client for TypeScript and for Python     (from PROTOCOL.md, not from the Rust)
engine/     storage, transactions, query execution            (no network, no async)
server/     the network server and the `database` command
bench/      the same workloads against this and against Postgres
```

An application holds a `Client`. A request is a Rust value (`Query`, `Insert`,
`TableDef`, ..) encoded into one frame and sent over TCP, or over a Unix
domain socket to a server on the same host. The server decodes it and
calls the engine, which runs it against one database's files and returns
rows.

## What was left out, and what that bought

There is no SQL, no planner and no optimizer. A `Query` names the index
it reads and the range of keys; joins are lookups the client spells out;
a filter is applied to whatever the range gives. The server never has to
guess, so the same request always costs the same, and the executor is a
few hundred lines.

There is one writer per database at a time. That removes row locks, lock
ordering and deadlocks: whatever is written is written by one
transaction at a time, on the latest committed state. Every `FOR
UPDATE`, `FOR SHARE`, `SKIP LOCKED` and advisory transaction lock an
application takes reduces to "do it in a transaction". The price is that
writes do not run in parallel within one database, and two things keep
that price small. Commits from different connections share a sync. And a
transaction of several requests does not hold the writer's place while
its owner thinks: it runs on a copy beside the writer and takes the
place only to commit (see "Transactions"). `docs/PERFORMANCE.md` shows
what is left of the price, and what could be done.

## Files

A database named `shop` is three kinds of file in the data directory:

```text
shop.db             pages of 8 KiB
shop.wal-0000000007 the log of commits since the last checkpoint
ACCESS              every database's password hash (one file for the server)
LOCK                held while a server has the directory open
```

### Pages (`page.rs`, `store.rs`, `cache.rs`)

The data file is an array of 8 KiB pages, each with a 16-byte header that
holds a CRC of the page. A page that fails its CRC is reported as
corruption, never returned.

Pages 0 and 1 are two copies of the **meta page**: where the catalog and
the free list are, how many pages the file has, which log file continues
from here. A checkpoint writes them alternately, so there is always one
whole meta page that describes a whole, synced state.

**Nothing is overwritten in place.** A change to a page writes a new page
and frees the old one; the parent then changes too, up to the root. A
tree is named by its root page, so an old root still names the tree as
it was. This one rule gives three things:

- **Snapshots for readers.** A reader takes the current catalog, which
  is the set of roots, and reads from it for as long as it likes.
- **Atomic checkpoints.** Until the meta page is switched, the file on
  disk is the previous checkpoint, whole.
- **Cheap rollback.** A transaction that rolls back just drops its pages.

A freed page cannot be reused while anything can still read it. Two
things can: a reader on an older version, and the last checkpoint (if
the machine dies now, recovery starts from it). So the free list keeps
freed pages in two waiting rooms, by the version that freed them, and
releases them when the oldest reader has moved on and a newer checkpoint
is on disk. Pages that were allocated and freed between two checkpoints
never were in a checkpoint and wait for readers only; without that, a
bulk load would grow the file by every intermediate version of every
page it touched.

Pages are held in three places:

- **Private** to a writing transaction: its own new pages, which it may
  still change in place. Past a limit they are written to the file early
  (to pages nothing refers to yet), so a huge transaction does not need
  huge memory.
- **Dirty**: committed, not yet in the data file. The log makes them
  durable; the next checkpoint writes them.
- **The cache**: clean pages read from the file, shared by all databases,
  with a byte budget (`DATABASE_CACHE_MB`, 32 by default) and CLOCK
  eviction. Every query passes through the same few pages at the top of
  each tree, so finding a page there takes no lock that another reader
  could hold: a lookup only reads its part of the cache, and a query
  keeps the upper pages it has met at hand for its next row.

### Trees (`btree.rs`)

Everything stored is a B+tree of byte-string keys and values: a table is
one (primary key to row), each index is one, the large values of a table
are one. Leaves hold `key, value` cells; branches hold `child page,
lowest key`. Cells are at most a quarter of a page, so a split always
has somewhere to put things. There are no sibling links (copy-on-write
would have to rewrite them); a cursor keeps the path it came down by.

A change to one entry moves the bytes after it within its page, on the
page itself if the writer made the page in this transaction and on one
copy otherwise; a page is taken apart into its cells only when it must
split. Keys that arrive in rising order fill their pages: a full
rightmost page is left as it is and a new one started. A `Builder`
writes a whole tree from sorted entries, each page once.

### Keys (`keys.rs`)

A key is the row's key values written so that comparing bytes compares
the values: integers big-endian with the sign bit flipped, text with a
terminator, decimals as sign, exponent and digits, NULL last, a
descending key as the same bytes inverted. A composite key is the parts
one after another, so a prefix of the values is a prefix of the bytes.
That is what makes a `KeyRange` of prefixes work, on the primary key and
on every index alike.

An index entry's key is the index's key values followed by the primary
key; its value says where the primary key starts. So entries with equal
index keys are distinct and ordered by primary key, any entry leads to
its row, and a unique index is checked by looking for another entry with
the same leading bytes.

### Rows (`row.rs`, `blob.rs`)

A row is its column values in the protocol's encoding, with a count in
front. A row read from disk is kept as bytes and each column is decoded
when first asked for, so a query that selects two columns of a wide row
pays for two.

A value too large to share a page with its row (a long text) is moved to
the table's tree of large values, cut into chunks keyed by `(id, chunk
number)`, and the row keeps the id and the length. `octet_length` reads
the length; `left` and `substr_bytes` read only the chunks they need. A
column added to a table with rows is not written to them: a row shorter
than the table's column list reads the new column's fill value.

### The log (`wal.rs`)

Every commit appends one record to the log: the **logical** changes of
the transaction (insert this row, update these columns of that key,
create this table), not page images. A record carries a CRC that also
covers the log file's generation and the record's position, so a record
from an older file or a torn write cannot be taken for a valid one.

A commit is acknowledged only after its record is on disk. Commits that
arrive while a sync is running share the next one: whoever finds nobody
syncing does it for all that are waiting ("group commit"). That is why
16 connections each making a durable insert get several times the
throughput of one.

A file that grows with every record makes every sync slow, because the
file system must then commit the file's new length as well as its bytes:
measured here, four times as slow. So a log file that gets a second
record is grown ahead of its records with zeros, in steps that double up
to a megabyte, and most records are written over room the file already
has. A record is never empty, so the zeros end the log for a reader.

Tables marked `volatile` (Postgres' `UNLOGGED`) are not logged; they are
emptied on recovery. Their commits need no fsync.

### Checkpoints and recovery (`store.rs`, `db.rs`)

A checkpoint makes the data file current: under the write lock it takes
the set of dirty pages and starts a new log file (a moment's work); then,
with writers running again, it writes those pages, syncs, writes the
other meta page, syncs, and deletes the old log. One runs when the log
passes 8 MiB or dirty pages pass their limit, and at a clean stop.

Opening a database: read both meta pages and take the newer valid one;
empty the volatile tables; replay every log record after it, stopping at
the first record that is incomplete or fails its CRC; checkpoint. In the
newest log file such a record is the tail a crash tore, and it was never
acknowledged. In an older file it is damage, since a file is synced
whole before the next is begun: if commits follow it in a later file,
the database refuses to open rather than apply them over the gap. Replay runs the same functions that executed
the statements the first time, so there is one code path to trust.

`engine/tests/recovery.rs` runs a random workload over a simulated file
system that can cut power at any write, keep or drop or tear each
unsynced write independently, and checks after every reopening that the
database holds exactly the acknowledged commits, with every index, the
free list and every page accounted for.

## Transactions (`db.rs`, `write.rs`)

A **version** is an immutable catalog: every table's definition, its
tree roots and its counters. The database publishes one version at a
time as current.

- A **reader** pins the current version and reads its trees. It takes no
  lock and blocks nobody.
- A **writer** takes the write lock, copies the catalog, and writes new
  pages. At commit it appends its log record, makes its catalog the next
  version, releases the write lock, waits for the fsync, and only then
  is the version published to readers and the client answered. So a
  commit is visible to no one before it is durable, and to every
  connection once the client hears of it.
- A statement that fails rolls back to the start of the statement: its
  pages are dropped and the catalog restored. Inside an explicit
  transaction the transaction is then poisoned, as in Postgres, unless a
  savepoint was taken.
- `now()` is read once when the transaction begins.

A **fork** is a writer without the write lock: a transaction on a private
copy of the newest durable catalog, running beside the writer and other
forks. It reads as a reader does and writes pages nobody else can reach
(lent to it by the store, and counted free by any checkpoint taken
meanwhile, since a crash ends the fork). To commit it takes the lock. If
no commit was made since it began, its copy is the next version as it
stands. Otherwise it cannot be: its pages build on trees that have moved
on. The fork is dropped and its owner (the server, which kept the
requests) runs them again in an ordinary writer, checking that each
answers as it did the first time. If all do, the transaction's owner was
told nothing that is no longer true, and committing the second run is
the same as having run alone at that moment; if one does not, the
transaction fails with `Conflict`, having changed nothing. So that the
second run can answer the same, what a statement draws rather than
derives is kept from the first run and given to the second: random ids,
and the numbers of `auto_increment` counters. Every transaction, a fork
or not, takes those numbers from one count kept beside the tables, which
moves on whether or not the transaction commits, so that no two are
given the same. The clock is the fork's own.

A statement on its own reads the newest commit, which may still be on
its way to the disk. Whatever it then answers, rows or none or an error
such as a key already taken, rests on that commit, so the answer waits
for it as it would for a commit of its own.

A writer that would wait longer than `lock_wait` gets `LockTimeout`. A
connection that holds a writing transaction open and goes quiet for a
minute has it rolled back, so a stuck client cannot stop all writes. One
that sits on a reading transaction is closed after ten: while a snapshot
is held, no page freed since can be used again.

## Executing requests (`exec.rs`, `eval.rs`, `write.rs`)

A query runs as a pipeline in a fixed order: the rows of the key range;
joins (each a lookup per row so far); `each` (one row per item of a
list); the filter; grouping; ordering; offset and limit; the selected
values. With no `order_by` the pipeline stops as soon as the limit is
reached, which is what makes "the newest 50" cost 50 rows.

**What a statement read is counted as it reads**: an entry for each
index entry a range or a tree walk gives and each hit of a search, a
row for each row read from a table, by any path. The two counts are
all the explaining there is to do where nothing is planned: a
statement's cost is what its own access path made the server read.
They are what `Request::Explain` answers with in place of a query's
rows, and what the server writes to its log beside a statement that
took longer than `DATABASE_SLOW_MS`.

Expressions are a tree (`Expr`) evaluated against a chain of scopes:
the current row, the rows of enclosing joins and queries, the proposed
row of an upsert, the old row of an update. NULL follows SQL's
three-valued logic. Comparing values of different kinds is an error,
except that text is read as a uuid, timestamp, date or decimal when
compared with one, as an untyped SQL literal would be.

Writes find their rows with a query, collect them, then change them one
by one through three functions (`apply_insert`, `apply_update`,
`apply_delete`) that keep the row tree, every index and the large values
in step and add to the log record. Constraints are checked before a row
is written: types and NOT NULL, checks, unique indexes, then foreign
keys; a delete then visits the tables that point at the deleted rows.

## Full-text indexes (`fts.rs`, `text.rs`)

The one index that is not a tree of entries. An entry per word per row
would be hundreds of writes all over a tree for each row. Instead:

- A new or changed row is **pending**: its words and scores are stored
  as they are, and a search reads the pending rows directly.
- When pending rows pass 256 KiB they become a **segment**: for each
  word, the list of rows that hold it, as differences between row
  numbers (two or three bytes a row), written as a tree in one pass.
  Each row's words are stored in order, so the segment is written by
  merging the rows as they lie, with nothing copied out per word.
- Segments of a similar size are **merged** four at a time, so each
  entry is rewritten only as often as the index quadruples. Where no row
  of theirs has died, their lists are joined as stored, not taken apart.
- A row deleted or changed is forgotten (its number no longer leads to
  a row) and its entries are dropped when its segment is next merged,
  or at once when half a segment is dead.
- Segments are **merged beside the writers.** A merge reads nothing but
  segments, which never change once written, so the writer that makes
  one due only says so. A thread of the engine's merges on a version
  held as a reader holds one, into pages lent by the store as a fork's
  are, and takes the write lock only to put the merged tree in place:
  a commit with nothing in the log, which after a crash is simply not
  there (the log's rows are indexed and merged again as it is
  replayed). The tree is put in place only if the segments it read are
  in the index as they were: the same trees, ranges of row numbers and
  counts of dead rows. A tree is only ever replaced, its pages are not
  given out again while the merge holds its version, and a row cannot
  die without its segment's count rising; so equal segments hold
  exactly the rows the merge kept. Otherwise the merge is dropped. A
  writer still merges at once a segment that is mostly dead, and the
  cheapest run of segments once more than 40 have gathered, so that a
  search never meets many.

A search looks each word up in every segment (a handful), intersects the
lists, and adds the scores. Text is cut into words and, for English,
stemmed by the Porter algorithm, with the same code that `Func::Matches`
and `Func::Headline` use without an index. A text's words are gathered
sorted and packed in one buffer, and a row's words are held in the form
a pending row stores them: nothing is allocated per word.

Cutting text into words needs nothing but the text, so a large insert
can have it done before its turn to write (see "The server"). Words cut
ahead are used for a row only if the text the row is written with is
the very text they were cut from, compared whole: a default in place of
a text not given, an upsert that writes another text, or a table changed
meanwhile only means that the row is cut again under the turn.

## The server (`server/`)

One process serves every database in the data directory. Each connection
is a task on a small pool of threads (at most four). A frame is a
four-byte length and a payload; a result too large for one frame is sent
as several.

**A request is answered on the thread that read it, when it can be.**
Waking another thread costs more than most requests do: with every
request handed to a thread of its own, a point lookup took 70 µs where
Postgres took 53, and a durable insert 400 µs against 170. So:

- A **read** is tried where it is for a quarter of a millisecond. If it
  has not finished, it is abandoned and begun again on a thread of its
  own, where it may take as long as it likes. A read changes nothing, so
  nothing is lost but that moment; and a slow query cannot hold up the
  connections that share the thread.
- **Writers queue for their turn** at a database without holding a
  thread, in a gate the server keeps per database; whoever has the turn
  finds the engine's own write lock free (but for the moments a
  checkpoint holds it).
- A **brief write** (a few rows inserted, rows updated by key, in a
  small request) is a job for the gate: it runs where it arrives if the
  turn is free, and otherwise whoever holds the turn runs the jobs
  waiting behind it, back to back, before passing it on. Each
  connection then sends its own answer. Between two small writes
  nothing waits for a task to be woken, which costs more than the write.
- Anything else, and every delete not by key, for what it may cascade
  to, takes the turn itself and runs on a thread of its own.
- A **large insert** that is its own transaction, into a table with a
  full-text index, first has its texts cut into words on the
  connection's own thread, beside whoever has the turn
  (`session/load.rs`). Several connections that load at once then cut
  on as many cores, and take turns only to store rows and the words
  already cut.
- An **optimistic transaction** is a fork (see "Transactions"): its
  statements run without the turn, and the connection keeps each
  request with a digest of its answer and what it drew. Its commit is a
  job like a brief write: attach the fork if nothing was committed
  beside it, or else run the requests again and compare the digests. A
  transaction asked for something not worth doing twice (a large
  statement, a change of schema) is settled the same way early and
  carries on holding the turn.
- A **commit** writes its log record, passes the turn on, and then
  waits for the disk without holding a thread: if nobody is syncing the
  log it syncs it itself, once, and otherwise it is told by whoever is.
  Only when its version is on disk and published does the client hear.
  A commit that put nothing in the log (it changed only tables that
  need not survive a crash) is published at once.

**What a small request costs is mostly not the engine**: the trip
through the network stack, decoding the request into a tree of
expressions, and for a transaction one round trip per statement. Three
things cut that, each a request kind of its own (`message.rs`):

- A **prepared statement** is kept by the connection, decoded, under a
  number its client chose; run again, the request is that number and
  the values for the statement's parameters (`Expr::Param`), which the
  evaluator reads from its context. Nothing is substituted into the
  tree, so nothing is rebuilt. The client sends the statement with the
  first request that names it on a connection, so preparing is not a
  round trip of its own. A connection keeps 256, letting go of the one
  used longest ago; asked for one it let go it says so, having run
  nothing, and the client sends it again. A session lookup's request
  went from 54,700 instructions in the server to 37,800.
- A **transaction begins with its first statement** (`BeginWith`): one
  request, answered as the statement. If the transaction cannot begin,
  or the statement is refused before it could (it is not a statement,
  or one too large to keep), the connection is left with a dead
  transaction that can only be ended, as after a failed statement, so
  that whatever the client sends next cannot run outside the
  transaction it was meant for. Only a statement the connection no
  longer keeps is answered with nothing begun: the client sends that
  request again.
- A **batch** is a whole transaction in one request, for statements
  that need no answers from each other. It runs as one job in the
  gate, like a brief write: it has the turn while it runs, so there is
  no fork, nothing to compare at a commit and nothing to run twice. A
  statement can carry the number of rows it must affect, and the batch
  is undone if it does not: a condition without a round trip. The
  answer is each statement's result in order, or one error.

A fork run again by the server (see "Transactions") runs each kept
statement with the values it had the first time: they are part of what
the connection keeps of a step.

A connection says which database it wants and proves it with that
database's password (stored as a salted hash; the admin password comes
from the environment and may do anything). The protocol is not
encrypted: the server is meant for a private network, such as the one
between an application's containers.

Given a path, the server also listens on a **Unix domain socket**
there. A connection is served the same whichever way it came (the
session is written over any pair of byte streams), and is asked for the
same password; what differs is that the file's permissions decide who
may connect at all, and that each request and answer skip the loopback
network's work: 15 µs of a round trip on macOS, 2 µs on Linux, as
measured (`PERFORMANCE.md`).

Named locks live in the server, per database and name, owned by a
connection and released when it closes.

**Large requests share a memory budget** (`session/budget.rs`). A
request larger than 64 KiB takes a share of 4 MiB, counted in its bytes
on the wire, before its body is read, and gives it back once it is
answered; one that finds the budget spent waits, its bytes still in its
socket, in the order of arrival. A request of ordinary size takes
nothing and never waits; one larger than the budget takes all of it and
runs alone. A connection that holds the turn between requests (an
exclusive transaction) never waits for the budget, since whoever holds
the budget may be waiting for that turn: it takes what is free and
runs. As a large request is answered, the memory it held is handed back
to the system, at most five times a second; left in the allocator's
pool of the thread that decoded it, it was most of what the server held
during a bulk load.

## Dump and restore (`dump.rs`)

A dump is a stream: a header, then for each database its name and
password hash, each table's definition and rows in batches, and a
closing record with counts; every record carries a CRC, and the stream
ends with a count of databases. It is taken from one snapshot per
database, so it is consistent however long it takes and writers are not
held up.

A restore reads the stream into new databases under temporary names,
row by row, rebuilding every index as it goes. The rows are taken as
they were dumped: each record's checksum is verified, but unique keys,
foreign keys and checks are not tested again. Only when the whole stream
has been read and verified are the databases renamed into place. A stream that is cut short or
damaged leaves the server as it was.

## Limits worth knowing

| What | Limit |
|---|---|
| A key (primary or index, all parts together) | 1,900 bytes |
| A value | 256 MiB per request frame |
| One transaction's changes | 1 GiB of log; a larger one fails at commit and must be made in parts |
| Text cut into words for a full-text index | the first 1,000,000 bytes; words over 64 bytes are skipped |
| Nesting of expressions and JSON | 100 levels |
| Writers | one at a time per database; an optimistic transaction runs beside them and takes its turn only to commit, an exclusive one holds it between its requests |
| Text order | by bytes (use `lower()` for case-insensitive order) |
