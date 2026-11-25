# The TypeScript client

A client for the database server on Node's standard library alone
(`node:net`, `node:tls`): no runtime dependency. Node 20 or later; an ES module.

```ts
import { Client, ColumnDef, IndexDef, KeyRange, Query, TableDef, Type, Update, col, count } from "database-client";

const db = await Client.connect("database://shop:secret@database:5454");

await db.createTableIfNotExists(
  new TableDef("mails")
    .column(new ColumnDef("mail_id", Type.Uuid).defaultRandomUuid())
    .column(new ColumnDef("user_id", Type.Uuid))
    .column(new ColumnDef("subject", Type.Text).default(""))
    .column(new ColumnDef("date", Type.Timestamp).defaultNow())
    .column(new ColumnDef("read", Type.Bool).default(false))
    .primaryKey(["mail_id"])
    .index(new IndexDef("mails_user_date").on(["user_id"]).onDesc("date").on(["mail_id"])),
);

// The newest 50 mails of a user: 50 index entries read, 50 rows fetched.
const page = await db.query(Query.table("mails").index("mails_user_date", KeyRange.eq([userId])).limit(50));
for (const mail of page) console.log(mail.get("subject"), String(mail.get("date")));

// Several statements as one, as if nothing else wrote while it ran.
const unread = await db.transaction(async (tx) => {
  const row = await tx.queryOne(
    Query.table("mails").index("mails_user_date", KeyRange.eq([userId])).select("n", count().filter(col("read").eq(false))),
  );
  await tx.update(Update.table("users").key([userId]).set("unread", row.int("n")));
  return row.int("n");
});

db.close();
```

The vocabulary is the Rust client's, name for name in camelCase:
`docs/CLIENT.md` describes what each piece means and how the things
usually said in SQL are said here. The bytes on the wire are
`docs/PROTOCOL.md`.

## What differs from the Rust client

- **Names are camelCase:** `orderBy`, `groupBy`, `leftJoin`, `isNull`,
  `inList`, `onConflictUpdate`, `returningAll`, `defaultRandomUuid`,
  `foreignKeyTo`, and so on. Where a name is taken by the language:
  `cases(..)` for `case`, `NULL` for `null()`, `KeyRange.of(lower,
  upper)` for `KeyRange::new`, `depth()` and `rank()` for `Expr::Depth`
  and `Expr::Rank`.
- **No operators:** arithmetic is `add`, `sub`, `mul`, `div`, `mod`,
  `neg`; `now().sub(minutes(5))`.
- **Enumerations are strings:** `call("lower", [..])`, `"cascade"`,
  `"english"`, `"set_null"`. TypeScript checks them.
- **Builders do not change:** each method returns a new builder, so a
  query kept in a variable can be narrowed twice. A builder's `.spec`
  (`.node` for an expression) is the plain data the wire carries, typed
  in `ast`; `describe()` returns those.
- **A transaction must be ended.** JavaScript has no destructor to roll
  one back when it is forgotten, and until it ends it occupies one of
  the pool's connections (8 by default, `?pool=n` in the URL).
  `db.transaction(work)`, `db.exclusive(work)` and `db.read(work)` end
  theirs whatever `work` does; `db.begin()`, `db.beginOptimistic()` and
  `db.beginRead()` hand the transaction to the caller, who calls
  `commit()` or `rollback()`. The same goes for a `LockGuard` and
  `unlock()`.
- **`db.transaction(work)` may call `work` more than once,** as in the
  Rust client: twice beside other writers, then as the database's only
  writer.
- **It greets the server with protocol version 2** (`PROTOCOL_VERSION`),
  so it has none of what later versions added: no prepared statements
  or `param(n)`, no transaction that begins with its first statement in
  one request, no batches, and no `explain`. It cannot set `keep_start`
  on a column, and a table that uses `keep_start` is described to it
  without the setting.

## Values

| Database | Written as | Read as |
|---|---|---|
| NULL | `null` | `null` |
| bool | `boolean` | `boolean` |
| int (64-bit) | a `bigint`, or a `number` that is a safe integer | a `number` if it is a safe integer, else a `bigint` |
| float | any other `number`, or `float(x)` | `number` |
| decimal | `Decimal` | `Decimal` |
| text | `string` | `string` |
| bytes | `Uint8Array` (a `Buffer` is one) | `Uint8Array` |
| uuid | `Uuid` | `Uuid` |
| timestamp | `Timestamp`, or a `Date` | `Timestamp` |
| date | `CalendarDate` | `CalendarDate` |
| json | a plain object, or `json(value)` | the parsed value |
| list | an array | an array |

**Integers.** The database's integers are 64-bit, and a JavaScript
number holds integers exactly only up to 2^53. The rule is stated once:
a `number` is written as an int exactly when `Number.isSafeInteger` says
so, and as a float otherwise (`float(1)` writes a whole number as a
float); a `bigint` is always an int. An int is read as a `number` when
it is a safe integer and as a `bigint` when it is not. `row.int(name)`
and `row.bigint(name)` read a column as one or the other without the
question; the first throws where a number would be inexact.

