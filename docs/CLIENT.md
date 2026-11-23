# Using the client

`database-client` is how an application talks to the server. There is no
SQL: a table, a query and a write are Rust values the application builds,
and the server carries them out as written. This file is the contract:
what each piece means, and how the things usually said in SQL are said
here.

```rust
use database_client::*;

let db = Client::connect_url("database://shop:secret@database:5454").await?;
```

A `Client` is a handle on a pool of connections (8 by default, `?pool=n`
in the URL). Clone it freely. Everything that reads or writes is a method
of the `Db` trait, which `Client` and `Transaction` both implement, so a
function can take `db: &impl Db` and run either way.

## Tables

```rust
db.create_table_if_not_exists(
    TableDef::new("mails")
        .column(ColumnDef::new("mail_id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("message_id", Type::Text))
        .column(ColumnDef::new("subject", Type::Text).default(""))
        .column(ColumnDef::new("date", Type::Timestamp).default_now())
        .column(ColumnDef::new("to_address", Type::Json).nullable())
        .column(ColumnDef::new("read", Type::Bool).default(false))
        .column(ColumnDef::new("amount", Type::decimal(15, 2)).nullable())
        .primary_key(["mail_id"])
        .unique("mails_user_message", ["user_id", "message_id"])
        .index(IndexDef::new("mails_user_date").on(["user_id"]).on_desc("date").on(["mail_id"]))
        .index(IndexDef::new("mails_recipient").on(["user_id"]).on_each(col("to_address").json_pluck("address")))
        .foreign_key(["user_id"], "users", OnDelete::Cascade)
        .check("subject_length", col("subject").length().le(998)),
).await?;
```

- Every table has a primary key. A column cannot be NULL unless it is
  `.nullable()`.
- Types: `Bool`, `Int` (64-bit), `Float`, `Type::decimal(p, s)` or
  `Decimal(None)`, `Text`, `Bytes`, `Uuid`, `Timestamp` (microseconds,
  UTC), `Date`, `Json`, `Type::list(..)`.
- From Postgres: `varchar(n)` is `Text` with a `check` on its length;
  `timestamptz` is `Timestamp`; `numeric(p,s)` is `Type::decimal(p, s)`;
  `jsonb` is `Json`; `text[]` is `Type::list(Type::Text)`; `bytea` is
  `Bytes`; `inet` is `Text`; `bigserial` is `Int` with `.auto_increment()`.
- A default is evaluated per inserted row: a constant, `default_now()`,
  `default_random_uuid()`, or any expression of constants.
- `auto_increment()` numbers rows from 1. An insert may give the number
  itself; later rows continue above the highest number ever stored.
- An index key is a column or an expression (`on_expr(col("email").lower())`).
  `on_desc` stores that key descending. `on_each(list_expr)` indexes the
  row once under each distinct item of the list. `.unique()` forbids two
  rows with the same keys; rows where a key is NULL are exempt, as in SQL.
  `.filter(expr)` indexes only the rows where the expression is true.
- A text written to a `Uuid`, `Timestamp`, `Date` or decimal column is
  parsed, and refused with `TypeMismatch` if it is not one. Any other
  wrong type is refused the same way. A decimal is rounded to the
  column's scale; one too large is `OutOfRange`.
- JSON is stored whole. Object keys come back ordered by length, then by
  bytes, as Postgres' `jsonb` returns them.
