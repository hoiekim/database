# What the database has to do

This is the list the client API and the test suite are built from: what
an ordinary web application asks of the relational database behind it.
The patterns are the common ones (look a row up by key, page through a
range, upsert a counter, spend a token exactly once, walk a tree, search
text) together with the less common ones that real applications turn out
to lean on.

Each requirement below has an id and says how the database meets it. The
test that covers it carries the id in a comment, so `grep -r R-W4
server/tests` finds the proof. `docs/DESIGN.md` explains the choices this
list forced.

Nothing here is SQL. An application talks to the database through a
typed Rust client, and where a SQL database did the work in a planner or
a stored function, the application now states the access path or builds
the expression itself. `docs/CLIENT.md` has the SQL-to-API equivalents.

The scenario suites in `server/tests/` exercise the list the way four
kinds of application would: an identity service, a mail store, a document
workspace and a personal-finance ledger.

## What is left out

Not built, because the applications this is for do not need it:
`LISTEN`/`NOTIFY`, window functions other than a total row count,
`UNION` (two queries, merged by the caller), sequences other than
auto-incrementing ids, triggers and stored functions (each is an
expression the application can build, see R-Q20 and R-Q17), views,
replication, explicit isolation levels, cursors, `COPY`.

## A. Values

| Id | Requirement | How |
|---|---|---|
| R-T1 | Booleans, 64-bit integers, floats, text, bytes | `Value::{Bool, Int, Float, Text, Bytes}` |
| R-T2 | UUIDs: read in either letter case, printed lower-case, with a total order that keyset pagination can rely on | `Value::Uuid`, ordered by its bytes |
| R-T3 | Timestamps to the microsecond, compared and paginated exactly (rows stamped by one statement are found again by equal timestamps) | `Value::Timestamp`, microseconds since the epoch |
| R-T4 | Calendar dates; date and timestamp text read by the server, an impossible day refused | `Value::Date`; text is parsed when written to a date or timestamp column |
| R-T5 | Exact decimals for money: `NUMERIC(15,2)` and `(15,6)`, rounded to scale on write, overflow refused, compared exactly (`a.amount = -b.amount`), accepted from floats | `Type::decimal(p, s)`; floats convert through 15 significant digits, as Postgres does |
| R-T6 | JSON documents stored whole and returned with object keys in Postgres' `jsonb` order (by length, then bytes), so that responses built from them do not change | `Value::Json`, put in that order when stored |
| R-T7 | Ordered lists of text or uuids | `Value::List`, `Type::list(..)` |
| R-T8 | Large text: bodies of many megabytes, whose length, first few hundred characters, and byte or character ranges are read without loading the rest | Large values live outside the row; `octet_length`, `left`, `substr`, `substr_bytes` read only the pages they need |
| R-T9 | SQL's NULL: three-valued logic under `NOT`, `IS DISTINCT FROM`, unique indexes that allow many NULLs | Expressions evaluate as SQL does; NULL is not a key in a unique index |

## B. Schema

| Id | Requirement | How |
|---|---|---|
| R-S1 | Create a table if absent; read back every table's definition; add a column with a default to a table that has rows; drop tables and columns. Serves both styles of migration: versioned steps, and comparing a model with the live schema | `create_table`, `describe`, `add_column`, `drop_table`, `drop_column` |
| R-S2 | Primary keys of one to four columns, of text, uuid, bytes, integers and booleans | Any column types, any count |
| R-S3 | Unique indexes: on several columns, on an expression (`lower(email)`, `lower(btrim(name))`), on part of the table (`WHERE email IS NOT NULL`); a violation names the index, so the caller can choose its message | `IndexDef` with expression keys and a filter; `DbError::constraint` |
| R-S4 | Ordered indexes on several columns, with a column descending, and on part of the table | `IndexDef::on`, `on_desc`, `filter` |
| R-S5 | Defaults: a constant, the server's clock, a random uuid, a row number that an insert may also supply itself | `ColumnDef::default`, `default_now`, `default_random_uuid`, `auto_increment` |
| R-S6 | Foreign keys that cascade, set NULL or refuse; pointing at the same table; cascading through several tables (deleting a user empties everything that was theirs) | `TableDef::foreign_key` with `OnDelete` |
| R-S7 | Checks: a value in a set, a length cap, at most one row in a table | `TableDef::check` |
| R-S8 | Schema changes inside a transaction, committed or rolled back with the rows written beside them (a migration and its bookkeeping row) | Schema changes are transactional like any write |
| R-S9 | An index over the members of a list or of a JSON array, to find the rows that hold a given member (what a GIN index on an array does) | `IndexDef::on_each` |
| R-S10 | A table that need not survive a crash, written as cheaply as possible (rate-limit counters) | `TableDef::volatile` |
| R-S11 | Add an index to a table that already has rows; a unique one fails if the rows have duplicates | `create_index` |