**Timestamps** are microseconds since 1970-01-01T00:00:00Z. A `Date`
keeps milliseconds only, so a timestamp read from the server is a
`Timestamp`: handed back as a key or a cursor it is the same instant, to
the microsecond. `timestamp.toDate()` gives the `Date`,
`Timestamp.fromDate(date)` the reverse, and a `Date` may be written
directly.

**Decimals** are exact: `Decimal.parse("12.34")`, `decimal.toString()`,
`decimal.toNumber()`. **Dates** are days: `CalendarDate.parse("2026-03-05")`,
`CalendarDate.fromYmd(2026, 3, 5)`. **Uuids**: `Uuid.parse(text)`,
`String(uuid)`. All four turn into their text in `JSON.stringify`.

**JSON** must be told from text and from lists when written: `"x"` is
text and `[1, 2]` is a list, so a JSON string or array is written
`json("x")` or `json([1, 2])`. A plain object can only be JSON and needs
no wrapper. It is read with `JSON.parse`, so an integer inside JSON
beyond 2^53 loses precision as it does anywhere in JavaScript. The
server stores object keys by length, then by bytes, but a JavaScript
object lists keys that look like array indices first, in numeric order:
`{"2":1,"b":2,"10":3}` comes back with the keys `2`, `10`, `b`. Where
the stored order matters, read the column as text,
`col("detail").cast(Type.Text)`, which gives the JSON as stored.

## TLS

Without a setting a connection is in the clear, the password included,
which is right only on a network nobody else can listen on; a server
started with a certificate refuses such a connection unless it was told
`DATABASE_TLS=optional`. The settings go in the URL, or in the `tls` of
a `Config`:

```ts
// The server's certificate must be signed by an authority in this file, and by no other.
await Client.connect("database://shop:secret@database:5454?tls_ca=%2Fetc%2Fdatabase%2Fca.pem");
// Reached by an address, or by a name the certificate does not carry.
await Client.connect({ ...config("10.0.0.5", 5454, "shop", "secret"), tls: { ca: "/etc/database/ca.pem", name: "database.example.com" } });
// Signed by a public authority.
await Client.connect("database://shop:secret@database.example.com?tls=on");
```

| URL | `Config.tls` | |
|---|---|---|
| `tls=on` | `{}` | TLS, the server verified against the authorities Node trusts: its own list, with what `NODE_EXTRA_CA_CERTS` adds |
| `tls_ca=<file>` | `{ ca }` | verified against the authorities in that PEM file, and no other |
| `tls_name=<name>` | `{ name }` | the name or address the certificate must be for, where that is not the host |
| `tls=off` | absent | in the clear: the default |

`tls_ca` and `tls_name` turn TLS on by themselves, and `tls=off` beside
either is an invalid URL. A client that asks for TLS always verifies the
certificate and the name it is for: there is no setting that encrypts
without verifying, and no falling back to a connection in the clear. A
server that cannot be verified, or that serves no TLS, rejects with a
`DbError` of code `Connection` before the password is sent. Only
TLS 1.3 is spoken. The Rust client's `tls_cert`, which takes one exact
certificate for the server's, is not here.

## Errors

Everything the server refuses rejects with a `DbError`: `code` (an
`ErrorCode`), `codeName`, `message`, and for the constraint errors
`constraint` and `table`. A value JavaScript holds that the database has
no kind for throws `TypeError` or `RangeError` when the request is
built, before anything is sent.

```ts
try {
  await db.insert(Insert.into("users").set("email", email));
} catch (e) {
  if (e instanceof DbError && e.violates("users_email")) { /* taken */ } else throw e;
}
```

## Administration

`db.admin` has `createDatabase`, `dropDatabase`, `setPassword`,
`listDatabases`, `checkpoint`, `check`, `stats`, `dump(write)` and
`restore(source, replace)`; all but `checkpoint`, `check`, `stats` and a
dump of the connection's own database need the administrator's password
(`Client.connect("database://<admin password>@host")`).

## Tests

```sh
cargo build -p database-server         # the integration tests start target/debug/database
cd clients/typescript
npm ci
npm test
```

- `test/vectors.test.ts`: every message of
  `protocol/fixtures/vectors.json` (the golden vectors, kept by the Rust
  crate's own tests) encodes to the same bytes here, and every response
  in it decodes to what it describes.
- `test/builders.test.ts`: the builders say what the Rust builders say.
- `test/values.test.ts`: JavaScript values, URLs and rows, without a server.
- `test/integration.test.ts`: every kind of request against a real
  server, in the clear and under TLS with the certificates of
  `protocol/fixtures/tls`. Skipped if no server binary is found;
  `DATABASE_BIN` names one.