- `volatile()` marks a table whose rows need not survive a restart
  (Postgres' `UNLOGGED`): its writes are cheaper, and it is empty after
  the server starts.
- `describe()` returns every table as the server holds it. `add_column`
  adds a column to a table with rows; they read its default. Schema
  changes can run inside a transaction and roll back with it.
- The primary key's columns cannot be changed by an update: delete the
  row and insert it again.

## Reading

A query names one way into the table and reads rows in that order:

```rust
Query::table("mails")                                   // every row, by primary key
Query::table("mails").key([mail_id])                    // one row by primary key
Query::table("mails").keys(ids.iter().map(|id| [*id]))  // several, in the order given
Query::table("mails").range(KeyRange::new(Bound::excluded([cursor]), Bound::Unbounded))
Query::table("mails").index("mails_user_date", KeyRange::eq([user_id]))
Query::table("mails").index("mails_user_date", KeyRange::eq([user_id])).reverse()
```

A `KeyRange` is two bounds, each a prefix of the key. On an index over
`(user_id, date, mail_id)`:

- `KeyRange::eq([u])` is every entry of user `u`;
- `KeyRange::eq([u, d, id])` is one entry;
- `KeyRange::within([u], Bound::excluded([d]), Bound::Unbounded)` is the
  entries of `u` that sort after every entry with date `d`;
- `Bound::Excluded([u, d, id])` as the lower bound starts just after that
  entry: the cursor of a keyset page. (For a descending key, "after"
  means a smaller value.)

**A NULL in a bound finds nothing**, as `key = NULL` is never true in
SQL: `KeyRange::eq([session_id])` with a missing id is an empty range,
not the rows whose id is missing, and a join on a missing key matches no
row. To read the rows whose key *is* NULL, say so: `KeyRange::is([..])`
is `IS NOT DISTINCT FROM` for each value (with a literal NULL, `IS
NULL`), and `.nulls()` does the same for any range. NULL keys sort after
all others, and a range with no upper bound runs through them.

**There is no planner.** `filter` is applied to the rows the range gives,
whatever it mentions. `Query::table("users").filter(col("email").eq(x))`
reads the whole table; give it an index range to make it fast. A text in
a range bound or compared with a uuid, timestamp, date or decimal is read
as one, as in SQL.

The steps, in order: the range's rows; `join`s; `each`; `filter`;
grouping and `having`; `order_by`; `offset` and `limit`; `select`.

- **Without `order_by`, rows come in key order and `limit` stops the
  read early.** With it, the server reads the whole range, sorts, then
  cuts: leave it out when the index's order is the one wanted.
- `select("name", expr)` and `columns([..])` choose what comes back;
  without them, every column of the query's own table. NULLs sort last
  ascending and first descending unless `.nulls_last()`/`.nulls_first()`.
  `order_by` may name a selected item.
- `with_total()` also counts the rows before `offset` and `limit`
  (`Rows::total`).
- `Query::row()` is one row of no table, for what SQL writes as a
  `SELECT` without `FROM`:
  `Query::row().select("taken", exists(..)).select("at", now())`.

### Expressions

`col("name")`, `val(x)` (or any Rust value where an expression is
expected), then methods: `eq ne lt le gt ge`, `is_null`, `is_not_null`,
`is_distinct_from`, `and`, `or`, `not`, `in_list`, `like`, `ilike`,
`between`, arithmetic operators, `lower`, `length`, `left`, `substr`,
`json`, `json_text`, `json_contains`, `cast`, `try_cast`, and so on; free
functions `all`, `any`, `coalesce`, `least`, `greatest`, `concat`, `when`,
`case`, `now`, `seconds`/`minutes`/`hours`/`days`, `list`, `call(Func::X, ..)`.
`database_protocol::expr` documents each.

- NULL is SQL's: a comparison with NULL is NULL, and a filter keeps a row
  only when its condition is true. `is_distinct_from` treats NULL as a
  value.
- Comparing values of different kinds (text with a number) is a
  `TypeMismatch` error, not "no rows". Numbers of different kinds compare
  by value. Text compares by its bytes; use `lower()` on both sides for
  case-insensitive order or equality.
- `now()` is the server's clock, fixed when the transaction begins (for a
  lone statement, when it begins). A timestamp plus or minus an integer
  is a timestamp that many microseconds away: `now() - minutes(5)`. Two
  timestamps subtracted give microseconds.
- `list([a, b]).lt(list([x, y]))` compares like SQL's `(a, b) < (x, y)`.
- `like` patterns use `%`, `_` and backslash as in SQL; `like_escape`
  makes user text literal.

### Joins and subqueries

A join is another query, run once for each row so far. It finds its rows
by a key built from that row:

