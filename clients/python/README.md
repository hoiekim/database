# The Python client

A synchronous client for the database server, on the standard library
alone: no dependency to install, nothing compiled. Python 3.9 or later.

```python
from database_client import *

db = Client.connect("database://shop:secret@database:5454")

db.create_table_if_not_exists(
    TableDef("mails")
    .column(ColumnDef("mail_id", Type.UUID).default_random_uuid())
    .column(ColumnDef("user_id", Type.UUID))
    .column(ColumnDef("subject", Type.TEXT).default(""))
    .column(ColumnDef("date", Type.TIMESTAMP).default_now())
    .column(ColumnDef("read", Type.BOOL).default(False))
    .primary_key(["mail_id"])
    .index(IndexDef("mails_user_date").on(["user_id"]).on_desc("date").on(["mail_id"]))
)

# The newest 50 mails of a user: 50 index entries read, 50 rows fetched.
page = db.query(Query.table("mails").index("mails_user_date", KeyRange.eq([user_id])).limit(50))
for mail in page:
    print(mail["subject"], mail["date"])

# Several statements as one, as if nothing else wrote while it ran.
def mark_read(tx: Transaction) -> int:
    unread = tx.query_one(
        Query.table("mails").index("mails_user_date", KeyRange.eq([user_id]))
        .select("n", count().filter(col("read").eq(False)))
    )["n"]
    tx.update(Update.table("users").key([user_id]).set("unread", unread))
    return unread

unread = db.transaction(mark_read)
```

The vocabulary is the Rust client's, name for name: `docs/CLIENT.md`
describes what each piece means and how the things usually said in SQL
are said here. The bytes on the wire are `docs/PROTOCOL.md`.

## What differs from the Rust client

- **Names the language takes get an underscore:** `and_`, `or_`, `not_`,
  `KeyRange.is_`, `Delete.from_`, `all_`, `any_`, `list_`, `sum_`, `min_`,
  `max_`. `Expr.Depth` and `Expr.Rank` are `depth()` and `rank()`.
  Constructors are plain calls: `TableDef("t")`, `ColumnDef("c", Type.INT)`,
  `IndexDef("i")`.
- **Expressions are built with methods, not `and`/`or`/`not`:**
  `col("a").eq(1).and_(col("b").gt(2))`. Python's own `and` cannot be
  overloaded, so using an expression as a truth value raises `TypeError`
  instead of silently dropping half a filter. `+ - * / %` and unary `-`
  work on expressions.
- **Builders do not change:** each method returns a new builder, so a
  query kept in a variable can be narrowed twice.
- **Calls block.** A `Client` may be shared between threads; it keeps a
  pool of connections (8 by default, `?pool=n` in the URL). A
  transaction is used by one thread at a time.
- **A transaction is a context manager:** it commits when the block
  ends and rolls back if the block raises. `db.transaction(work)` is the
  one that runs `work` again after a conflict, as in the Rust client:
  twice beside other writers, then as the database's only writer.

```python
with db.begin() as tx:              # exclusive; begin_optimistic(), begin_read()
    tx.insert(Insert.into("audit").set("action", "user.create"))

with db.lock("shop.migrate"):       # a named lock, held until the block ends
    migrate(db)
```

## Values

| Database | Python |
|---|---|
| NULL | `None` |
| bool | `bool` |
| int (64-bit) | `int`; one that does not fit 64 bits raises `ValueError` |
| float | `float` |
| decimal | `decimal.Decimal`, exact; at most 30 digits after the point |
| text | `str` |
| bytes | `bytes` (`bytearray` and `memoryview` are accepted) |
| uuid | `uuid.UUID` |
| timestamp | `datetime.datetime` |
| date | `datetime.date` |
| json | read: the parsed value. Written: a `dict`, or `Json(value)` for anything else |
| list | `list` (a `tuple` is accepted) |

**Timestamps** are microseconds since 1970-01-01T00:00:00Z. One read
from the server is a `datetime` with `tzinfo=timezone.utc`. One written
may be in any zone and is converted exactly, to the microsecond; a
`datetime` *without* a zone is taken to be UTC, not local time. The
database holds times and days far outside the years 1 to 9999 that
`datetime` can express; those are read as `Timestamp(micros)` and
`Date(days)`, which may also be written.

**JSON** must be told from text and from lists when written: `"x"` is
text and `[1, 2]` is a list, so a JSON string or array is written
`Json("x")` or `Json([1, 2])`. A `dict` can only be JSON and needs no
wrapper. Object keys come back in the order the server stores them: by
length, then by bytes.

## Errors

Everything the server refuses raises `DbError`, with `code` (an
`ErrorCode`), `message`, and for the constraint errors `constraint` and
`table`. A value Python holds that the database has no kind for raises
`TypeError` or `ValueError` before anything is sent.

```python
try:
    db.insert(Insert.into("users").set("email", email))
except DbError as e:
    if e.violates("users_email"):
        ...
```

## Administration

`db.admin` has `create_database`, `drop_database`, `set_password`,
`list_databases`, `checkpoint`, `check`, `stats`, `dump(out)` and
`restore(source, replace=False)`; all but `checkpoint`, `check`, `stats`
and a dump of the connection's own database need the administrator's
password (`Client.connect("database://<admin password>@host")`).

## Tests

```sh
cargo build -p database-server         # the integration tests start target/debug/database
cd clients/python
python -m unittest discover -s tests -t .
```

- `tests/test_vectors.py`: every message of
  `protocol/fixtures/vectors.json` (the golden vectors, kept by the Rust
  crate's own tests) encodes to the same bytes here, and every response
  in it decodes to what it describes.
- `tests/test_builders.py`: the builders say what the Rust builders say.
- `tests/test_values.py`: Python values, URLs and rows, without a server.
- `tests/test_integration.py`: every kind of request against a real
  server. Skipped if no server binary is found; `DATABASE_BIN` names one.