## C. Reads

| Id | Requirement | How |
|---|---|---|
| R-Q1 | One row by primary key or by a unique key, including a bytes key (token hashes): the hottest path of most applications | `Query::key`, `Query::index(.., KeyRange::eq(..))` |
| R-Q2 | Rows equal on some columns, further narrowed by a filter (soft-delete flags, ownership) | An index range plus `filter` |
| R-Q3 | Case-insensitive lookup | An index on `lower(col)`, read with a lower-cased key |
| R-Q4 | A range in order, forwards or backwards, with a limit; keyset pagination on text ascending, integers descending, `(timestamp, uuid)` descending and `(text, uuid)` ascending | `KeyRange`, `reverse`, `limit`; a bound may exclude the cursor row |
| R-Q5 | Offset and limit, and the count of all rows beside one page | `offset`, `limit`, `with_total` |
| R-Q6 | Rows whose key is one of a list (up to 10,000), rows whose column is or is not in a list | `Query::keys`, `index_ranges`, `in_list`, `not_in_list` |
| R-Q7 | `LIKE` and `ILIKE`: a prefix with escaping, a substring anywhere, and the user's unescaped wildcards | `like`, `ilike`, `like_escape` |
| R-Q8 | The server's clock in filters and results: `expires_at > now()`, `now() - interval`, a computed "is stale" column; one value for the whole transaction, so that the database can be the only clock | `now()`, arithmetic on timestamps |
| R-Q9 | Joins of two and three tables, inner and left, including a table with itself, with conditions on both sides | `join`, `left_join`: one lookup per row, by a key the client names |
| R-Q10 | A value from another query inside an expression: a scalar, `EXISTS` and `NOT EXISTS` reading the outer row, `IN (query)`; a permission predicate repeated across many statements | `scalar`, `exists`, `list_of` |
| R-Q11 | Aggregates: count, count of rows matching a condition, sum, min, max, average; grouped by one or two columns; filtered and ordered by the aggregate; distinct | `count`, `sum`, .. with `.filter(..)`, `group_by`, `having` |
| R-Q12 | Group by each member of a list held in the row (the addresses of a message), counting each row once per member | `group_by` on a list expression marked `each` |
| R-Q13 | JSON: a member by path, as JSON or text, negative indexes from the end; containment; key or member present, any of, all of; array length; merge, remove a key, drop nulls; build an array or object | `json`, `json_text`, `json_contains`, `json_has_any`, `json_merge`, .. |
| R-Q14 | Lists: value in a list column, two lists overlapping, joined and de-duplicated, position of a member | `in_list`, `list_overlaps`, `list_concat`, `Func::ListDistinct`, `Func::ListPosition` |
| R-Q15 | Text functions: lower, trim, length in characters and bytes, first n characters, substring, position of a substring, concatenation | `lower`, `left`, `substr`, `strpos`, `concat`, .. |
| R-Q16 | Arithmetic and the functions around it: least, greatest, ceil, power, sign, absolute value, a field of a date, seconds since the epoch (a rate limiter's window, a score) | Operators on `Expr`, `least`, `greatest`, `Func::{Ceil, Power, Sign, DatePart}` |
| R-Q17 | Full-text search: English stemming, every word required, ordered by rank, with an excerpt marking the matches; and a variant without stemming where each word matches as a prefix and a title match outranks a body match | A full-text index per table, `Query::search`, `Expr::Rank`, `Func::Headline`; `Func::Matches` for text that has no index |
| R-Q18 | How alike two texts are, by shared three-letter runs (`pg_trgm`), as a number in a score | `Func::Similarity` |
| R-Q19 | A tree in one table: a row's ancestors up to a stopping row, its descendants in depth order skipping trashed branches, whether one row is inside another's subtree | `Query::ancestors`, `descendants`, `prune`, `stop_at`, `Expr::Depth` |
| R-Q20 | Filters and sort orders built at run time from a user's choices: nested and/or to depth 10, up to ten sort keys with empty values last, a sort key looked up in another table, text read as a number or a time | Expressions are data: the application builds them; `try_cast`, `Func::ParseTimestamp`, `nulls_last`, a subquery as a sort key |
| R-Q21 | Several reads that see one moment of the database | A reading transaction |
| R-Q22 | Converting between types, failing loudly or giving NULL; a wrong-typed comparison is an error, not an empty result | `cast`, `try_cast`; comparing text with a number is `TypeMismatch` |

## D. Writes

| Id | Requirement | How |
|---|---|---|
| R-W1 | Insert a row leaving columns to their defaults, and get the stored row back (`RETURNING *`) | `Insert::returning`, `returning_all` |
| R-W2 | Insert many rows at once: 500 in a statement, 2,000 in a statement, 10,000 in a transaction, all or nothing | Several `values`; a transaction |
| R-W3 | Insert the rows another query produces | `Insert::from_query` |
| R-W4 | Upsert: on the primary key, on a composite or expression unique index; the update reads both the stored row and the proposed one (`hits + 1`, a window that restarts after a time, a list union, a lease that passes when it expires); the update may be declined by a condition, which the caller sees as no row | `on_conflict_update(index)`, `excluded(..)`, `update_where` |
| R-W5 | Insert unless present, and learn which happened | `on_conflict_do_nothing` and what `returning` gives |
| R-W6 | Update by key or by filter, many rows at once, assigning expressions, returning the new row and the old values | `Update::set`, `returning`, `old(..)` |
| R-W7 | Compare-and-swap: an update or delete conditional on the row's current state, whose row count is the answer (redeem a code once, spend a recovery code) | The filter is checked and the row written under one lock |
| R-W8 | Update or delete rows chosen through a join, returning the joined table's columns (`UPDATE .. FROM`, `DELETE .. USING`) | `Update::rows(query with joins)` |
| R-W9 | Update the first n rows in some order, as one step: lease a batch from a queue (`FOR UPDATE SKIP LOCKED`) | `Update::rows(query.order_by(..).limit(n))` |
| R-W10 | Delete by range or filter, with cascades | `Delete` |
| R-W11 | Constraint errors the caller can tell apart: unique (with the index's name), foreign key, not-null, check, out of range | `ErrorCode`, `DbError::constraint` |
| R-W12 | Allocate the next number of a counter atomically, seeding it from the data the first time | An upsert whose inserted value is `scalar(max query)` |
| R-W13 | Change a whole subtree in one step: trash it, restore what was trashed together, delete it returning what was removed, move it | `Update`/`Delete` over `descendants` |

## E. Transactions and concurrency

| Id | Requirement | How |
|---|---|---|
| R-X1 | Several statements as one: all applied at commit, none after a rollback, a dropped connection or a crash; after a failed statement the transaction cannot commit | `Client::begin`, `Transaction::commit`; dropping one rolls it back |
| R-X2 | No reader sees a change before it is committed; a writer sees the latest committed state and is not interleaved with another. This is what every `FOR UPDATE`, `FOR SHARE`, transaction-scoped advisory lock and lock-ordering rule in an application exists for | One writer at a time per database; readers on snapshots |
| R-X3 | Of several clients racing to do a once-only thing, exactly one succeeds | R-W7 under R-X2 |
| R-X4 | A commit is visible to every connection as soon as the client hears of it, because one operation may span pooled connections | Published before the reply is sent |
| R-X5 | A named lock held by a connection across statements, waited for or only tried, freed when the connection drops (migrations, maintenance that one process should do) | `Client::lock`, `try_lock` |
| R-X6 | Savepoints: undo part of a transaction after an expected failure and carry on | `Transaction::savepoint`, `rollback_to`, `release` |
| R-X7 | The clock reads the same throughout a transaction | `now()` is fixed when the transaction begins |
| R-X8 | Many connections from several processes at once (a rolling deploy runs two copies of an application for a moment); a writer that waits too long is told so | A server over TCP; `LockTimeout` |
| R-X9 | A statement that runs too long is stopped | A per-request deadline |
| R-X10 | What was committed survives a crash or power cut; what was not leaves no trace | Write-ahead log, synced before the commit is acknowledged |

## F. Operations

| Id | Requirement | How |
|---|---|---|
| R-O1 | A dump of every database as one stream on stdout, taken while the server runs and consistent as of one moment, to pipe through compression and encryption to wherever backups go; a restore from stdin into an empty server; a cut-off or damaged dump is refused. What `pg_dumpall` and `psql` do in a backup job | `database dump`, `database restore` |
| R-O2 | A database per application, each with its own password; databases created and dropped at run time (test suites make one per run) | `database create`, admin requests |
| R-O3 | A cheap health check, and the server's time | `Client::ping`, `Client::now` |
| R-O4 | A way to verify the files | `database check` |
| R-O5 | A fixed memory budget well under what a Postgres of the same job needs, and little CPU when idle | One page cache with a byte limit; no background work when nothing changes |