```rust
Query::table("sessions").alias("s")
    .index("sessions_token_hash_key", KeyRange::eq([hash]))
    .join(Query::table("users").alias("u").key([col("s.user_id")]).filter(col("u.disabled_at").is_null()))
    .filter(col("s.expires_at").gt(now()))
    .select("id", col("s.id"))
    .select("username", col("u.username"))
```

`join` drops rows with no match; `left_join` keeps them with NULLs. The
joined query's own `filter` is the join condition; the outer `filter`
runs after all joins. Joined columns must be selected by name
(`col("u.username")`); a bare `col("x")` means the innermost table that
has `x`. With a `limit`, a join gives the first matches per row.

`scalar(query)`, `exists(query)` and `list_of(query)` put a query inside
an expression, anywhere one is allowed: a filter, a selected value, a sort
key, an inserted value, an update. They may read the enclosing row's
columns the same way.

### Aggregates

```rust
Query::table("access").index("access_service", KeyRange::eq([service]))
    .group_by([col("role")])
    .select("role", col("role"))
    .select("n", count())
    .select("recent", count().filter(col("granted_at").gt(now() - days(7))))
    .order_by(col("role"))
```

`count()`, `sum`, `min`, `max`, `avg`, `collect`, each with `.filter(..)`
and `.distinct()`. With an aggregate and no `group_by`, the whole result
is one group and always one row. `having` filters groups; `order_by` may
use aggregates or selected names. `distinct()` drops repeated result
rows.

`each("address", list_expr)` repeats a row once per distinct item of a
list computed from it, with the item as the column `address`: group by it
to count rows per item.

### Trees

For a table whose rows point at their parent's primary key:

```rust
Query::table("pages").ancestors([[page_id]], "parent_id")           // the page, its parent, ...
Query::table("pages").descendants([[page_id]], "parent_id", "pages_parent")
    .prune(col("deleted_at").is_null())      // skip trashed pages and everything under them
    .stop_at(col("id").eq(root))             // reach this row, go no further
    .select("id", col("id")).select("depth", Expr::Depth)
```

Rows come breadth first: the start rows (depth 0), then depth 1, and so
on; each row once. `descendants` needs an index whose first key is the
parent column, and reads children in that index's order.

### Full-text search

```rust
IndexDef::new("mails_search").on(["user_id"])
    .full_text(TextConfig::English, [(col("subject"), 1.0), (col("text"), 0.4)])

Query::table("mails").search("mails_search", [user_id], "invoice overdue")
    .filter(col("expunged").eq(false))
    .select("mail_id", col("mail_id"))
    .select("rank", Expr::Rank)
    .select("excerpt", call(Func::Headline, [col("text"), val("invoice overdue"), val("english"), val("<em>"), val("</em>"), val(50), val(10)]))
    .order_by(Expr::Rank.desc())
    .limit(1000)
```

A row matches when its indexed text holds every word of the query.
`TextConfig::Simple` cuts text into lower-cased runs of letters and
digits; `English` also drops common words and endings, so `invoices`
finds `invoice`. `.match_prefix()` lets each query word match longer
words. `Expr::Rank` grows with the number of matches, weighted by field.
`Func::Matches` and `Func::MatchRank` do the same on any text without an
index.

The values given to `search` are the index's leading keys (`on([..])`),
all of them: a search is always within one user's mails, one space's
pages. An index with no leading keys searches the whole table. Hits come
in primary key order unless ordered; the index is current as of every
commit, with no separate refresh.

## Writing

```rust
// Insert, taking defaults, getting the stored row back.
let row = db.insert(Insert::into("users").set("username", "ann").returning(["id", "created"])).await?.one()?;

// Several rows.
db.insert(Insert::into("words").columns(["user_id", "word", "n"]).values([u, "a".into(), 1.into()]).values(..)).await?;

// Upsert.
db.insert(
    Insert::into("throttle").set("key", key).set("window_start", now()).set("hits", 1)
        .on_conflict_update(PRIMARY)
        .update("hits", when(col("window_start").le(now() - seconds(60)), 1, col("hits") + 1))
        .update("window_start", when(col("window_start").le(now() - seconds(60)), now(), col("window_start")))
        .returning(["hits"]),
).await?;

// Compare-and-swap: the count is the answer.
let redeemed = db.update(
    Update::table("auth_codes").key([hash]).filter(col("used_at").is_null()).set("used_at", now()).returning(["client_id"]),
).await?;

// Delete, cascading, returning what was there.
db.delete(Delete::from("access").key([user, service]).returning(["role"])).await?;
```

