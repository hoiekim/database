# The wire protocol

What travels between a client and the server, byte for byte: enough to
write a client in any language without reading the Rust code. The Rust
crate `protocol/` is the implementation this describes; where the two
differ, the code is right and this file has a bug.

Three things keep them from differing:

- `protocol/fixtures/vectors.json` holds 105 named messages of
  version 2, each described in JSON and given as the bytes the Rust
  crate encodes it to. A test of that crate (`protocol/tests/vectors.rs`)
  fails when the file is no longer what the crate encodes, and another
  checks the limits this document states.
- The clients in `clients/typescript/` and `clients/python/` are written
  from this document, read the same file, and must arrive at the same
  bytes.
- Both clients run their tests against a real server in CI.

What a request *means* (what a query reads, when a transaction
conflicts) is in `CLIENT.md`. This file says how each one is written and
what the server answers.

This document describes protocol version **6**. What a version after 2
added is marked "from version N" where it is described; "Versions" lists
it all in one place. The golden vectors are of version 2, and the
clients in `clients/` greet with version 2.

## The connection

A client opens a TCP connection to the server (port 5454 unless
configured otherwise). Nothing is encrypted and nothing is negotiated:
the server is meant for a private network.

Each side sends **frames**:

```text
length    4 bytes, unsigned, little-endian: the number of payload bytes
payload   that many bytes
```

A frame's payload is one message. A client's payload is a request; the
server's is a response. The largest payload either side accepts is
268,435,456 bytes (256 MiB). The server closes the connection, without
an answer, when a frame claims more; before the greeting has succeeded
it accepts no more than 16,384 bytes.

**One request at a time.** A client sends a request and reads the whole
of its answer before it sends the next; the server reads the next frame
only after it has answered the last. Every request gets exactly one
response frame, with three exceptions: a result may come as several
`Rows` frames (see "Results in several frames"), a dump is answered with
`Chunk` frames until a final `Ok` or `Error`, and a batch is answered
with one result for each of its statements (see "Batches").

A request the server cannot decode is answered with an `Error` of code
`InvalidRequest`, and the connection carries on. An `Error` is always an
answer, never the end of the connection, except in the greeting.

## How things are written

Every structure below is its fields one after another, in the order
listed, with nothing between them and no padding. Fields are written
with these encodings:

| Name | Bytes |
|---|---|
| `u8` | one byte |
| `bool` | one byte: 0 for false, 1 for true. Any other value is refused |
| `u16`, `u32`, `u64` | 2, 4, 8 bytes, unsigned, little-endian |
| `f64` | the 8 bytes of an IEEE 754 double, little-endian |
| `i128` | 16 bytes, two's complement, little-endian |
| `varint` | an unsigned 64-bit number, 7 bits to a byte, lowest bits first; every byte but the last has its top bit set (LEB128). At most 10 bytes, the tenth being 0 or 1 |
| `zigzag` | a signed 64-bit number `n`, written as the `varint` of `(n << 1) ^ (n >> 63)` (the shift right is arithmetic): 0, -1, 1, -2 become 0, 1, 2, 3 |
| `bytes` | a `varint` length, then that many bytes |
| `str` | `bytes` that are UTF-8. Text that is not UTF-8 is refused |
| `opt(X)` | a `bool`; if true, an `X` follows |
| `list(X)` | a `varint` count, then that many `X` |

A writer uses the shortest `varint`; a reader also accepts a longer
spelling of the same number (`80 00` for 0).

A decoder checks a count or a length against the bytes left in the
frame, and refuses one that is larger: each counted item takes at least
one byte. (The one exception is the row count of a `Rows` response,
since a row of no columns takes no bytes.) Bytes left over after a
message is read are an error.

Every choice between kinds is a **tag**: one `u8`, or for a function a
`u16`. An unknown tag is refused.

