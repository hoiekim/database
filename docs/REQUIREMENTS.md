# What the database has to do

This is the list the client API and the test suite are built from. It was
collected from the four apps that use Postgres today (auth, mail, doc and
budget), by reading every statement each one issues, in both its Bun
server and its Rust one. The full inventories, statement by statement with
file and line, are beside this file:

| App | Tables | Distinct statements | Inventory |
|---|---|---|---|
| auth | 21 | 171 | `requirements/auth.md` |
| mail | 11 | 107 | `requirements/mail.md` |
| doc | 26 | 191 | `requirements/doc.md` |
| budget | 23 | 185 | `requirements/budget.md` |

Each requirement below has an id, the places it comes from (`Qn` numbers
are the inventories'), and how the database meets it. The test that covers
it carries the id in its name or in a comment, so `grep -r R-W4 server/tests`
finds the proof. `docs/DESIGN.md` explains the choices this list forced.

Nothing here is SQL. The apps talk to the database through a typed Rust
client, and where Postgres did the work in a planner or a stored function,
the app now states the access path or builds the expression itself.

## What none of the apps use

No requirement, so not built: `LISTEN`/`NOTIFY`, window functions other
than a total row count, `UNION` (two cases, both trivially two queries),
sequences other than `bigserial` ids, triggers and stored functions (two
apps have a handful; each is an expression the app can build, see R-Q20
and R-Q17), views, replication, explicit isolation levels, cursors, `COPY`.

## A. Values

| Id | Requirement | From | How |
|---|---|---|---|
| R-T1 | Booleans, 64-bit integers, floats, text, bytes | all | `Value::{Bool, Int, Float, Text, Bytes}` |
| R-T2 | UUIDs: read in either letter case, printed lower-case, with a total order that keyset pagination can rely on | mail Q93–Q98, doc §4.3, budget | `Value::Uuid`, ordered by its bytes |
| R-T3 | Timestamps to the microsecond, compared and paginated exactly | mail Q94, doc Q106/Q107 (rows trashed together are found by equal timestamps) | `Value::Timestamp`, microseconds since the epoch |
| R-T4 | Calendar dates; date and timestamp text read by the server, an impossible day refused | budget §4.3, auth §4.5, doc §2.25 | `Value::Date`; text is parsed when written to a date or timestamp column |
| R-T5 | Exact decimals for money: `NUMERIC(15,2)` and `(15,6)`, rounded to scale on write, overflow refused, compared exactly (`t2.amount = -t1.amount`), accepted from floats | budget §4.4 | `Type::decimal(p, s)`; floats convert through 15 significant digits, as Postgres does |
| R-T6 | JSON documents stored whole and returned with object keys in Postgres' `jsonb` order (by length, then bytes), because responses and their ETags expose it | mail §4, doc §4.6, budget §4.5, auth §4.8 | `Value::Json`, put in that order when stored |
| R-T7 | Ordered lists of text or uuids | auth (nine `text[]` columns), doc, budget | `Value::List`, `Type::list(..)` |
| R-T8 | Large text: mail bodies of many megabytes, whose length, first 400 characters, and byte or character ranges are read without loading the rest | mail Q76, Q82, Q83, `{H}` | Large values live outside the row; `octet_length`, `left`, `substr`, `substr_bytes` read only the pages they need |
| R-T9 | SQL's NULL: three-valued logic under `NOT`, `IS DISTINCT FROM`, unique indexes that allow many NULLs | mail Q77 (a NULL `cc_text` is not matched by `NOT (cc ILIKE ..)`), mail `users.auth_sub`, doc | Expressions evaluate as SQL does; NULL is not a key in a unique index |

## B. Schema

| Id | Requirement | From | How |
|---|---|---|---|
| R-S1 | Create a table if absent; read back every table's definition; add a column with a default to a table that has rows; drop tables and columns | mail and budget migrate by comparing their models with the live schema; auth and doc by versioned steps | `create_table`, `describe`, `add_column`, `drop_table`, `drop_column` |
| R-S2 | Primary keys of one to four columns, of text, uuid, bytes, integers and booleans | mail `mail_uid_counters`, auth `recovery_codes` | Any column types, any count |
| R-S3 | Unique indexes: on several columns, on an expression (`lower(email)`, `lower(btrim(name))`), on part of the table (`WHERE email IS NOT NULL`); a violation names the index | auth §3.4 (`users_username_key` vs `users_email` choose the message), doc, mail, budget | `IndexDef` with expression keys and a filter; `DbError::constraint` |
| R-S4 | Ordered indexes on several columns, with a column descending, and on part of the table | auth `outbox_pending`, mail §6 (per-user ordered keys) | `IndexDef::on`, `on_desc`, `filter` |
| R-S5 | Defaults: a constant, the server's clock, a random uuid, a row number that an insert may also supply itself | auth `audit.id`, mail and doc `gen_random_uuid()`, every `created_at` | `ColumnDef::default`, `default_now`, `default_random_uuid`, `auto_increment` |
| R-S6 | Foreign keys that cascade, set NULL or refuse; pointing at the same table; cascading through several tables | auth §1.22 (deleting a user empties ten tables), doc §4.8, mail | `TableDef::foreign_key` with `OnDelete` |
| R-S7 | Checks: a value in a set, a length cap, at most one row in a table | auth (`kind IN (..)`, singleton `services_revision`), mail (`VARCHAR(100)` overflow fails the statement) | `TableDef::check` |
| R-S8 | Schema changes inside a transaction, committed or rolled back with the rows written beside them | auth Q164 (a migration and its bookkeeping row), mail, budget | Schema changes are transactional like any write |
| R-S9 | An index over the members of a list or of a JSON array, to find the rows that hold a given member | mail (five GIN indexes on address arrays, one on reference ids) | `IndexDef::on_each` |
| R-S10 | A table that need not survive a crash, written as cheaply as possible | auth `throttle` (`UNLOGGED`; the most written table) | `TableDef::volatile` |
| R-S11 | Add an index to a table that already has rows; a unique one fails if the rows have duplicates | mail builds 34 indexes after boot, budget pre-checks | `create_index` |

## C. Reads

| Id | Requirement | From | How |
|---|---|---|---|
| R-Q1 | One row by primary key or by a unique key, including a bytes key (token hashes) | every app's hottest path: auth Q52, Q86, Q140; mail Q17, Q27 | `Query::key`, `Query::index(.., KeyRange::eq(..))` |
| R-Q2 | Rows equal on some columns, further narrowed by a filter (soft-delete flags, ownership) | mail Q17, budget G1 | An index range plus `filter` |
| R-Q3 | Case-insensitive lookup | auth Q2, mail Q23/Q33, doc | An index on `lower(col)`, read with a lower-cased key |
| R-Q4 | A range in order, forwards or backwards, with a limit; keyset pagination on text ascending, integers descending, `(timestamp, uuid)` descending and `(text, uuid)` ascending | auth Q7/Q100/Q130, mail Q94, doc Q188, budget G2 | `KeyRange`, `reverse`, `limit`; a bound may exclude the cursor row |
| R-Q5 | Offset and limit, and the count of all rows beside one page | doc Q189, mail Q93 | `offset`, `limit`, `with_total` |
| R-Q6 | Rows whose key is one of a list (up to 10,000), rows whose column is or is not in a list | auth Q9, mail Q92, doc (30 call sites) | `Query::keys`, `index_ranges`, `in_list`, `not_in_list` |
| R-Q7 | `LIKE` and `ILIKE`: a prefix with escaping, a substring anywhere, and the user's unescaped wildcards | auth Q7, mail Q77, doc Q119 | `like`, `ilike`, `like_escape` |
| R-Q8 | The server's clock in filters and results: `expires_at > now()`, `now() - interval`, a computed "is stale" column; one value for the whole transaction | auth §4.4 (the database is the only clock), doc Q190, mail Q95 | `now()`, arithmetic on timestamps |
| R-Q9 | Joins of two and three tables, inner and left, including a table with itself, with conditions on both sides | auth Q8, Q69, Q86, Q141; mail Q75–Q80; budget transfers | `join`, `left_join`: one lookup per row, by a key the client names |
| R-Q10 | A value from another query inside an expression: a scalar, `EXISTS` and `NOT EXISTS` reading the outer row, `IN (query)` | auth Q31, Q114; mail Q88; doc's permission predicate in 17 statements | `scalar`, `exists`, `list_of` |
| R-Q11 | Aggregates: count, count of rows matching a condition, sum, min, max, average; grouped by one or two columns; filtered and ordered by the aggregate; distinct | auth Q29, Q74, Q131; mail Q75, Q103; budget | `count`, `sum`, .. with `.filter(..)`, `group_by`, `having` |
| R-Q12 | Group by each member of a list held in the row (addresses of a mail), counting each mail once per address | mail Q101 (run on every IMAP `LIST`) | `group_by` on a list expression marked `each` |
| R-Q13 | JSON: a member by path, as JSON or text, negative indexes from the end; containment; key or member present, any of, all of; array length; merge, remove a key, drop nulls; build an array or object | mail §4, doc §4.6, auth Q131 | `json`, `json_text`, `json_contains`, `json_has_any`, `json_merge`, .. |
| R-Q14 | Lists: value in a list column, two lists overlapping, joined and de-duplicated, position of a member | budget §4.6, doc Q115, auth Q156 | `in_list`, `list_overlaps`, `list_concat`, `Func::ListDistinct`, `Func::ListPosition` |
| R-Q15 | Text functions: lower, trim, length in characters and bytes, first n characters, substring, position of a substring, concatenation | mail `{H}`, doc snippets (Q118) | `lower`, `left`, `substr`, `strpos`, `concat`, .. |
| R-Q16 | Arithmetic and the functions around it: least, greatest, ceil, power, sign, absolute value, a field of a date, seconds since the epoch | auth Q133/Q134 (rate limiter), budget Q-TXN-11 | Operators on `Expr`, `least`, `greatest`, `Func::{Ceil, Power, Sign, DatePart}` |
| R-Q17 | Full-text search: English stemming, every word required, ordered by rank, with an excerpt marking the matches; and a variant without stemming where each word matches as a prefix and a title match outranks a body match | mail Q100, doc Q118 | A full-text index per table, `Query::search`, `Expr::Rank`, `Func::Headline`; `Func::Matches` for text that has no index |
| R-Q18 | How alike two texts are, by shared three-letter runs (`pg_trgm`), as a number in a score | budget Q-TXN-11 | `Func::Similarity` |
| R-Q19 | A tree in one table: a row's ancestors up to a stopping row, its descendants in depth order skipping trashed branches, whether one row is inside another's subtree | doc Q98–Q105 (every public page view walks up) | `Query::ancestors`, `descendants`, `prune`, `stop_at`, `Expr::Depth` |
| R-Q20 | Filters and sort orders built at run time from a user's choices: nested and/or to depth 10, up to ten sort keys with empty values last, a sort key looked up in another table, text read as a number or a time | doc §2.25 | Expressions are data: the app builds them; `try_cast`, `Func::ParseTimestamp`, `nulls_last`, a subquery as a sort key |
| R-Q21 | Several reads that see one moment of the database | auth Q141, budget streams | A reading transaction |
| R-Q22 | Converting between types, failing loudly or giving NULL; a wrong-typed comparison is an error, not an empty result | budget §3.5 (500s pinned by its suite), doc `cell_number` | `cast`, `try_cast`; comparing text with a number is `TypeMismatch` |

## D. Writes

| Id | Requirement | From | How |
|---|---|---|---|
| R-W1 | Insert a row leaving columns to their defaults, and get the stored row back | every app (`RETURNING *`) | `Insert::returning` |
| R-W2 | Insert many rows at once: 500 in a statement, 2,000 in a statement, 10,000 in a transaction, all or nothing | mail Q56, doc Q117, auth imports | Several `values`; a transaction |
| R-W3 | Insert the rows another query produces | auth Q67, budget, doc | `Insert::from_query` |
| R-W4 | Upsert: on the primary key, on a composite or expression unique index; the update reads both the stored row and the proposed one (`hits + 1`, a window that restarts after a time, a list union); the update may be declined by a condition, which the caller sees as no row | auth Q24, Q133; mail Q59, Q56; doc Q190 (leader lease); budget §4 | `on_conflict_update(index)`, `excluded(..)`, `update_where` |
| R-W5 | Insert unless present, and learn which happened | auth Q41, Q137; mail Q25, Q47 | `on_conflict_do_nothing` and what `returning` gives |
| R-W6 | Update by key or by filter, many rows at once, assigning expressions, returning the new row and the old values | all; doc returns the pre-update value | `Update::set`, `returning`, `old(..)` |
| R-W7 | Compare-and-swap: an update or delete conditional on the row's current state, whose row count is the answer | auth lists 18 (redeem a code once, spend a recovery code); mail Q70; doc | The filter is checked and the row written under one lock |
| R-W8 | Update or delete rows chosen through a join, returning the joined table's columns | mail Q86 (`UPDATE .. FROM`), budget (`DELETE .. USING`) | `Update::rows(query with joins)` |
| R-W9 | Update the first n rows in some order, as one step: lease a batch from a queue | auth Q123 (`FOR UPDATE SKIP LOCKED`) | `Update::rows(query.order_by(..).limit(n))` |
| R-W10 | Delete by range or filter, with cascades | housekeeping in auth, mail, doc | `Delete` |
| R-W11 | Constraint errors the caller can tell apart: unique (with the index's name), foreign key, not-null, check, out of range | auth §3.4, mail §3.5 | `ErrorCode`, `DbError::constraint` |
| R-W12 | Allocate the next number of a counter atomically, seeding it from the data the first time | mail Q59 (every delivery reserves UIDs and a MODSEQ) | An upsert whose inserted value is `scalar(max query)` |
| R-W13 | Change a whole subtree in one step: trash it, restore what was trashed together, delete it returning what was removed, move it | doc Q106–Q109 | `Update`/`Delete` over `descendants` |

## E. Transactions and concurrency

| Id | Requirement | From | How |
|---|---|---|---|
| R-X1 | Several statements as one: all applied at commit, none after a rollback, a dropped connection or a crash; after a failed statement the transaction cannot commit | all (auth lists 60 transactions, doc 25, budget 19) | `Client::begin`, `Transaction::commit`; dropping one rolls it back |
| R-X2 | No reader sees a change before it is committed; a writer sees the latest committed state and is not interleaved with another | every `FOR UPDATE`, `FOR SHARE`, transaction-scoped advisory lock and lock-ordering rule in the four apps exists for this | One writer at a time per database; readers on snapshots |
| R-X3 | Of several clients racing to do a once-only thing, exactly one succeeds | auth (five racing code redemptions give one success) | R-W7 under R-X2 |
| R-X4 | A commit is visible to every connection as soon as the client hears of it | mail §3.2 (one operation spans pooled connections), auth Q44 | Published before the reply is sent |
| R-X5 | A named lock held by a connection across statements, waited for or only tried, freed when the connection drops | migrations in all four; mail's boot maintenance | `Client::lock`, `try_lock` |
| R-X6 | Savepoints: undo part of a transaction after an expected failure and carry on | budget Q-SYS-3 | `Transaction::savepoint`, `rollback_to`, `release` |
| R-X7 | The clock reads the same throughout a transaction | doc Q106/Q107 | `now()` is fixed when the transaction begins |
| R-X8 | Many connections from several processes at once; a writer that waits too long is told so | rolling deploys run two copies of each app | A server over TCP; `LockTimeout` |
| R-X9 | A statement that runs too long is stopped | mail (30 s on every connection) | A per-request deadline |
| R-X10 | What was committed survives a crash or power cut; what was not leaves no trace | implicit in all | Write-ahead log, fsync before the commit is acknowledged |

## F. Operations

| Id | Requirement | From | How |
|---|---|---|---|
| R-O1 | A dump of every database as one stream on stdout, taken while the server runs and consistent as of one moment; a restore from stdin into an empty server; a cut-off or damaged dump is refused | `stack/backup` (`pg_dumpall` piped through gzip and encryption to object storage; restore before the apps start) | `database dump`, `database restore` |
| R-O2 | A database per app, each with its own password; databases created and dropped at run time (test suites make one per run) | `stack` services; the apps' conformance harnesses | `database create`, admin requests |
| R-O3 | A cheap health check, and the server's time | all (`SELECT 1`); mail Q95 | `Client::ping`, `Client::now` |
| R-O4 | A way to verify the files | replaces `pg_amcheck` | `database check` |
| R-O5 | A fixed memory budget well under the 256 MB Postgres has today, and little CPU when idle | goal 2 | One page cache with a byte limit; no background work when nothing changes |