- `Rows::affected()` is the number of rows inserted, updated or deleted.
- `returning([..])` names the columns to get back; `returning_all()` is
  SQL's `RETURNING *`; `returning_expr(name, expr)` computes one.
- **Insert.** Columns left out take their defaults. `from_query` inserts
  another query's rows. `on_conflict_do_nothing()` skips a row that
  breaks any unique index (`_on(index)`: only that one); skipped rows are
  not counted or returned. `on_conflict_update(index)` updates the
  existing row instead: `update(..)` expressions read its columns, and
  `excluded("c")` the proposed value; `update_where(cond)` declines the
  update when the condition fails for the existing row, and then nothing
  is counted or returned. The primary key's index is named `PRIMARY`.
  As in Postgres, the proposed row must be a valid row whether or not it
  ends up stored: NOT NULL and checks are tested before the conflict is
  looked for. A row that collides on the named index is handled there
  even if it also collides on another; one that collides only on another
  unique index is a `UniqueViolation`.
- **Update.** `Update::table(t)` plus `key`/`index`/`filter`, or
  `Update::rows(query)` for anything a query can express: joins, a tree,
  `order_by` and `limit` (the first n in that order). Every `set` reads
  the row as it was before the statement. `returning` reads the new row,
  `old("c")` the previous value, and joined columns by their table name.
  A row matched through several joined rows is updated once.
- **Delete.** The same shapes. `returning` reads the row before it goes.
  Children go with it or are set to NULL as their foreign keys say; a
  `Restrict` child makes the delete fail. `affected` counts the table's
  own rows.
- One statement is atomic: all of its rows, or an error and none.

Errors are `DbError { code, message, constraint, table }`:

| `ErrorCode` | When |
|---|---|
| `UniqueViolation` | A unique index or the primary key has the key already. `constraint` is the index's name (`PRIMARY` for the primary key); `err.violates("users_email")` tests it |
| `KeyTooLong` | A primary key or index key over 1,900 bytes |
| `ForeignKeyViolation` | The parent is missing, or a `Restrict` child exists |
| `NotNullViolation`, `CheckViolation` | `constraint` names the column or the check |
| `TypeMismatch`, `OutOfRange`, `Arithmetic` | A value of the wrong kind; a number too large; division by zero |
| `NotFound`, `AlreadyExists`, `InvalidRequest` | No such table, column or index; one already there; a request that makes no sense |
| `LockTimeout`, `Timeout` | Waited too long for the write lock; the statement ran too long |
| `TransactionAborted` | The transaction already failed or ended |
| `Connection` | The connection broke; whether the statement took effect is unknown |

## Transactions

```rust
let balance: Decimal = db.transaction(|tx| async move {
    let row = tx.query_one(Query::table("accounts").key([id])).await?;
    let balance: Decimal = row.get("balance")?;
    tx.update(Update::table("accounts").key([id]).set("balance", col("balance") - amount)).await?;
    tx.insert(Insert::into("ledger").set("account", id).set("amount", amount)).await?;
    Ok::<_, DbError>(balance)
}).await?;
```

or `let tx = db.begin().await?; ..; tx.commit().await?;`. Dropping a
transaction rolls it back.

- **A writing transaction has the database to itself.** `begin()` waits
  for any other writer (transaction or lone statement) to finish, then
  holds the write lock until it ends. What it reads is the latest
  committed state and cannot change under it. So there is no `FOR
  UPDATE`, no `FOR SHARE`, no `SKIP LOCKED`, no advisory transaction
  lock, no deadlock and no serialization failure to retry: read, decide,
  write. The price: keep it short, and never wait on the network inside
  one. A writer that waits longer than `lock_wait` (30 s) gets
  `LockTimeout`.