**Nesting.** Expressions, queries, list values and list types may nest
100 levels deep, counted together: a query is one level, an expression
inside it the next, a subquery inside that a third. A message nested
deeper is refused. JSON text is separately held to 100 levels of arrays
and objects. Requests that hold requests (see "Requests inside
requests") are counted apart from both.

## The greeting

The first frame a client sends is a `Hello` request. It must arrive
within 10 seconds of connecting. Like every request (see "Requests") it
begins with `timeout_ms`, here unused, and its tag, 1; then:

```text
Hello: version u32, database str, password str, client str
```

- `version` is the protocol version the client speaks: the newest one
  whose messages it implements, both those it sends and those it reads
  (see "Versions"). A server answers versions 2 to 6.
- `database` is the database to use. It may be empty, with the
  administrator's password, for a connection that only administers.
- `password` is that database's password, or the administrator's, which
  opens every database and allows the admin requests.
- `client` is a name for the client. The server reads it and does not
  use it at present.

The server answers with a `Hello` response, and the connection is open:

```text
Hello: version u32, server str        the server's own version, 6, whatever the client said;
                                      and its name: "database 0.1.0"
```

or with an `Error` response, after which it closes the connection:
`ProtocolMismatch` if the client's version is below 2 or above 6
(see "Versions"), `AccessDenied` if the database does not exist or the
password is wrong (the two are not told apart), `InvalidRequest` if the
first frame was not a `Hello`.

## Requests

A request's payload is:

```text
timeout_ms varint     how long the server may spend on this request, in milliseconds;
                      0 for no limit beyond the server's own
tag        u8         which request
...                   its fields
```

| Tag | Request | Fields | Answer |
|---:|---|---|---|
| 1 | Hello | `version u32, database str, password str, client str` | `Hello`. Only as the first request; later it is `InvalidRequest` |
| 2 | Ping | | `Ok` |
| 3 | Begin | `kind u8, lock_wait_ms varint` | `Ok`. Kinds: 0 read, 1 exclusive, 2 optimistic (see "Transactions") |
| 4 | Commit | | `Ok`, or an error; either way the transaction has ended. `TransactionAborted` when none is open |
| 5 | Rollback | | `Ok`, also when no transaction is open |
| 6 | Savepoint | `name str` | `Ok` |
| 7 | RollbackTo | `name str` | `Ok` |
| 8 | Release | `name str` | `Ok` |
| 9 | Query | `Query` | `Rows` |
| 10 | Insert | `Insert` | `Rows` |
| 11 | Update | `Update` | `Rows` |
| 12 | Delete | `Delete` | `Rows` |
| 13 | Ddl | a schema change, below | `Ok` |
| 14 | Describe | | `Tables`: every table's definition |
| 15 | Lock | `name str, wait_ms varint` | `Flag`: whether the lock was taken |
| 16 | Unlock | `name str` | `Ok`, also when the lock was not held |
| 17 | Now | | `Time`: the server's clock; inside a transaction, the moment it began |
| 18 | Admin | an admin request, below | see "Administration" |
| 19 | Prepared | `id varint, statement opt(Request), params list(Value)` | What the statement answers. From version 3; see "Prepared statements" |
| 20 | BeginWith | `kind u8, lock_wait_ms varint, statement Request` | What the statement answers. From version 3; see "Beginning with a statement" |
| 21 | Batch | `items list(BatchItem)`, where `BatchItem` is `statement Request, expect opt(varint)` | One result per statement, or one `Error`. From version 3; see "Batches" |
| 22 | Explain | `statement Request` | `Rows`: one row of four ints and, to a client of version 6, a text. From version 4; see "Explain" |

In tags 19 to 22, a field of type `Request` is a request as this table
gives it, without a `timeout_ms` of its own: its `u8` tag, then its
fields. The frame's one `timeout_ms` is the deadline of every statement
in it. Which requests may stand in such a field is in "Requests inside
requests".

A server that does not know a request's tag answers `InvalidRequest`.

A statement (tags 9 to 14), a `Begin`, and tags 19 to 22 need a
connection that named a database: on a connection that named none they
are `InvalidRequest`. After the database has been dropped, or replaced
by a restore, they answer `Unavailable` on every connection that was
open before: the client reconnects.

### Schema changes

`Ddl` is a `u8` tag and its fields:

| Tag | Change | Fields |
|---:|---|---|
| 1 | CreateTable | `def TableDef, if_not_exists bool` |
| 2 | DropTable | `name str, if_exists bool` |
| 3 | AddColumn | `table str, column ColumnDef, if_not_exists bool` |
| 4 | DropColumn | `table str, column str, if_exists bool` |
| 5 | CreateIndex | `table str, index IndexDef, if_not_exists bool` |
| 6 | DropIndex | `table str, index str, if_exists bool` |

Without the flag, creating what exists is `AlreadyExists` and dropping
what does not is `NotFound`; with it, both are `Ok` and change nothing.

### Prepared statements

From version 3. A connection keeps statements under numbers its client
chooses, so that a statement run often is sent and decoded once and
afterwards named by its number.

```text
Prepared: id varint, statement opt(Request), params list(Value)
```

- `id` is the client's number for the statement: any `varint`. Numbers
  belong to the connection; another connection, or the same client
  after a reconnect, keeps nothing under them.
- **With `statement`**, the connection keeps the statement under `id`,
  in place of any it kept under that number, then runs it. The
  statement is a `Query`, `Insert`, `Update` or `Delete` (tags 9 to 12);
  any other request is `InvalidRequest`. So is a statement over 65,536
  bytes, counted as the payload it would be sent as on its own with a
  `timeout_ms` of 0 (one byte for `timeout_ms`, then its tag and fields):
  such a statement is sent as an ordinary request instead. After either
  refusal nothing has run, and what the connection kept under `id` is
  still kept. A statement that is kept stays kept whatever its run
  answers.
- **Without `statement`**, the connection runs the statement it keeps
  under `id`. If it keeps none, the answer is `NotPrepared` and nothing
  has run; the client sends the same request again, this time with the
  statement.
- `params` are the values the statement is run with, sent with every
  run, with the statement or without it. An expression `Param` (tag 22)
  with number `n` reads `params[n]`, counting from 0 (see "Expressions").
- The answer is what the statement answers when sent as it is, the
  `Param`s read as the values given: `Rows`, or an `Error`. A value is
  checked where it is used, as one written into the statement would be.
- A connection keeps at most 256 statements, and at most 1,048,576 bytes
  of them together, each counted as above. To keep one more, it first
  lets go of the ones used longest ago, a use being any `Prepared` that
  sent or named it, wherever it stood. The client is not told: it learns
  of it from a `NotPrepared`.
- Inside a transaction, the statement runs in the transaction, as any
  statement does. `NotPrepared` and the two refusals above come before
  anything runs: they do not abort the transaction.

### Beginning with a statement

From version 3. `BeginWith` is a `Begin` and the transaction's first
statement in one request.

```text
BeginWith: kind u8, lock_wait_ms varint, statement Request
```

- `kind` and `lock_wait_ms` are those of `Begin` (see "Transactions").
- `statement` is one a transaction runs: a `Savepoint`, `RollbackTo`,
  `Release`, `Query`, `Insert`, `Update`, `Delete`, `Ddl`, `Describe` or
  `Now` (tags 6 to 14 and 17); a `Prepared` (tag 19); or an `Explain`
  (tag 22, see "Explain").
- The answer is what the statement answers, as if `Begin` had been sent
  and answered `Ok`, then the statement.

A client takes it that the connection holds a transaction after a
`BeginWith`, whatever it answered, and ends it with `Commit` or
`Rollback`. The server makes that so:

- **If the transaction began and the statement ran**, the transaction
  is as `Begin` and then the statement would leave it. A statement that
  failed leaves a writing transaction aborted (see "Transactions"); a
  reading transaction carries on.
- **If the transaction could not begin, or the statement was refused
  before it ran**, the answer is that error and the connection holds a
  dead transaction. The transaction could not begin when the wait for
  the write lock ran out (`LockTimeout`), the database is gone
  (`Unavailable`), or the connection named no database
  (`InvalidRequest`). The statement was refused when it is not one of
  those listed above, when a `Prepared` with its statement was refused
  (see "Prepared statements"), or when what is explained is not a
  query. All are `InvalidRequest`. In a dead transaction every statement
  answers `TransactionAborted`, where the connection has its database:
  on a connection that named none, or whose database is gone, a
  statement is answered `InvalidRequest` or `Unavailable` as it is
  outside any transaction, since the database is looked for first.
  `Commit` answers the error the `BeginWith` answered and ends the
  transaction; `Rollback` answers `Ok` and ends it.
  So nothing the client sends next runs outside the transaction it was
  meant for.

Two answers leave no transaction behind, and nothing has begun:

- `NotPrepared`, when the statement is a `Prepared` without its
  statement, or an `Explain` of one, that the connection does not keep.
  The client sends the same `BeginWith` again with the statement.
- `InvalidRequest` on a connection that already holds a transaction.
  The transaction it holds is as it was.

### Batches

From version 3. A batch is a transaction sent whole: statements that
need nothing from each other's answers, run in one request.

```text
Batch:      items list(BatchItem)
BatchItem:  statement Request, expect opt(varint)
```

- Each `statement` is a `Query`, `Insert`, `Update` or `Delete`, or a
  `Prepared` of one. Any other request is `InvalidRequest`, as is a
  batch of no statements.
- A batch is a transaction of its own. On a connection that holds a
  transaction it is `InvalidRequest`, and the transaction it holds is as
  it was.
- Before any statement runs, the server finds every `Prepared` of the
  batch, in order. One it does not keep is `NotPrepared`, and one it
  refuses is `InvalidRequest`, with nothing run: the client sends the
  batch again, with the statement. Statements sent with their
  `Prepared` before that one are kept.
- The statements run in order. Each sees what those before it did, and
  nothing else is written between them.
- **`expect`**, when present, is the number of rows the statement must
  affect for the batch to go on: its result's `affected`, the rows it
  inserted, updated or deleted, or for a query the rows it returned.
  Any other number stops the batch with `Unmet`.
- **When every statement succeeds**, the answer is each statement's
  result, in order: for each, one or more `Rows` frames, the last with
  `more` clear (see "Results in several frames"). A client reads as many
  results as it sent statements. A batch that wrote is on disk when it
  is answered.
- **When a statement fails, or its `expect` is not met**, the batch
  stops there, nothing of it takes effect, and the answer is one `Error`
  frame: the statement's own error, or `Unmet`. The message names the
  statement by its place in the batch, counting from 1.
- A batch of queries alone reads one moment of the database and holds
  no writer up. A batch that writes waits for the write lock as a lone
  write does (see "Time limits"), then holds it until it is answered.

### Explain

From version 4. `Explain` runs a query and answers with what it read in
place of its rows, and from version 6 with the way into each table it
read.

```text
Explain: statement Request
```

- `statement` is a `Query`, or a `Prepared` whose statement is a query.
  Any other request is `InvalidRequest`, and nothing runs; a `Prepared`
  sent with an insert, an update or a delete as its statement keeps that
  statement all the same. A `Prepared` without its statement that the
  connection does not keep is `NotPrepared`.
- The query runs as it would if it were sent alone: in the connection's
  transaction if it holds one, else on its own. A query that fails
  answers with its error, and in a transaction counts as a failed
  statement.
- The answer is one `Rows` frame: `more` false, `affected` 1, no
  `total`, the columns `index_entries`, `rows_read`, `rows_returned` and
  `micros`, and one row of four ints. To a client that greeted with
  version 6 or more the frame has a fifth column, `way`, and the row a
  fifth value, a text:

| Column | |
|---|---|
| `index_entries` | Entries read from indexes: those of an ordered index's range, the children a tree walk found, a search's hits |
| `rows_read` | Rows read from tables: in key order, through an index, by a join or a subquery |
| `rows_returned` | The rows the query returned: its `affected` |
| `micros` | Microseconds from when the server began on the request to when the query finished. For a `BeginWith`, the wait to begin is included |
| `way` | From version 6. The way into each table the query read: see below |

- `way` is for a person to read. It is a list of parts joined by `"; "`,
  each a table's name, quoted and escaped as Rust's `{:?}` writes a
  string, then ` by ` and the way into it:

| The way in | Written |
|---|---|
| The primary key | `its primary key` |
| An ordered index | `index "name"` |
| A tree walk | `a tree walk` |
| A full-text search | `a search of "name"` |
| Every row | `the whole table` |
| A tree read in its own order, for a query sorted so | `its primary key, in its order` or `index "name", in its order` |

  The first part is the query's own way in, if it names one (see
  "Queries"). Then comes one part for each way in the server chose,
  which ends with `, chosen`: for the query itself if it names none, and
  for each query inside it that names none and was run (a join's, a
  subquery's), each distinct choice once, in the order first made, at
  most eight. The whole table is such a choice: it is what the server
  reads where nothing narrower is sure to answer the same. A query of
  no table that chose for nothing has an empty `way`. Examples:

```text
"mails" by index "mails_user_date"
"mails" by index "mails_user_date", in its order, chosen
"mails" by the whole table, chosen
"sessions" by index "sessions_token"; "users" by its primary key, chosen
```

### Requests inside requests

Requests 19 to 22 hold requests. A decoder takes at most three levels of
requests inside the frame's own, and refuses a frame that holds more as
`InvalidRequest`. Which requests the server runs where:

| In | May stand |
|---|---|
| `Prepared`'s `statement` | `Query`, `Insert`, `Update`, `Delete` |
| `BeginWith`'s `statement` | a statement (tags 6 to 14 and 17), `Prepared`, `Explain` |
| `BatchItem`'s `statement` | `Query`, `Insert`, `Update`, `Delete`, `Prepared` |
| `Explain`'s `statement` | `Query`, `Prepared` |

The deepest that runs is a `BeginWith` of an `Explain` of a `Prepared`
with its `Query`: three levels inside the frame's own. A request that
decodes but stands where it may not is `InvalidRequest`.

## Responses

A response's payload is a `u8` tag and its fields:

| Tag | Response | Fields |
|---:|---|---|
| 1 | Ok | |
| 2 | Hello | `version u32, server str` |
| 3 | Rows | `more bool, columns list(str), affected varint, total opt(varint), rows` |
| 4 | Tables | `list(TableDef)` |
| 5 | Flag | `bool` |
| 6 | Time | `zigzag`: microseconds since 1970-01-01T00:00:00Z |
| 7 | Names | `list(str)` |
| 8 | Text | `str` |
| 9 | Chunk | `bytes` |
| 10 | Error | `code u8, message str, constraint opt(str), table opt(str)` |

In `Rows`, `rows` is a `varint` count of rows, then each row as one
`Value` per column, in the order of `columns`, with nothing between rows.

- `columns` names what each row holds. A write without `returning`
  answers with no columns and no rows.
- `affected` is the number of rows a write inserted, updated or deleted,
  whether or not it returned them; for a query, the number of rows
  returned.
- `total` is present for a query that asked `with_total`: the rows there
  are before `offset` and `limit`.

### Results in several frames

A result too large for one frame comes as several `Rows` frames, cut
when a frame's rows pass about 4 MiB. Every frame repeats `columns`,
`affected` and `total`; every frame but the last has `more` set. The
client reads frames until one has `more` clear, and the result is all
their rows in order. A row is never split, so one row with a very large
value makes one large frame.

### Errors

An `Error` carries a code, a message for people, and for the constraint
errors the name of what was broken and its table.

| Code | Name | When |
|---:|---|---|
| 1 | UniqueViolation | A unique index or the primary key already holds the key. `constraint` is the index's name, `primary` for the primary key |
| 2 | ForeignKeyViolation | A row points at a parent that is not there, or a parent still has children that forbid its removal. `constraint` is the foreign key's name |
| 3 | NotNullViolation | NULL in a column that does not allow it. `constraint` is the column |
| 4 | CheckViolation | A row fails one of its table's checks. `constraint` is the check's name |
| 5 | TypeMismatch | A value of the wrong type for its column or for an operator |
| 6 | NotFound | No such database, table, column or index |
| 7 | AlreadyExists | A database, table, column or index of that name is already there |
| 8 | InvalidRequest | The request makes no sense as written, or could not be decoded |
| 9 | KeyTooLong | A primary key or index key over 1,900 bytes |
| 10 | LockTimeout | Waited too long for the write lock |
| 11 | Timeout | The statement ran past its deadline |
| 12 | TransactionAborted | The transaction already failed or ended |
| 13 | AccessDenied | Wrong name or password, or no right to the request |
| 14 | ProtocolMismatch | The server does not speak the protocol version the client greeted with |
| 15 | Connection | The connection broke. A client does not receive this one: clients raise it themselves, for a connection that failed |
| 16 | Io | The disk failed under the server |
| 17 | Corrupt | The server found its own files damaged |
| 18 | Unavailable | The server cannot take the request now; the database was dropped or replaced |
| 19 | Internal | A bug in the server |
| 20 | Arithmetic | Arithmetic that has no answer: division by zero, overflow |
| 21 | OutOfRange | A number too large for its column |
| 22 | Conflict | An optimistic transaction was told something that a commit made beside it has since changed. Nothing of it took effect |
| 23 | NotPrepared | From version 3. A `Prepared` without its statement named a number the connection does not keep. Nothing was run: send it again with the statement (see "Prepared statements") |
| 24 | Unmet | From version 3. A statement of a batch did not affect the number of rows its `expect` gave. Nothing of the batch took effect (see "Batches") |

A client reads a code it does not know as `Internal` (19), which is
what the Rust decoder does: a newer server's new code is then an error
of no particular kind and not a failure to decode.

## Values

A `Value` is a `u8` tag and its body:

| Tag | Kind | Body | |
|---:|---|---|---|
| 0 | NULL | | |
| 1 | false | | |
| 2 | true | | |
| 3 | int | `zigzag` | A signed 64-bit integer |
| 4 | float | `f64` | NaN and the infinities included |
| 5 | decimal | `scale u8, mantissa i128` | The number `mantissa / 10^scale`. `scale` is at most 30; the one mantissa that cannot be negated (-2^127) is refused |
| 6 | text | `str` | |
| 7 | bytes | `bytes` | |
| 8 | uuid | 16 bytes | In the order they are printed |
| 9 | timestamp | `zigzag` | Microseconds since 1970-01-01T00:00:00Z. There are no time zones |
| 10 | date | `zigzag` | Days since 1970-01-01; must fit 32 bits |
| 11 | json | `str` | The JSON text. Any valid JSON text is read; the server writes it without spaces |
| 12 | list | `list(Value)` | |

- **bool** has no body: the tag is the value.
- **decimal** keeps its scale: `5.00` is mantissa 500, scale 2, and is
  not the same bytes as `5` (mantissa 5, scale 0), though the two
  compare equal. A column of type `decimal(p, s)` stores every value
  with scale `s`.
- **json** travels as text. What a `json` column returns is the stored
  form, in which object keys are ordered by length, then by bytes, and
  no key appears twice. A JSON number that is not an integer of 64 bits
  is a double.
- A **timestamp** has the range of a signed 64-bit count of
  microseconds, about 292,000 years either side of 1970; a **date**, of
  a signed 32-bit count of days. Both go beyond what most languages'
  date types hold, so a client needs a way to carry the raw number.

Where a text is written to a `uuid`, `timestamp`, `date` or decimal
column, or compared with one, the server reads it as one (`CLIENT.md`,
"Tables" and "Reading"). An int written to a float or decimal column is
widened. Any other mismatch of kinds is `TypeMismatch`.

## Types

A column's `Type` is a `u8` tag:

| Tag | Type | Body |
|---:|---|---|
| 1 | bool | |
| 2 | int | |
| 3 | float | |
| 4 | decimal, of any precision and scale | |
| 5 | text | |
| 6 | bytes | |
| 7 | uuid | |
| 8 | timestamp | |
| 9 | date | |
| 10 | json | |
| 11 | list | the `Type` of its items |
| 12 | decimal(p, s) | `precision u8, scale u8`, with `scale <= precision` and `scale <= 30` |

## Expressions

An `Expr` is a `u8` tag and its body. The fields are listed in the order
they are written, which is not always the order of the Rust struct.

| Tag | Expression | Body | |
|---:|---|---|---|
| 1 | Value | `Value` | A constant |
| 2 | Column | `table opt(str), name str` | A column, optionally of one named table (or alias) of the query. Without a table, the innermost table that has the column |
| 3 | Excluded | `name str` | In an upsert's update: the value the insert proposed |
| 4 | Old | `name str` | In an update's `returning`: the column as it was before |
| 5 | Depth | | In a tree walk: how many steps from a start row |
| 6 | Rank | | In a full-text search: how well the row matches |
| 7 | Not | `Expr` | |
| 8 | And | `list(Expr)` | True when every part is; an empty list is true |
| 9 | Or | `list(Expr)` | True when any part is; an empty list is false |
| 10 | Cmp | `op u8, left Expr, right Expr` | A comparison |
| 11 | IsNull | `negated bool, Expr` | `IS NULL`; `IS NOT NULL` when negated |
| 12 | In | `negated bool, value Expr, list Expr` | Whether the value equals an item of the list |
| 13 | Like | `case_insensitive bool, negated bool, value Expr, pattern Expr` | SQL `LIKE`: `%`, `_`, and a backslash to escape |
| 14 | Arith | `op u8, left Expr, right Expr` | |
| 15 | Neg | `Expr` | |
| 16 | Call | `func u16, args list(Expr)` | A function, below |
| 17 | Case | `branches list(condition Expr, value Expr), otherwise opt(Expr)` | The first branch whose condition is true, else `otherwise`, else NULL |
| 18 | Cast | `lenient bool, Type, Expr` | Converts to the type; when lenient, a value that cannot be converted gives NULL instead of an error |
| 19 | List | `list(Expr)` | A list of the items' values. Lists compare item by item, as SQL row values do |
| 20 | Agg | `func u8, distinct bool, arg opt(Expr), filter opt(Expr)` | An aggregate, over the rows where `filter` is true |
| 21 | Subquery | `kind u8, Query` | A query inside an expression |
| 22 | Param | `n varint` | From version 3. The value `params[n]` of the `Prepared` request the statement is run by, counting from 0 (see "Prepared statements"). `n` must fit 32 bits |

A `Param` is evaluated as any expression is: one whose number is past
the end of `params`, or in a statement not run by a `Prepared` (which
has no `params`), is `InvalidRequest` when it is evaluated. A column's
default, a check and an index cannot hold one: the definition is
`InvalidRequest`.

Comparison operators (`Cmp`):

| 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|---|
| `=` | `<>` | `<` | `<=` | `>` | `>=` | `IS DISTINCT FROM` | `IS NOT DISTINCT FROM` |

Arithmetic operators (`Arith`): 0 add, 1 subtract, 2 multiply, 3 divide,
4 remainder. A timestamp plus or minus an int is a timestamp that many
microseconds away; two timestamps subtracted give microseconds.

Aggregates (`Agg`): 0 count (rows, or non-NULL values when given an
argument), 1 sum, 2 min, 3 max, 4 avg, 5 collect (the values as a list,
in the order the rows were read), 6 bool_or, 7 bool_and.

Subqueries: 0 scalar (the first column of the first row, or NULL), 1
exists (whether there is any row), 2 list (the first column of every
row, as a list).

NULL is SQL's: a comparison with NULL is NULL, `And` and `Or` use
three-valued logic, and a filter keeps a row only when it is true.

### Functions

`Call` names its function with a `u16`. Unless said otherwise, a NULL
argument gives NULL. The server checks the number and kinds of the
arguments when the expression is evaluated, not when it is decoded.

| Number | Function | |
|---:|---|---|
| 1 | `lower(text)` | |
| 2 | `upper(text)` | |
| 3 | `length(text)` | In characters |
| 4 | `octet_length(value)` | In bytes, of text or bytes. Does not load a large value |
| 5 | `trim(text)` | Without leading and trailing spaces |
| 6 | `left(text, n)` | The first n characters |
| 7 | `substr(text, start, length)` | Counting characters from 1 |
| 8 | `substr_bytes(value, start, length)` | Counting bytes from 1; gives bytes |
| 9 | `strpos(text, needle)` | Where the needle first occurs, counting characters from 1, or 0 |
| 10 | `concat(...)` | All arguments joined. NULL if any is NULL |
| 11 | `replace(text, from, to)` | |
| 12 | `hex(bytes)` | Lower-case hex |
| 13 | `similarity(a, b)` | From 0 to 1, by the three-letter runs two texts share |
| 20 | `abs(x)` | |
| 21 | `ceil(x)` | |
| 22 | `floor(x)` | |
| 23 | `round(x)`, `round(x, digits)` | |
| 24 | `power(x, y)` | |
| 25 | `least(...)` | The smallest argument, skipping NULLs |
| 26 | `greatest(...)` | |
| 27 | `sign(x)` | -1, 0 or 1 |
| 30 | `coalesce(...)` | The first argument that is not NULL |
| 31 | `nullif(a, b)` | NULL if the two are equal, else the first |
| 40 | `now()` | The server's clock when the transaction began, or for a statement outside one, when the statement began |
| 41 | `parse_timestamp(text[, offset_seconds])` | NULL if the text is not a point in time |
| 42 | `random_uuid()` | A fresh random uuid (version 4) |
| 43 | `date_part(part, value)` | `year`, `month`, `day`, `hour`, `minute`, `second`, `dow` (Sunday is 0) or `epoch`, of a timestamp (in UTC) or a date |
| 50 | `json_get(json, path...)` | The member, as JSON; NULL if absent. A path is text for an object key, an int for an array index, negative ones counting from the end |
| 51 | `json_get_text(json, path...)` | The member as text: a string without its quotes, anything else as its JSON text. NULL if absent or JSON null |
| 52 | `json_typeof(json)` | `object`, `array`, `string`, `number`, `boolean` or `null` |
| 53 | `json_len(json)` | Items in an array. NULL for anything else |
| 54 | `json_contains(a, b)` | Postgres' `a @> b` |
| 55 | `json_has_key(json, text)` | Whether the text is a key of the object, or an item of the array |
| 56 | `json_has_any(json, texts)` | The same for any of a list of texts |
| 57 | `json_has_all(json, texts)` | |
| 58 | `json_merge(a, b)` | Two objects merged, the second winning; two arrays joined |
| 59 | `json_strip_nulls(json)` | Without object members whose value is null, at every depth |
| 60 | `json_remove(json, key)` | The object without that key |
| 61 | `json_elems(json)` | An array's items as a list of JSON values. An empty list for NULL or anything not an array |
| 62 | `json_pluck(array, key)` | The text of that key in each object of the array, as a list, skipping objects without it. An empty list for NULL |
| 63 | `json_array(...)` | A JSON array of the arguments |
| 64 | `json_object(key, value, ...)` | A JSON object from alternating keys and values |
| 65 | `to_json(value)` | Any value as JSON |
| 66 | `json_texts(json)` | An array's items as a list of text: a string as it is, a null as NULL, anything else as its JSON text |
| 80 | `list_len(list)` | |
| 81 | `list_overlaps(a, b)` | Whether two lists share an item |
| 82 | `list_concat(a, b)` | One list after the other. A NULL argument counts as empty |
| 83 | `list_distinct(list)` | Each distinct item once, in order of first appearance |
| 84 | `list_position(list, item)` | Counting from 1, or NULL |
| 90 | `matches(text, query, config, prefix)` | Whether the text holds every word of the query. `config` is `simple` or `english`; with `prefix` true each query word matches words that start with it |
| 91 | `match_rank(text, query, config, prefix)` | How often |
| 92 | `headline(text, query, config, start, stop, max_words, min_words)` | An excerpt around the matches, each wrapped in `start` and `stop` |

## Queries

```text
Query:
  table       str
  alias       opt(str)       what the table is called in expressions
  access      Access         the way in, and the range of keys to read
  joins       list(Join)
  unnest      opt(Select)    a list computed per row; the row is seen once per distinct item,
                             the item readable as a column of the given name
  filter      opt(Expr)
  group_by    list(Expr)
  having      opt(Expr)
  order_by    list(Order)
  offset      varint
  limit       opt(varint)
  select      list(Select)   empty for every column of `table`
  distinct    bool
  with_total  bool           also count the rows before offset and limit

Select:  name str, expr Expr
Join:    kind u8 (0 inner, 1 left), query Query
Order:   expr Expr, desc bool, nulls u8 (0 the default, 1 NULLs first, 2 NULLs last)
```

The server reads the rows `access` names, in key order, then applies
the rest in a fixed sequence: joins (each run once per row so far), the
unnest, the filter, grouping and `having`, the sort, offset and limit,
the select. A table named `""` is one row of no table, for a query that
only computes. The default null order puts NULLs last ascending and
first descending.

**What a query answers is fixed by that sequence. What the server reads
to answer it is fixed only where the query names its way in.** A query
names no way in when its `access` is `Primary` with `reverse` false and
one range whose two bounds are `Unbounded`: every row, which is what a
client's query is until it is narrowed. For such a query, and for each
such query inside another (a join's, a subquery's, an insert's source,
the `rows` of an update or a delete), the server may read a range of
the primary key or of an index in place of every row, where the query's
filter gives one. It does so only where the answer is the one the
sequence above gives over every row: the same rows in the same order,
or the same error. A client can therefore not tell, except by `Explain`
and by the time taken. Any other `access` is read as it is written:
`Whole` (from version 6) is every row and nothing else, and so is a
`Primary` over every key that is reversed. A server may be set to choose
for no query; it then reads every row for one that names no way in.

```text
Access, a u8 tag:
  1 Primary: reverse bool, ranges list(KeyRange)            rows by primary key
  2 Index:   index str, reverse bool, ranges list(KeyRange) rows through an index, in its order
  3 Walk:    start list(list(Expr)), parent str, direction u8 (0 up, 1 down), index opt(str),
             prune opt(Expr), stop opt(Expr), max_depth u32
  4 Search:  index str, prefix list(Expr), query Expr, match_prefix bool
  5 Whole:   nothing more                                   every row, by primary key. From version 6

KeyRange:  lower Bound, upper Bound, nulls bool
Bound, a u8 tag:
  0 Unbounded
  1 Included: list(Expr)
  2 Excluded: list(Expr)
```

- A **bound** is a prefix of the key: its values stand for the key's
  leading parts. "Every row" is one range with both bounds unbounded;
  "the row with this key" is one range whose two bounds are `Included`
  with the same values. Several ranges are read one after another.
- A bound that holds a NULL names no key, and the range is empty,
  unless `nulls` is set: then a NULL in a bound stands for the keys that
  are NULL.
- **Walk** reads a tree held in one table. `start` is the primary keys
  of the start rows; `parent` is the column holding the parent's primary
  key. Going down needs `index`, an index whose first key is that
  column. A row for which `prune` is false is not reached, nor anything
  beyond it; one for which `stop` is true is reached but not followed.
  The Rust client sends a `max_depth` of 1000 unless told otherwise.
- **Search** reads the rows whose indexed text holds every word of
  `query`. `prefix` is the values of the full-text index's leading keys,
  all of them.
- **Whole** reads every row of the table in primary key order, as a
  `Primary` over every key does, and is never replaced by a range of the
  server's choosing. From version 6; a server older than that refuses
  the frame as malformed.

## Writes

```text
Insert:
  table        str
  columns      list(str)      the columns the rows give values for
  source       u8 tag:  0 Values: list(list(Expr))      one list per row, in the order of `columns`
                        1 Query:  Query                 one row per row of the query
  on_conflict  u8 tag:  0 Error
                        1 DoNothing: target opt(str)    skip a row that conflicts on the named unique
                                                        index, or on any when none is named
                        2 DoUpdate:  target str, set list(Assign), filter opt(Expr)
  returning    list(Select)

Update:  rows Query, set list(Assign), returning list(Select)
Delete:  rows Query, returning list(Select)
Assign:  column str, value Expr
```

- The primary key's index is named `primary` wherever an index name is
  asked for.
- An update or a delete changes the rows its query finds, in the
  query's own table; joins only help choose rows and supply values.
- `returning` names what to send back for each row. A select named `*`
  whose expression is the column `*` (tag 2, no table, name `*`) stands
  for every column of the row.

## Definitions

```text
TableDef:
  name          str
  columns       list(ColumnDef)
  primary_key   list(str)             column names
  indexes       list(IndexDef)
  foreign_keys  list(ForeignKey)
  checks        list(Check)
  volatile      bool                  rows need not survive a restart

ColumnDef:
  name         str
  type         Type
  nullable     bool
  default      opt(Expr)
  flags        u8                     bit 0: auto_increment; bit 1: keep_start follows. No other bit
  keep_start   varint                 only when bit 1 is set: 1 to 1,024

IndexDef:
  name     str
  keys     list(IndexKey)
  unique   bool
  filter   opt(Expr)                  only rows for which this is true are in the index
  kind     u8 tag:  0 Ordered
                    1 FullText: config u8 (0 simple, 1 english), fields list(TextField)

IndexKey:    expr Expr, desc bool, each bool
TextField:   expr Expr, weight f64
ForeignKey:  name str, columns list(str), ref_table str, ref_columns list(str), on_delete u8
Check:       name str, expr Expr
```

- An index key is a column (an `Expr` of tag 2) or any expression over
  the row. With `each`, the expression gives a list and the row is
  indexed once under each distinct item.
- A column's `flags` byte is, from version 5, what was the bool
  `auto_increment` in versions 2 to 4. A column without `keep_start` has
  `flags` 0 or 1, the same byte in every version. A decoder refuses a
  byte with any other bit set, and a `keep_start` of 0 or of more than
  32 bits.
- `keep_start` is how many bytes of the start of a large value the
  column keeps in its row (`CLIENT.md`, "Tables"). It is for a text or
  bytes column; on any other type, or above 1,024, the definition is
  `InvalidRequest`. A server describes a column's `keep_start` only to a
  client that greeted with version 5 or more (see "Versions").
- `on_delete` is 0 restrict, 1 cascade, 2 set NULL.
- `ref_columns` may be sent empty, for the parent's primary key. The
  server fills it in: `Describe` returns the parent's key columns there.
- The Rust client names a foreign key `<table>_<columns joined by _>_fkey`.
  The name is the client's to choose; it is what a `ForeignKeyViolation`
  reports.

The same encoding of `TableDef` is what `Describe` returns, so a client
decodes definitions, and with them expressions and queries, as well as
encoding them.

## Transactions

Outside a transaction, every statement is a transaction of its own: it
takes effect whole or not at all, and is on disk when its answer
arrives.

`Begin` opens a transaction on the connection, of one of three kinds;
`BeginWith` opens one and runs its first statement (see "Beginning with
a statement"). The connection carries at most one; a `Begin`, a
`BeginWith` or a `Batch` inside one is `InvalidRequest`, and leaves the
transaction as it was. A `Batch` is a transaction of its own (see
"Batches").

| Kind | | |
|---:|---|---|
| 0 | read | Sees the database as of the moment it began. Writes inside it are `InvalidRequest` |
| 1 | exclusive | The database's only writer from `Begin` to its end. `Begin` waits up to `lock_wait_ms` for the write lock, then answers `LockTimeout` |
| 2 | optimistic | May write, and runs beside other writers on a copy of its own. At `Commit` the server checks that no commit made meanwhile changed anything it was told. It waits up to `lock_wait_ms` for its turn to commit |

Statements sent while it is open run inside it and see each other's
effects. `Commit` or `Rollback` ends it. If the connection closes first,
it is rolled back.

- **`Commit` always ends the transaction.** `Ok` means its changes are
  on disk and visible to every connection. An error means it was rolled
  back whole. After either, the connection is outside any transaction.
- **A statement that fails** inside a writing transaction leaves it
  aborted: every later statement, and `Commit`, answers
  `TransactionAborted`, until a `Rollback`, or a `RollbackTo` a
  savepoint taken before the failure. In a reading transaction a failed
  statement changes nothing and the transaction carries on.
- **`Conflict`** is how an optimistic transaction fails: its `Commit`,
  or an earlier statement of it, answers with that code when a commit
  made beside it changed something it had been told. Nothing of it took
  effect. The client sends `Rollback` if the transaction is still open
  (a `Commit` that answered has ended it), and runs the work again from
  the start.
- **Savepoints** (`Savepoint`, `RollbackTo`, `Release`) belong to
  writing transactions. `RollbackTo` undoes everything since the
  savepoint, which stays in place, and clears the aborted state.

What the Rust client does, and the TypeScript and Python clients after
it, for "run this work in a transaction":

1. Begin an optimistic transaction and run the work.
2. If the work succeeded, `Commit`. On `Ok`, done. On `Conflict`, go to 4.
   On any other error, fail with it.
3. If the work failed, `Rollback`. If the server had answered `Conflict`
   to any request of the transaction, go to 4; otherwise fail with the
   work's error.
4. Do it once more from step 1. After two optimistic runs, begin an
   exclusive transaction instead and run the work in it, once: it cannot
   conflict.

The work may therefore run up to three times, and should do nothing but
talk to the database.

## Time limits

- **`timeout_ms`** on a request is the statement's deadline. A statement
  that runs past it is stopped and undone, and answers `Timeout`; the
  connection carries on. The Rust client sends 60,000 unless configured
  otherwise. 0 asks for no limit.
- A lone write, and a batch that writes, waits for the database's write
  lock for the server's own limit (30 seconds unless configured), then
  answers `LockTimeout`. A batch's `timeout_ms` is one deadline for all
  its statements together.
- **A client waits a little longer than it asked the server to:** the
  server enforces the deadline and answers; the client's own limit
  (statement timeout, plus lock wait, plus 10 seconds, in the Rust
  client) only catches a server that went silent. When it passes, the
  connection is broken (see "Broken connections").
- **Idle transactions.** A connection that holds an exclusive
  transaction open and sends nothing for 60 seconds is closed, and the
  transaction rolled back: it was holding every other writer up. One
  that holds a reading, an optimistic or a dead transaction (see
  "Beginning with a statement") is closed after ten minutes; an optimistic one that has become the database's writer (a
  large statement makes it so, see `CLIENT.md`) counts as exclusive. A
  connection outside any transaction may idle for ever.

## Named locks

`Lock` takes a lock by name, for work only one process should do at a
time. It waits up to `wait_ms` and answers `Flag(true)` if the lock was
taken (or was already held by this connection), `Flag(false)` if not.
The lock belongs to the connection, not to a transaction: it is held
until `Unlock`, or until the connection closes. Names are per database.

To wait indefinitely, the Rust client asks again and again with a
`wait_ms` of 5,000, so that no one request outlasts its own time limit.

## Administration

`Admin` is a `u8` tag and its fields. Unless said otherwise the
connection must have greeted with the administrator's password; a
database's own password is answered `AccessDenied`.

| Tag | Request | Fields | Answer |
|---:|---|---|---|
| 1 | CreateDatabase | `name str, password str, if_not_exists bool` | `Ok` |
| 2 | DropDatabase | `name str, if_exists bool` | `Ok` |
| 3 | SetPassword | `name str, password str` | `Ok` |
| 4 | ListDatabases | | `Names` |
| 5 | Checkpoint | | `Ok`. Any connection may ask |
| 6 | Check | | `Text`: a report. Any connection may ask |
| 7 | Stats | | `Text`: counters. Any connection may ask |
| 8 | Dump | `databases list(str)` | `Chunk` frames, then `Ok` or `Error` |
| 9 | RestoreBegin | `replace bool` | `Ok` |
| 10 | RestoreChunk | `bytes` | `Ok` |
| 11 | RestoreEnd | | `Ok` |

`Checkpoint`, `Check` and `Stats` are about the connection's own
database, or about every database on an administering connection.

**Dump.** The server answers with any number of `Chunk` frames and then
one `Ok`, or one `Error` if the dump failed part-way. The dump is the
chunks' bytes one after another. With no database named it is every
database, or, on a connection that named a database in its greeting,
that database; and that much a connection may dump with the database's
own password. The bytes are the server's dump format, which is not part
of this protocol: a client stores them and gives them back.

**Restore.** `RestoreBegin`, then the dump's bytes in any number of
`RestoreChunk` requests of any size (the Rust client sends a megabyte at
a time), then `RestoreEnd`, each answered `Ok`. Nothing is changed until
`RestoreEnd` has read and verified the whole dump: an error from any of
the three, or a connection that closes first, leaves the server as it
was. With `replace`, a database in the dump replaces one of the same
name; without it, such a database is `AlreadyExists`.

## Broken connections

A connection is **broken** from the moment a request is sent until the
last frame of its answer has been read. If anything goes wrong in
between (an I/O error, the client's own time limit, a caller that gave
up waiting), it stays broken: an answer may still be on its way, and
the next frame read would be taken for the answer to the next request.
A broken connection is closed, never used again. Whatever transaction
or lock it held, the server ends when it sees the connection close.
Whether the request took effect is not known.

A pool that keeps connections for reuse should also:

- keep a connection out of the pool, and close it, while it holds a
  transaction or a named lock that was not ended in the ordinary way;
- before trusting a connection that has sat idle (the Rust client: 20
  seconds) with a real request, send a `Ping`, and open a new one if it
  fails: a server restart closes idle connections.

## Limits

| What | Limit |
|---|---|
| A frame's payload | 268,435,456 bytes; 16,384 before the greeting has succeeded |
| The greeting | within 10 seconds of connecting |
| Nesting of expressions, queries, list values and list types | 100 levels |
| Nesting of JSON | 100 levels |
| Nesting of requests (tags 19 to 22) | 3 levels inside the frame's own |
| Prepared statements a connection keeps | 256, of at most 65,536 bytes each and 1,048,576 bytes together |
| `keep_start` | 1 to 1,024 bytes, on a text or bytes column |
| A decimal | a 127-bit mantissa and its sign; at most 30 digits after the point |
| A `varint` | 64 bits, in at most 10 bytes |
| A key (primary or index, all parts together) | 1,900 bytes, else `KeyTooLong` |
| A result frame | cut at about 4 MiB of rows; one row is never split |

## Versions

The greeting carries a protocol version, and the answer to it is where
a client and a server that cannot talk find out.

**What each version added.** Each version adds to the one before and
takes nothing away:

| Version | Added |
|---:|---|
| 2 | Everything not marked otherwise |
| 3 | Requests `Prepared` (19), `BeginWith` (20) and `Batch` (21); the expression `Param` (22); the error codes `NotPrepared` (23) and `Unmet` (24) |
| 4 | The request `Explain` (22) |
| 5 | A column's last byte is `flags`, which may be followed by `keep_start` (see "Definitions") |
| 6 | The access `Whole` (5); the fifth column of an `Explain`'s answer, `way` |

**What the server does today.** It speaks versions 2 to 6. It answers a
`Hello` that names a version in that range with its own, 6, and refuses
any other with `ProtocolMismatch`, then closes the connection. It
answers every request it can decode in the same way whatever version
the client greeted with, with two differences: to a client that greeted
with less than 5, it describes its tables without `keep_start`; and to
one that greeted with less than 6, it answers an `Explain` with four
columns, without `way`.

**The rule a client is written to.** It holds for this server and for
those after it:

- A client sends the newest version whose messages it implements, both
  those it sends and those it reads. One that implements only version 2
  sends 2, whatever versions exist by then; one that sends `Prepared`
  sends at least 3, one that sends `Explain` at least 4, one that
  reads `keep_start` at least 5, and one that sends `Whole` or reads
  `way` 6.
- A server answers `ProtocolMismatch` to a version it does not speak,
  in its answer to `Hello`, and to nothing later. A server may speak
  several versions; a client must not assume which, and must be ready
  for `ProtocolMismatch` there: it is an error to report, not to retry.
- The version in the server's `Hello` response is the server's own. A
  client does not need it to equal the one it sent.
- Within a version, the meaning of a frame does not change.

**Versions 3 and 4** add requests, an expression and error codes and
change nothing a version 2 client sends or reads. The two error codes
come only in answer to the requests of version 3, so a client that does
not send those never meets them.

**Version 5** changes one thing a client reads: the last byte of a
`ColumnDef` is a byte of flags, bit 0 `auto_increment` and bit 1 a
`keep_start` varint after it (see "Definitions"). A column without
`keep_start` is the same bytes in every version. A server describes its
tables to a client that said less than 5 with `keep_start` left out, so
such a client never meets the bit; the setting changes where a server
stores values, not what any request answers. The server takes a column
with `keep_start` in `CreateTable` or `AddColumn` from a client of any
version.

**Version 6** adds one thing a client sends, the access `Whole`, and
one thing a client reads, the column `way` of an `Explain`'s answer,
which the server leaves out for a client that said less than 6. It
changes the meaning of no frame: a query answers what it answered. What
changed with the server that first spoke it is what such a server
*reads* for a query that names no way in (see "Queries"), which no
version of the protocol ever fixed, and it reads so for a client of any
version. A client that must have every row read sends `Whole`, which
takes version 6, or is served by a server set to choose for no query.

**What a client does with what it does not know:**

- an error code: `Internal`, as above;
- a response tag, or anything else it cannot decode: the connection is
  broken (see "Broken connections"). A server sends a client nothing
  that the client's version does not have, so this is damage, not
  novelty.

## The golden vectors

`protocol/fixtures/vectors.json` is this document in a form a program
can check:

```text
protocol_version                                the version the vectors are of: 2
max_frame, max_nesting, ...                     the limits above
tables                                          names and numbers: functions, error_codes,
                                                comparisons, arithmetic, aggregates, subqueries
vectors                                         one per line:
  name        "request.query.sample", "response.rows.every_value", ...
  kind        "request" or "response"
  timeout_ms  for a request, as a string
  canonical   false for bytes the Rust crate reads but never writes
  message     the message, described in JSON
  hex         the frame's payload
```

In a description every choice between kinds is an object with a `kind`;
64-bit integers are strings; bytes are hex; a float is the 16 hex digits
of its bits, most significant first, so that NaN and the infinities can
be written. The vectors cover every request and response, every kind of
value at the edges of its range, every expression, operator, function
and aggregate, every way into a table, and every error code, all of
version 2. What versions 3 to 6 added has no vectors; "Requests of
versions 3 and 4, byte by byte" below gives an example of each request,
and of what version 6 added.

A client's tests should at least:

1. build every request from its description, encode it, and compare the
   bytes with `hex`;
2. decode every response from `hex` and compare it with its description;
3. compare the client's own tables of names and numbers with `tables`.

The vectors are of one version, and stay so when the crate moves to a
later one: a later server must still read and write every message of
this version as the file has it. To change what a version's messages
are (which a released version never should): change `protocol/`, run
`PROTOCOL_VECTORS=write cargo test -p database-protocol --test vectors`,
and update this document and the clients until every test passes again.

## Two messages, byte by byte

`Query::table("mails").key([7])`, with a statement timeout of 60
seconds, is the vector `request.query.key`:

```text
e0 d4 03              timeout_ms: 60000
09                    Query
05 6d 61 69 6c 73     table: "mails"
00                    alias: none
01                    access: Primary
00                      reverse: false
01                      one range
01 01                     lower: Included, one value
01 03 0e                    Expr Value, Value int, zigzag 14 = 7
01 01                     upper: Included, one value
01 03 0e                    the same
00                        nulls: false
00                    joins: none
00                    unnest: none
00                    filter: none
00                    group_by: none
00                    having: none
00                    order_by: none
00                    offset: 0
00                    limit: none
00                    select: every column
00                    distinct: false
00                    with_total: false
```

An answer of one row, `id` 1 and `name` "ann", is `response.rows.one`:

```text
03                    Rows
00                    more: false
02                    two columns
02 69 64                "id"
04 6e 61 6d 65          "name"
01                    affected: 1
00                    total: none
01                    one row
03 02                   int, zigzag 2 = 1
06 03 61 6e 6e          text, "ann"
```

## Requests of versions 3 and 4, byte by byte

Each of these has a `timeout_ms` of 1,000 (`e8 07`). The statement they
share is `Query::table("t").key([param(0)])`, the row of `t` whose key
is the first value given:

```text
09                    Query
01 74                 table: "t"
00                    alias: none
01                    access: Primary
00                      reverse: false
01                      one range
01 01                     lower: Included, one value
16 00                       Expr Param, n 0
01 01                     upper: Included, one value
16 00                       the same
00                        nulls: false
00 00 00 00 00 00     joins, unnest, filter, group_by, having, order_by: none
00 00 00 00 00        offset 0, limit none, select every column, distinct false, with_total false
```

Run as statement 7 with the value 5, sent with the statement the first
time:

```text
e8 07                 timeout_ms: 1000
13                    Prepared
07                    id: 7
01                    statement: present
09 01 74 00 01 ...      the query above, 27 bytes, with no timeout_ms of its own
01                    params: one value
03 0a                   int, zigzag 10 = 5
```

and by its number alone after that:

```text
e8 07                 timeout_ms: 1000
13                    Prepared
07                    id: 7
00                    statement: none
01                    params: one value
03 0a                   int 5
```

An optimistic transaction that begins with it, waiting up to 5 seconds
for its turn to commit:

```text
e8 07                 timeout_ms: 1000
14                    BeginWith
02                    kind: optimistic
88 27                 lock_wait_ms: 5000
13 07 00 01 03 0a     statement: Prepared 7, no statement, params [5]
```

A batch that takes 1 from column `a` of the row with key 1, which must
change exactly one row, then runs statement 7:

```text
e8 07                 timeout_ms: 1000
15                    Batch
02                    two statements
0b                      Update
01 74 00 01 00 01         rows: table "t", no alias, Primary, not reversed, one range
01 01 01 03 02              lower: Included, Value int 1
01 01 01 03 02              upper: the same
00                          nulls: false
00 00 00 00 00 00 00 00 00 00 00   the rest of the query, as above
01                        set: one assignment
01 61                       column "a"
0e 01                       Arith subtract
02 00 01 61                   Column, no table, "a"
01 03 02                      Value int 1
00                        returning: none
01 01                   expect: present, 1
13 07 00 01 03 0a       Prepared 7, no statement, params [5]
00                      expect: none
```

Statement 7 explained, then its answer. The statement reads one row by
its key: no index entry and one row read, one row returned, in 240
microseconds:

```text
e8 07                 timeout_ms: 1000
16                    Explain
13 07 00 01 03 0a     statement: Prepared 7, no statement, params [5]
```

```text
03                    Rows
00                    more: false
04                    four columns
0d 69 6e 64 65 78 5f 65 6e 74 72 69 65 73     "index_entries"
09 72 6f 77 73 5f 72 65 61 64                 "rows_read"
0d 72 6f 77 73 5f 72 65 74 75 72 6e 65 64     "rows_returned"
06 6d 69 63 72 6f 73                          "micros"
01                    affected: 1
00                    total: none
01                    one row
03 00 03 02 03 02       int 0, int 1, int 1
03 e0 03                int, zigzag 480 = 240
```

That answer is what a client of version 4 or 5 reads. From version 6
there is a fifth column. A query of every row of `t`, asked for as the
whole table (the access `Whole`), explained, and its answer to a client
of version 6: no index entries, 50 rows read and returned, in 240
microseconds, by the way the query named:

```text
e8 07                 timeout_ms: 1000
16                    Explain
09                    statement: Query
01 74                   table: "t"
00                      alias: none
05                      access: Whole
00 00 00 00 00 00       no joins, no unnest, no filter, no group_by, no having, no order_by
00 00                   offset 0, no limit
00 00 00                select: every column; distinct: false; with_total: false
```

```text
03                    Rows
00                    more: false
05                    five columns
0d 69 6e 64 65 78 5f 65 6e 74 72 69 65 73     "index_entries"
09 72 6f 77 73 5f 72 65 61 64                 "rows_read"
0d 72 6f 77 73 5f 72 65 74 75 72 6e 65 64     "rows_returned"
06 6d 69 63 72 6f 73                          "micros"
03 77 61 79                                   "way"
01                    affected: 1
00                    total: none
01                    one row
03 00 03 64 03 64       int 0, int 50, int 50
03 e0 03                int, zigzag 480 = 240
06 16 22 74 22 20 62 79 20 74 68 65 20 77 68 6f 6c 65 20 74 61 62 6c 65
                        text, 22 bytes: "t" by the whole table
```
