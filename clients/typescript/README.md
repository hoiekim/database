# The TypeScript client

A client for the database server on Node's standard library alone
(`node:net`): no runtime dependency. Node 20 or later; an ES module.

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
beyond 2^53 loses precision as it does anywhere in JavaScript. Object
keys come back in the order the server stores them: by length, then by
bytes.

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
  server. Skipped if no server binary is found; `DATABASE_BIN` names one.