- **Readers never wait and never block.** A lone query, or `begin_read()`
  / `db.read(..)` for several queries that must see one moment, reads a
  snapshot.
- **A commit is durable and visible to every connection when `commit()`
  returns.** A lone statement likewise when its call returns.
- After a statement fails inside a transaction, the transaction can only
  be rolled back (a later statement or `commit` answers
  `TransactionAborted`), unless a savepoint was taken first:
  `tx.savepoint("s")`, then on failure `tx.rollback_to("s")` and carry
  on. `tx.release("s")` forgets it.
- `now()` reads the same throughout a transaction.

## Named locks

```rust
let guard = db.lock("shop.migrate").await?;      // waits
if let Some(guard) = db.try_lock("shop.maintenance").await? { .. }
guard.unlock().await?;
```

For work only one process should do, across transactions: migrations,
maintenance. The lock is held until `unlock`, or until the guard is
dropped or the process dies. It occupies one pooled connection.

## From SQL

| SQL | Here |
|---|---|
| `WHERE lower(email) = lower($1)` | an index `on_expr(col("email").lower())`, read with `KeyRange::eq([email.to_lowercase()])` |
| `WHERE id > $1 ORDER BY id LIMIT $2` | `.range(KeyRange::new(Bound::excluded([cursor]), Bound::Unbounded)).limit(n)` |
| `WHERE (date, id) < ($1, $2) ORDER BY date DESC, id DESC` | an index on `(.., date, id)` read `.reverse()` with upper bound `Bound::excluded([.., date, id])` |
| `col = ANY($1)` on a key | `.keys(..)` or `.index_ranges(name, values.map(KeyRange::eq))` |
| `col = ANY($1)` elsewhere | `.filter(col("c").in_list(values))` |
| `WHERE parent_id IS NULL` on an index | `KeyRange::is([.., Value::Null])` |
| `WHERE k IS NOT DISTINCT FROM $1` | `KeyRange::is([k])` |
| `SELECT EXISTS(..)`, `SELECT now()` | `Query::row().select(..)` |
| `RETURNING *` | `.returning_all()` |
| `jsonb_array_elements_text(a)` | `col("a").json_texts()`, a list: `in_list`, `each` |
| `a OR b` over two different indexes | two queries (in `db.read(..)` for one moment), merged by the caller |
| `UNION` | two queries, merged by the caller |
| `SELECT count(*) FILTER (WHERE ..)` | `count().filter(..)` |
| `SELECT ... FOR UPDATE` then `UPDATE` | the same two statements in `db.transaction(..)` |
| `UPDATE .. WHERE id IN (SELECT .. ORDER BY .. LIMIT n FOR UPDATE SKIP LOCKED) RETURNING` | `Update::rows(Query::table(..).index(..).filter(..).limit(n)).set(..).returning(..)` |
| `INSERT .. ON CONFLICT (k) DO UPDATE SET n = t.n + 1 RETURNING n` | `.on_conflict_update(index).update("n", col("n") + 1).returning(["n"])` |
| `pg_advisory_lock(k)` (session) | `db.lock(name)` |
| `pg_advisory_xact_lock(k)` | nothing: a writing transaction is already alone |
| `a @> '[{"address": "x"}]'` | an `on_each(col("a").json_pluck("address"))` index, read with `KeyRange::eq([.., "x"])`; or `col("a").json_contains(json)` as a filter |
| `WITH RECURSIVE` over `parent_id` | `ancestors` / `descendants` |
| `to_tsvector` / `@@` / `ts_rank` / `ts_headline` | a full-text index, `search`, `Expr::Rank`, `Func::Headline` |
| a stored function | a Rust function that returns an `Expr` |

## Not here

Things Postgres has that this does not, so that nobody goes looking:
regular expressions (`like`, `try_cast` and the text functions cover the
common matches); collations (text orders by bytes); `UNION`; window
functions other than `with_total`; adding a foreign key or a check to a
table that exists (declare them at creation); changing a column's type;
cancelling another connection's statement (each request has its own
deadline, `statement_timeout`); TLS.
