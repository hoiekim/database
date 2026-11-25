// This client against a real server: every kind of request, over TCP.

import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { after, before, describe, test } from "node:test";

import { Conn } from "../src/conn.js";
import {
  Bound,
  CalendarDate,
  Client,
  ColumnDef,
  DbError,
  Decimal,
  Delete,
  ErrorCode,
  IndexDef,
  Insert,
  KeyRange,
  PRIMARY,
  Query,
  TableDef,
  Timestamp,
  Type,
  Update,
  Uuid,
  col,
  collect,
  concat,
  count,
  depth,
  excluded,
  exists,
  float,
  json,
  list,
  max,
  old,
  rank,
  scalar,
  sum,
  val,
  type Config,
  type Db,
  type Transaction,
  type ast,
} from "../src/index.js";
import { PASSWORD, Server, binary, fixture } from "./server.js";

const path = binary();
const skip = path === null ? "no server binary: run `cargo build -p database-server`, or set DATABASE_BIN" : false;

async function code(expected: ErrorCode, attempt: Promise<unknown>): Promise<DbError> {
  let caught: unknown;
  await assert.rejects(attempt, (e) => {
    caught = e;
    return true;
  });
  assert.ok(caught instanceof DbError, String(caught));
  assert.equal(caught.codeName, new DbError(expected, "").codeName, caught.message);
  return caught;
}

const users = (): TableDef =>
  new TableDef("users")
    .column(new ColumnDef("id", Type.Int).autoIncrement())
    .column(new ColumnDef("name", Type.Text))
    .column(new ColumnDef("email", Type.Text).nullable())
    .column(new ColumnDef("age", Type.Int).default(0))
    .column(new ColumnDef("created", Type.Timestamp).defaultNow())
    .primaryKey(["id"])
    .unique("users_name_key", ["name"])
    .index(new IndexDef("users_email").onExpr(col("email").lower()).unique().filter(col("email").isNotNull()))
    .check("age_not_negative", col("age").ge(0));

const orders = (): TableDef =>
  new TableDef("orders")
    .column(new ColumnDef("id", Type.Uuid).defaultRandomUuid())
    .column(new ColumnDef("user_id", Type.Int))
    .column(new ColumnDef("day", Type.Date))
    .column(new ColumnDef("amount", Type.decimal(15, 2)))
    .column(new ColumnDef("paid", Type.Bool).default(false))
    .primaryKey(["id"])
    .index(new IndexDef("orders_user_day").on(["user_id"]).onDesc("day").on(["id"]))
    .foreignKey(["user_id"], "users", "cascade");

const day = (n: number): CalendarDate => CalendarDate.fromYmd(2026, 1, n);
const text = (values: unknown[]): string[] => values.map(String);

describe("against a server", { skip }, () => {
  let server: Server;
  const opened: Client[] = [];

  before(async () => {
    server = await Server.start(path!);
  });

  after(async () => {
    for (const client of opened) client.close();
    await server?.stop();
  });

  /** A database of its own for each test. */
  async function fresh(): Promise<[string, Client]> {
    const name = `t_${randomBytes(6).toString("hex")}`;
    const db = await server.client(name);
    opened.push(db);
    return [name, db];
  }

  async function another(name: string, settings: Partial<Config> = {}): Promise<Client> {
    const db = await server.client(name, settings);
    opened.push(db);
    return db;
  }

  test("tables, columns and indexes", async () => {
    const [, db] = await fresh();
    await db.createTable(users());
    await db.createTable(orders());
    await code(ErrorCode.AlreadyExists, db.createTable(users()));
    await db.createTableIfNotExists(users());

    // What the server holds is what was sent, read back through the
    // decoders for definitions, expressions and types.
    const described = new Map((await db.describe()).map((t) => [t.name, t]));
    assert.deepEqual([...described.keys()].sort(), ["orders", "users"]);
    assert.deepEqual(described.get("users"), users().spec);
    // But for a foreign key to a primary key, whose columns the server has filled in.
    const sent = orders().spec;
    assert.deepEqual(sent.foreignKeys[0]!.refColumns, []);
    const stored = { ...sent, foreignKeys: [{ ...sent.foreignKeys[0]!, refColumns: ["id"] }] };
    assert.deepEqual(described.get("orders"), stored);
    assert.deepEqual(await db.describeTable("orders"), stored);
    assert.equal(await db.describeTable("nothing"), null);

    await db.addColumn("users", new ColumnDef("tags", Type.list(Type.Text)).default(["new"]));
    await db.addColumnIfNotExists("users", new ColumnDef("tags", Type.list(Type.Text)));
    await db.createIndex("users", new IndexDef("users_tags").onEach(col("tags")));
    await db.createIndexIfNotExists("users", new IndexDef("users_tags").onEach(col("tags")));
    await db.insert(Insert.into("users").set("name", "ann"));
    const tagged = await db.query(Query.table("users").index("users_tags", KeyRange.eq(["new"])));
    assert.deepEqual(tagged.column("name"), ["ann"]);
    const table = await db.describeTable("users");
    assert.deepEqual(table?.columns.find((c) => c.name === "tags")?.type, Type.list(Type.Text));
    assert.ok(table?.indexes.some((i) => i.name === "users_tags"));

    await db.dropIndex("users", "users_tags");
    await db.dropIndexIfExists("users", "users_tags");
    await code(ErrorCode.NotFound, db.dropIndex("users", "users_tags"));
    await db.dropColumn("users", "tags");
    await db.dropColumnIfExists("users", "tags");
    await db.dropTable("orders");
    await db.dropTableIfExists("orders");
    await code(ErrorCode.NotFound, db.dropTable("orders"));
    await code(ErrorCode.NotFound, db.query(Query.table("orders")));
  });

  test("every kind of value comes back as it went", async () => {
    const [, db] = await fresh();
    await db.createTable(
      new TableDef("things")
        .column(new ColumnDef("id", Type.Int))
        .column(new ColumnDef("flag", Type.Bool).nullable())
        .column(new ColumnDef("n", Type.Int).nullable())
        .column(new ColumnDef("x", Type.Float).nullable())
        .column(new ColumnDef("money", Type.decimal(15, 2)).nullable())
        .column(new ColumnDef("exact", Type.Decimal).nullable())
        .column(new ColumnDef("text", Type.Text).nullable())
        .column(new ColumnDef("blob", Type.Bytes).nullable())
        .column(new ColumnDef("uid", Type.Uuid).nullable())
        .column(new ColumnDef("at", Type.Timestamp).nullable())
        .column(new ColumnDef("day", Type.Date).nullable())
        .column(new ColumnDef("doc", Type.Json).nullable())
        .column(new ColumnDef("tags", Type.list(Type.Text)).nullable())
        .column(new ColumnDef("grid", Type.list(Type.list(Type.Int))).nullable())
        .primaryKey(["id"]),
    );
    const columns = ["id", "flag", "n", "x", "money", "exact", "text", "blob", "uid", "at", "day", "doc", "tags", "grid"];
    const uid = Uuid.parse("6f9619ff-8b86-d011-b42d-00c04fc964ff");
    const at = new Timestamp(1_772_706_030_123_456n);
    await db.insert(
      Insert.into("things")
        .columns(columns)
        .values([1, true, -(2n ** 63n), 1.5, Decimal.parse("12.34"), Decimal.parse("-0.000001"), "héllo, wörld: 日本語 🚀", new Uint8Array([0, 255, 7]), uid, at, day(5), { b: 1, a: [1, 2.5, null, { k: "v" }] }, ["a", "b"], [[1, 2], [], [3]]])
        .values([2, false, 2n ** 63n - 1n, float(-Infinity), Decimal.parse("5"), Decimal.parse("123456789012345678.90"), "", new Uint8Array(), new Uuid(new Uint8Array(16)), new Date(1500), CalendarDate.parse("0001-01-01"), json("just text"), [], null])
        .values([3, ...Array<null>(13).fill(null)]),
    );

    const rows = await db.query(Query.table("things"));
    assert.deepEqual(rows.columns, columns);
    assert.equal(rows.affected, 3);
    const [one, two, three] = rows.rows as [(typeof rows.rows)[0], (typeof rows.rows)[0], (typeof rows.rows)[0]];
    assert.deepEqual([one.get("id"), one.get("flag"), one.get("n"), one.get("x")], [1, true, -(2n ** 63n), 1.5]);
    assert.equal(String(one.get("money")), "12.34");
    assert.equal(String(one.get("exact")), "-0.000001");
    assert.equal(one.get("text"), "héllo, wörld: 日本語 🚀");
    assert.deepEqual([...(one.get("blob") as Uint8Array)], [0, 255, 7]);
    assert.ok((one.get("uid") as Uuid).equals(uid));
    // A timestamp keeps its microseconds, which a Date would drop.
    assert.equal((one.get("at") as Timestamp).micros, at.micros);
    assert.equal(String(one.get("at")), "2026-03-05T10:20:30.123456Z");
    assert.equal(String(one.get("day")), "2026-01-05");
    // JSON comes back parsed, its keys in the stored order.
    assert.deepEqual(one.get("doc"), { a: [1, 2.5, null, { k: "v" }], b: 1 });
    assert.deepEqual(Object.keys(one.get("doc") as object), ["a", "b"]);
    assert.deepEqual(one.get("tags"), ["a", "b"]);
    assert.deepEqual(one.get("grid"), [[1, 2], [], [3]]);

    assert.equal(two.get("n"), 2n ** 63n - 1n);
    assert.equal(two.bigint("n"), 2n ** 63n - 1n);
    assert.equal(two.get("x"), -Infinity);
    // A decimal column keeps its scale: 5 in a (15, 2) column is 5.00.
    assert.equal(String(two.get("money")), "5.00");
    assert.equal(String(two.get("exact")), "123456789012345678.90");
    assert.deepEqual([two.get("text"), (two.get("blob") as Uint8Array).length, String(two.get("uid"))], ["", 0, "00000000-0000-0000-0000-000000000000"]);
    // A Date was written to the millisecond.
    assert.equal((two.get("at") as Timestamp).micros, 1_500_000n);
    assert.equal(String(two.get("day")), "0001-01-01");
    assert.equal(two.get("doc"), "just text");
    assert.deepEqual(two.get("tags"), []);
    assert.deepEqual(three.values, [3, ...Array<null>(13).fill(null)]);

    // Values as keys and in expressions.
    assert.ok(((await db.get("things", [1]))?.get("uid") as Uuid).equals(uid));
    const ids = async (q: Query): Promise<unknown[]> => (await db.query(q.select("id", col("id")))).column();
    assert.deepEqual(await ids(Query.table("things").filter(col("at").eq(at))), [1]);
    assert.deepEqual(await ids(Query.table("things").filter(col("money").gt(Decimal.parse("5.001")))), [1]);
    assert.deepEqual(await ids(Query.table("things").filter(col("doc").jsonText(["a", 3, "k"]).eq("v"))), [1]);
    // A whole number given to a float column is widened by the server.
    await db.update(Update.table("things").key([3]).set("x", 2));
    assert.equal((await db.get("things", [3]))?.get("x"), 2);

    // Keys that look like array indices come first in a JavaScript
    // object whatever the stored order; read as text, the JSON keeps it.
    await db.update(Update.table("things").key([3]).set("doc", { "2": 1, b: 2, "10": 3 }));
    const doc = await db.queryOne(Query.table("things").key([3]).select("doc", col("doc")).select("text", col("doc").cast(Type.Text)));
    assert.deepEqual(Object.keys(doc.get("doc") as object), ["2", "10", "b"]);
    assert.deepEqual([...(doc.get("text") as string).matchAll(/"(\w+)":/g)].map((m) => m[1]), ["2", "b", "10"]);

    // What JavaScript cannot send is refused before it is sent.
    assert.throws(() => Query.row().select("v", val(2n ** 63n)), RangeError);
    assert.throws(() => Query.row().select("v", val(new Map() as never)), TypeError);
    assert.equal((await db.queryOne(Query.row().select("v", val(1)))).get("v"), 1);
  });

  test("insert, upsert, update and delete, with returning", async () => {
    const [, db] = await fresh();
    await db.createTable(users());
    const inserted = await db.insert(Insert.into("users").set("name", "ann").set("email", "Ann@Example.com").returning(["id", "created"]));
    assert.equal(inserted.affected, 1);
    const ann = inserted.one();
    assert.equal(ann.get("id"), 1);
    const created = ann.get("created");
    assert.ok(created instanceof Timestamp);
    assert.ok(Math.abs(created.toDate().getTime() - (await db.now()).toDate().getTime()) < 60_000);

    const several = await db.insert(Insert.into("users").columns(["name", "age"]).values(["bob", 30]).values(["cy", 40]).returningAll());
    assert.equal(several.affected, 2);
    assert.deepEqual(several.columns, ["id", "name", "email", "age", "created"]);
    assert.deepEqual(several.column("name"), ["bob", "cy"]);

    // An upsert on the name: the existing row is updated, the proposed
    // values read through excluded().
    const upsert = Insert.into("users")
      .set("name", "bob")
      .set("age", 31)
      .onConflictUpdate("users_name_key")
      .update("age", excluded("age").add(col("age")))
      .returning(["id", "age"])
      .returningExpr("was", old("age"));
    assert.deepEqual((await db.insert(upsert)).one().toObject(), { id: 2, age: 61, was: 30 });
    const skipped = await db.insert(Insert.into("users").set("name", "bob").onConflictDoNothing());
    assert.deepEqual([skipped.affected, skipped.length], [0, 0]);
    const copied = await db.insert(
      Insert.into("users")
        .columns(["name", "age"])
        .fromQuery(Query.table("users").select("name", concat([col("name"), "2"])).select("age", col("age"))),
    );
    assert.equal(copied.affected, 3);

    // Compare-and-swap: the count is the answer.
    const swap = Update.table("users").key([1]).filter(col("age").eq(0)).set("age", col("age").add(1)).returning(["age"]).returningExpr("was", old("age"));
    assert.deepEqual((await db.update(swap)).one().toObject(), { age: 1, was: 0 });
    assert.equal((await db.update(swap)).affected, 0);
    assert.equal((await db.update(Update.table("users").filter(col("age").ge(30)).set("age", col("age").add(100)))).affected, 4);

    const gone = await db.delete(Delete.from("users").key([3]).returning(["name"]));
    assert.equal(gone.one().get("name"), "cy");
    assert.equal(await db.get("users", [3]), null);
    assert.equal((await db.delete(Delete.from("users").filter(col("name").like("%2")))).affected, 3);
    assert.deepEqual((await db.query(Query.table("users"))).column("name"), ["ann", "bob"]);
  });

  test("a query with a range, a join, a filter and aggregates", async () => {
    const [, db] = await fresh();
    await db.createTable(users());
    await db.createTable(orders());
    await db.insert(Insert.into("users").columns(["name", "age"]).values(["ann", 30]).values(["bob", 40]).values(["cy", 50]));
    let insert = Insert.into("orders").columns(["user_id", "day", "amount", "paid"]);
    for (const user of [1, 2, 3]) {
      for (let n = 1; n <= 10; n++) insert = insert.values([user, day(n), new Decimal(BigInt(n * user * 25), 2), n % 2 === 0]);
    }
    assert.equal((await db.insert(insert)).affected, 30);

    // The newest five of one user, through the index, in its order.
    const page = await db.query(Query.table("orders").index("orders_user_day", KeyRange.eq([2])).limit(5).columns(["day", "amount"]));
    assert.deepEqual(text(page.column("day")), [10, 9, 8, 7, 6].map((n) => String(day(n))));
    assert.equal(String(page.rows[0]!.get("amount")), "5.00");
    // The next page: the entries after a cursor (on a descending key, earlier days).
    const cursor = KeyRange.within([2], Bound.excluded([day(6)]), Bound.Unbounded);
    assert.deepEqual(text((await db.query(Query.table("orders").index("orders_user_day", cursor).limit(2))).column("day")), ["2026-01-05", "2026-01-04"]);
    const backwards = await db.query(Query.table("orders").index("orders_user_day", KeyRange.eq([2])).reverse().limit(1));
    assert.deepEqual(text(backwards.column("day")), ["2026-01-01"]);

    // Two users' orders joined to their users, filtered, grouped, counted.
    const report = await db.query(
      Query.table("orders")
        .alias("o")
        .indexRanges("orders_user_day", [KeyRange.eq([1]), KeyRange.eq([3])])
        .join(Query.table("users").alias("u").key([col("o.user_id")]).filter(col("u.age").ge(30)))
        .filter(col("o.day").between(day(3), day(8)))
        .groupBy([col("u.name")])
        .select("name", col("u.name"))
        .select("orders", count())
        .select("paid", count().filter(col("o.paid")))
        .select("total", sum(col("o.amount")))
        .select("largest", max(col("o.amount")))
        .select("days", collect(col("o.day")).distinct())
        .orderBy(col("name").desc()),
    );
    assert.deepEqual(report.columns, ["name", "orders", "paid", "total", "largest", "days"]);
    const [cy, ann] = report.rows as [(typeof report.rows)[0], (typeof report.rows)[0]];
    assert.deepEqual(text(cy.values.slice(0, 5)), ["cy", "6", "3", "24.75", "6.00"]);
    assert.deepEqual(text(ann.values.slice(0, 5)), ["ann", "6", "3", "8.25", "2.00"]);
    assert.deepEqual(text(ann.get("days") as unknown[]).sort(), [3, 4, 5, 6, 7, 8].map((n) => String(day(n))));

    // A left join keeps the rows without a match; a subquery reads the enclosing row.
    await db.insert(Insert.into("users").set("name", "dee"));
    const own = (): Query => Query.table("orders").index("orders_user_day", KeyRange.eq([col("u.id")]));
    const counted = await db.query(
      Query.table("users")
        .alias("u")
        .leftJoin(own().alias("o").limit(1))
        .select("name", col("u.name"))
        .select("latest", col("o.day"))
        .select("orders", scalar(own().select("n", count())))
        .select("any", exists(own())),
    );
    const objects = counted.objects();
    assert.deepEqual({ ...objects[0], latest: String(objects[0]!.latest) }, { name: "ann", latest: "2026-01-10", orders: 10, any: true });
    assert.deepEqual(objects[3], { name: "dee", latest: null, orders: 0, any: false });

    const paged = await db.query(Query.table("orders").filter(col("paid")).orderBy(col("amount").desc()).orderBy(col("day")).offset(2).limit(3).withTotal().columns(["amount"]));
    assert.deepEqual([paged.length, paged.affected, paged.total], [3, 3, 15]);
    assert.deepEqual(text(paged.column()), ["5.00", "4.50", "4.00"]);
    const row = await db.queryOne(Query.row().select("sum", val(1).add(2)).select("text", val("a").upper()).select("list", list([1, "b"])));
    assert.deepEqual(row.toObject(), { sum: 3, text: "A", list: [1, "b"] });
    assert.equal(await db.queryOpt(Query.table("users").key([99])), null);
    await code(ErrorCode.NotFound, db.queryOne(Query.table("users").key([99])));
    await code(ErrorCode.InvalidRequest, db.queryOne(Query.table("users")));
  });

  test("trees and full-text search", async () => {
    const [, db] = await fresh();
    await db.createTable(
      new TableDef("pages")
        .column(new ColumnDef("id", Type.Int))
        .column(new ColumnDef("parent_id", Type.Int).nullable())
        .column(new ColumnDef("title", Type.Text))
        .primaryKey(["id"])
        .index(new IndexDef("pages_parent").on(["parent_id", "id"]))
        .index(new IndexDef("pages_search").fullText("english", [[col("title"), 1.0]])),
    );
    let insert = Insert.into("pages").columns(["id", "parent_id", "title"]);
    const pages: [number, number | null, string][] = [
      [1, null, "Invoices"],
      [2, 1, "Overdue invoices"],
      [3, 1, "Paid"],
      [4, 2, "An overdue invoice from March"],
      [5, 4, "Notes"],
    ];
    for (const page of pages) insert = insert.values(page);
    await db.insert(insert);
    const up = await db.query(Query.table("pages").ancestors([[5]], "parent_id").select("id", col("id")).select("depth", depth()));
    assert.deepEqual(
      up.rows.map((r) => r.values),
      [
        [5, 0],
        [4, 1],
        [2, 2],
        [1, 3],
      ],
    );
    const down = await db.query(Query.table("pages").descendants([[1]], "parent_id", "pages_parent").stopAt(col("id").eq(4)).maxDepth(3).select("id", col("id")));
    assert.deepEqual(down.column(), [1, 2, 3, 4]);
    assert.deepEqual((await db.query(Query.table("pages").index("pages_parent", KeyRange.is([null])))).column("id"), [1]);
    assert.equal((await db.query(Query.table("pages").index("pages_parent", KeyRange.eq([null])))).length, 0);
    const hits = await db.query(Query.table("pages").search("pages_search", [], "invoice overdue").select("id", col("id")).select("rank", rank()).orderBy(rank().desc()));
    assert.deepEqual(hits.column("id").sort(), [2, 4]);
    assert.ok(hits.rows.every((r) => typeof r.get("rank") === "number" && (r.get("rank") as number) > 0));
    const prefixed = await db.query(Query.table("pages").search("pages_search", [], "inv").matchPrefix().select("id", col("id")));
    assert.deepEqual(prefixed.column().sort(), [1, 2, 4]);
  });

  test("a result larger than a frame arrives whole", async () => {
    const [name, db] = await fresh();
    await db.createTable(new TableDef("big").column(new ColumnDef("id", Type.Int)).column(new ColumnDef("body", Type.Text)).primaryKey(["id"]));
    const body = "0123456789abcdef".repeat(8192); // 128 KiB
    for (let start = 0; start < 48; start += 8) {
      let insert = Insert.into("big").columns(["id", "body"]);
      for (let n = start; n < start + 8; n++) insert = insert.values([n, `${n}:${body}`]);
      await db.insert(insert);
    }
    const rows = await db.query(Query.table("big"));
    assert.deepEqual([rows.length, rows.affected], [48, 48]);
    assert.ok(rows.rows.every((row) => row.get("body") === `${row.int("id")}:${body}`));

    // On the wire it was several frames, all but the last marked so.
    const conn = await Conn.open(server.config(name));
    try {
      await conn.send({ kind: "query", query: Query.table("big").spec });
      const frames: (ast.Response & { kind: "rows" })[] = [];
      do {
        const frame = await conn.receive();
        assert.equal(frame.kind, "rows");
        if (frame.kind === "rows") frames.push(frame);
      } while (frames.at(-1)!.more);
      conn.settle();
      assert.ok(frames.length > 1);
      assert.ok(frames.every((f) => f.affected === 48n));
      assert.equal(
        frames.reduce((n, f) => n + f.rows.length, 0),
        48,
      );
    } finally {
      conn.close();
    }
  });

  test("a dump read slowly holds the server back", async () => {
    const [name, db] = await fresh();
    await db.createTable(new TableDef("blobs").column(new ColumnDef("id", Type.Int)).column(new ColumnDef("body", Type.Bytes)).primaryKey(["id"]));
    for (let id = 0; id < 16; id++) await db.insert(Insert.into("blobs").set("id", id).set("body", randomBytes(1 << 20)));

    // A dump taken a frame at a time, slower than the server sends it:
    // what has arrived and not been read stays near the high-water mark
    // instead of growing to the whole dump.
    const conn = await Conn.open(server.config(name));
    const unread = (): number => (conn as unknown as { buffered: number }).buffered;
    let most = 0;
    let total = 0;
    try {
      await conn.send({ kind: "admin", admin: { kind: "dump", databases: [] } });
      for (;;) {
        const frame = await conn.receive();
        if (frame.kind !== "chunk") {
          assert.equal(frame.kind, "ok");
          break;
        }
        total += frame.bytes.length;
        await new Promise((resolve) => setTimeout(resolve, 20));
        most = Math.max(most, unread());
      }
      conn.settle();
    } finally {
      conn.close();
    }
    assert.ok(total > 16 << 20, String(total));
    assert.ok(most < 4 << 20, `${most} bytes waited unread`);

    // Admin.dump with a slow `write` gets the whole of it.
    let written = 0;
    const counted = await db.admin.dump(async (chunk) => {
      written += chunk.length;
      await new Promise((resolve) => setTimeout(resolve, 1));
    });
    assert.equal(counted, written);
    assert.ok(written > 16 << 20);
  });

  test("errors carry their code and what was broken", async () => {
    const [, db] = await fresh();
    await db.createTable(users());
    await db.createTable(orders());
    await db.insert(Insert.into("users").set("name", "ann").set("email", "ann@example.com"));
    const duplicate = await code(ErrorCode.UniqueViolation, db.insert(Insert.into("users").set("name", "ann")));
    assert.deepEqual([duplicate.constraint, duplicate.table], ["users_name_key", "users"]);
    assert.ok(duplicate.violates("users_name_key") && duplicate.isUniqueViolation);
    const primary = await code(ErrorCode.UniqueViolation, db.insert(Insert.into("users").set("id", 1).set("name", "other")));
    assert.equal(primary.constraint, PRIMARY);
    const expression = await code(ErrorCode.UniqueViolation, db.insert(Insert.into("users").set("name", "ann2").set("email", "ANN@example.com")));
    assert.equal(expression.constraint, "users_email");
    assert.equal((await code(ErrorCode.NotNullViolation, db.insert(Insert.into("users").set("email", "x@example.com")))).constraint, "name");
    assert.equal((await code(ErrorCode.CheckViolation, db.insert(Insert.into("users").set("name", "neg").set("age", -1)))).constraint, "age_not_negative");
    const orphan = Insert.into("orders").set("user_id", 99).set("day", day(1)).set("amount", 1);
    assert.equal((await code(ErrorCode.ForeignKeyViolation, db.insert(orphan))).constraint, "orders_user_id_fkey");
    await code(ErrorCode.TypeMismatch, db.insert(Insert.into("users").set("name", 5)));
    await code(ErrorCode.TypeMismatch, db.query(Query.table("users").filter(col("name").eq(5))));
    await code(ErrorCode.NotFound, db.query(Query.table("nothing")));
    await code(ErrorCode.NotFound, db.query(Query.table("users").index("no_such_index", KeyRange.all())));
    await code(ErrorCode.Arithmetic, db.query(Query.row().select("x", val(1).div(0))));
    await code(ErrorCode.OutOfRange, db.insert(Insert.into("orders").set("user_id", 1).set("day", day(1)).set("amount", Decimal.parse("1e14"))));
    // An error is an answer: the connection carries on, and nothing was written.
    assert.deepEqual((await db.query(Query.table("users"))).column("name"), ["ann"]);
    // A delete takes its children with it, as the foreign key says.
    await db.insert(Insert.into("orders").set("user_id", 1).set("day", day(1)).set("amount", 1));
    assert.equal((await db.delete(Delete.from("users").key([1]))).affected, 1);
    assert.equal((await db.query(Query.table("orders"))).length, 0);
  });

  async function accounts(): Promise<[string, Client]> {
    const [name, db] = await fresh();
    await db.createTable(new TableDef("accounts").column(new ColumnDef("id", Type.Int)).column(new ColumnDef("balance", Type.Int)).primaryKey(["id"]));
    await db.insert(Insert.into("accounts").columns(["id", "balance"]).values([1, 100]).values([2, 100]));
    return [name, db];
  }

  const balance = async (db: Db, account: number): Promise<number> => (await db.get("accounts", [account]))!.int("balance");
  const setBalance = (account: number, value: number): Update => Update.table("accounts").key([account]).set("balance", value);
  const bump = Update.table("accounts").key([1]).set("balance", col("balance").add(1));

  test("commit and rollback", async () => {
    const [, db] = await accounts();
    let tx = await db.begin();
    await tx.update(setBalance(1, 50));
    assert.equal(await balance(tx, 1), 50);
    // Nobody else sees it until the commit.
    assert.equal(await balance(db, 1), 100);
    await tx.commit();
    assert.equal(await balance(db, 1), 50);
    assert.ok(tx.ended);
    await code(ErrorCode.TransactionAborted, tx.query(Query.table("accounts")));
    await code(ErrorCode.TransactionAborted, tx.commit());

    tx = await db.beginOptimistic();
    await tx.update(setBalance(1, 0));
    await tx.rollback();
    assert.equal(await balance(db, 1), 50);
    assert.equal(await db.exclusive(async (tx) => (await tx.update(setBalance(1, 60))).affected), 1);
    assert.equal(await balance(db, 1), 60);
    await assert.rejects(
      db.exclusive(async (tx) => {
        await tx.update(setBalance(1, 0));
        throw new Error("changed my mind");
      }),
      /changed my mind/,
    );
    assert.equal(await balance(db, 1), 60);
    // Requests made at once on one transaction go one after another.
    const both = await db.exclusive((tx) => Promise.all([balance(tx, 1), balance(tx, 2)]));
    assert.deepEqual(both, [60, 100]);
  });

  test("a failed statement ends the transaction unless a savepoint was taken", async () => {
    const [, db] = await accounts();
    let tx = await db.begin();
    await tx.update(setBalance(1, 10));
    await tx.savepoint("before");
    await code(ErrorCode.UniqueViolation, tx.insert(Insert.into("accounts").set("id", 1).set("balance", 0)));
    await code(ErrorCode.TransactionAborted, tx.query(Query.table("accounts")));
    await tx.rollbackTo("before");
    await tx.update(setBalance(2, 20));
    await tx.savepoint("again");
    await tx.update(setBalance(2, 0));
    await tx.rollbackTo("again");
    await tx.release("again");
    await tx.commit();
    assert.deepEqual([await balance(db, 1), await balance(db, 2)], [10, 20]);

    tx = await db.begin();
    await code(ErrorCode.NotFound, tx.query(Query.table("nothing")));
    await code(ErrorCode.TransactionAborted, tx.commit());
    assert.ok(tx.ended);
  });

  test("a transaction that conflicts is run again", async () => {
    const [name, db] = await accounts();
    const other = await another(name);

    // By hand: what this one read was changed by a commit made beside it.
    const tx = await db.beginOptimistic();
    const seen = await balance(tx, 1);
    await other.update(bump);
    await tx.update(setBalance(1, seen - 30));
    assert.equal(tx.conflicted, false);
    await code(ErrorCode.Conflict, tx.commit());
    assert.equal(tx.conflicted, true);
    assert.equal(await balance(db, 1), 101);

    // `transaction` does the running again by itself.
    let runs = 0;
    const transferred = await db.transaction(async (tx: Transaction) => {
      runs++;
      const available = await balance(tx, 1);
      // Another writer gets in between the read and the write.
      if (runs === 1) await other.update(bump);
      await tx.update(setBalance(1, available - 30));
      await tx.update(Update.table("accounts").key([2]).set("balance", col("balance").add(30)));
      return available;
    });
    assert.deepEqual([transferred, runs], [102, 2]);
    assert.deepEqual([await balance(db, 1), await balance(db, 2)], [72, 130]);

    // Beaten twice, the third run is the database's only writer: it
    // cannot conflict.
    runs = 0;
    await db.transaction(async (tx) => {
      runs++;
      const available = await balance(tx, 1);
      if (runs <= 2) await other.update(bump);
      await tx.update(setBalance(1, available - 1));
    });
    assert.equal(runs, 3);
    assert.equal(await balance(db, 1), 73);

    // An error of the work's own is not a conflict: it is thrown, once.
    runs = 0;
    await assert.rejects(
      db.transaction(async (tx) => {
        runs++;
        await tx.update(setBalance(2, 0));
        throw new RangeError("no");
      }),
      RangeError,
    );
    assert.deepEqual([runs, await balance(db, 2)], [1, 130]);
  });

  test("a transaction that conflicts before its commit is run again", async () => {
    const [name, db] = await accounts();
    const other = await another(name);
    // More than eight rows in one insert is more than the server does
    // twice: a transaction asked for it becomes the database's writer
    // there and then, running what it did again on the newest version.
    // If another commit has changed what it read, that fails with
    // Conflict at the insert, before any commit.
    const many = (from: number): Insert => {
      let insert = Insert.into("accounts").columns(["id", "balance"]);
      for (let id = from; id < from + 9; id++) insert = insert.values([id, 0]);
      return insert;
    };

    const tx = await db.beginOptimistic();
    await balance(tx, 1);
    await other.update(bump);
    await code(ErrorCode.Conflict, tx.insert(many(10)));
    assert.equal(tx.conflicted, true);
    await code(ErrorCode.Conflict, tx.commit());

    // `transaction` runs the work again when that happens.
    let runs = 0;
    await db.transaction(async (tx) => {
      runs++;
      const available = await balance(tx, 1);
      if (runs === 1) await other.update(bump);
      await tx.insert(many(10 * runs));
      await tx.update(setBalance(1, available - 1));
    });
    assert.equal(runs, 2);
    assert.equal(await balance(db, 1), 101);
    const ids = (await db.query(Query.table("accounts").filter(col("id").ge(10)))).column("id");
    assert.deepEqual(ids, [20, 21, 22, 23, 24, 25, 26, 27, 28]);
  });

  test("a reading transaction sees one moment", async () => {
    const [, db] = await accounts();
    const tx = await db.beginRead();
    assert.equal(await balance(tx, 1), 100);
    await db.update(setBalance(1, 7));
    await db.insert(Insert.into("accounts").set("id", 3).set("balance", 1));
    assert.equal(await balance(tx, 1), 100);
    assert.equal((await tx.query(Query.table("accounts"))).length, 2);
    await code(ErrorCode.InvalidRequest, tx.update(setBalance(1, 0)));
    const moment = await tx.now();
    await new Promise((resolve) => setTimeout(resolve, 10));
    assert.ok((await tx.now()).equals(moment));
    await tx.rollback();
    assert.equal(await balance(db, 1), 7);
    const total = await db.read(async (tx) => (await tx.query(Query.table("accounts"))).rows.reduce((n, row) => n + row.int("balance"), 0));
    assert.equal(total, 108);
  });

  test("many tasks share the pool", async () => {
    const [name] = await accounts();
    const db = await another(name, { poolSize: 3 });
    const work = async (): Promise<void> => {
      for (let i = 0; i < 10; i++) {
        await db.transaction(async (tx) => {
          await tx.update(setBalance(2, (await balance(tx, 2)) + 1));
        });
      }
    };
    await Promise.all(Array.from({ length: 8 }, work));
    assert.equal(await balance(db, 2), 180);
  });

  test("a named lock is held by one at a time", async () => {
    const [name, db] = await fresh();
    const other = await another(name);
    const guard = await db.tryLock("migrate");
    assert.ok(guard);
    assert.equal(guard.name, "migrate");
    assert.equal(await other.tryLock("migrate"), null);
    const elsewhere = await other.tryLock("something else");
    assert.ok(elsewhere);
    await elsewhere.unlock();

    const began = Date.now();
    let waited: number | null = null;
    const waiting = other.lock("migrate").then(async (held) => {
      waited = Date.now() - began;
      await held.unlock();
    });
    await new Promise((resolve) => setTimeout(resolve, 200));
    assert.equal(waited, null);
    await guard.unlock();
    await waiting;
    assert.ok(waited !== null && waited > 150);
    const again = await db.lock("migrate");
    assert.equal(await other.tryLock("migrate"), null);
    await again.unlock();
    await again.unlock();
  });

  test("a statement past its deadline is stopped", async () => {
    const [name, db] = await fresh();
    await db.createTable(new TableDef("big").column(new ColumnDef("id", Type.Int)).column(new ColumnDef("text", Type.Text)).primaryKey(["id"]));
    for (let batch = 0; batch < 8; batch++) {
      let insert = Insert.into("big").columns(["id", "text"]);
      for (let n = 0; n < 500; n++) insert = insert.values([batch * 500 + n, "lorem ipsum dolor sit amet ".repeat(40)]);
      await db.insert(insert);
    }
    // Every row is read and matched against a pattern that never matches.
    const never = col("text").ilike("%a%b%c%d%e%f%g%h%zzz%");
    const slow = Query.table("big").filter(never).select("n", count());
    let quickest = Infinity;
    for (let i = 0; i < 3; i++) {
      const began = performance.now();
      assert.equal((await db.queryOne(slow)).get("n"), 0);
      quickest = Math.min(quickest, performance.now() - began);
    }
    // The deadline must fall while the statement runs, however fast the machine is.
    const hurried = await another(name, { statementTimeout: Math.max(quickest / 8, 1) / 1000 });
    await code(ErrorCode.Timeout, hurried.query(slow));
    await code(ErrorCode.Timeout, hurried.update(Update.table("big").filter(never.not()).set("text", "x")));
    // The connection is still good, and nothing was changed.
    assert.equal(((await hurried.get("big", [1]))?.get("text") as string).length, 27 * 40);
  });

  test("a connection left mid-answer is not used again", async () => {
    const [name] = await fresh();
    const conn = await Conn.open(server.config(name));
    try {
      await conn.send({ kind: "ping" });
      await code(ErrorCode.Connection, conn.call({ kind: "ping" }));
    } finally {
      conn.close();
    }
  });

  test("databases and passwords", async () => {
    const [name, db] = await fresh();
    const admin = await server.admin();
    opened.push(admin);
    const at = `127.0.0.1:${server.port}`;
    assert.ok((await admin.admin.listDatabases()).includes(name));
    const other = `${name}_other`;
    await admin.admin.createDatabase(other, "first");
    await code(ErrorCode.AlreadyExists, admin.admin.createDatabase(other, "first"));
    await admin.admin.createDatabaseIfNotExists(other, "ignored");
    (await Client.connect(`database://${other}:first@${at}`)).close();
    await admin.admin.setPassword(other, "s3cr@t/2");
    await code(ErrorCode.AccessDenied, Client.connect(`database://${other}:first@${at}`));
    (await Client.connect(`database://anything:s3cr%40t%2F2@${at}/${other}?pool=1&name=test`)).close();
    await code(ErrorCode.AccessDenied, Client.connect(`database://nothing:${PASSWORD}@${at}`));
    // A database's own password does not administer the server.
    await code(ErrorCode.AccessDenied, db.admin.listDatabases());
    await code(ErrorCode.AccessDenied, db.admin.createDatabase("x", "y"));
    await admin.admin.dropDatabase(other);
    await admin.admin.dropDatabaseIfExists(other);
    await code(ErrorCode.NotFound, admin.admin.dropDatabase(other));
    assert.ok(!(await admin.admin.listDatabases()).includes(other));
    const unreachable = await code(ErrorCode.Connection, Client.connect("database://a:b@127.0.0.1:1"));
    assert.match(unreachable.message, /127\.0\.0\.1:1/);

    // A connect timeout longer than a timer holds is waited in full, not
    // cut to a millisecond.
    const overflows: Error[] = [];
    const warned = (w: Error): void => void (w.name === "TimeoutOverflowWarning" && overflows.push(w));
    process.on("warning", warned);
    try {
      for (let i = 0; i < 20; i++) (await Client.connect(`database://${PASSWORD}@${at}/${name}?connect_timeout=3000000`)).close();
      await new Promise((resolve) => setImmediate(resolve));
    } finally {
      process.off("warning", warned);
    }
    assert.deepEqual(overflows, []);
  });

  test("check, checkpoint, stats, dump and restore", async () => {
    const [name, db] = await fresh();
    await db.createTable(users());
    await db.insert(Insert.into("users").columns(["name", "age"]).values(["ann", 30]).values(["bob", 40]));
    await db.admin.checkpoint();
    assert.match(await db.admin.check(), /users/);
    assert.match(await db.admin.stats(), /optimistic transactions/);

    const pieces: Uint8Array[] = [];
    const written = await db.admin.dump((chunk) => void pieces.push(chunk));
    const dump = Buffer.concat(pieces);
    assert.equal(written, dump.length);
    assert.ok(written > 0);

    const admin = await server.admin();
    opened.push(admin);
    await db.delete(Delete.from("users").key([1]));
    await code(ErrorCode.AlreadyExists, admin.admin.restore(dump));
    await admin.admin.restore([dump.subarray(0, 100), dump.subarray(100)], true);
    // The database was replaced under the old connections: they are told
    // to reconnect, and a new client reads what the dump held.
    await code(ErrorCode.Unavailable, db.query(Query.table("users")));
    const restored = await Client.connect(server.config(name));
    opened.push(restored);
    assert.deepEqual((await restored.query(Query.table("users"))).column("name"), ["ann", "bob"]);
    await code(ErrorCode.InvalidRequest, admin.admin.restore(dump.subarray(0, written >> 1), true));
    assert.deepEqual((await restored.query(Query.table("users"))).column("name"), ["ann", "bob"]);
  });

  test("a pooled connection from before a restart is replaced", async () => {
    const own = await Server.start(path!);
    try {
      const db = await own.client("kept");
      await db.createTable(new TableDef("t").column(new ColumnDef("id", Type.Int)).primaryKey(["id"]));
      await db.insert(Insert.into("t").set("id", 1));
      await own.restart();
      // The pooled connection was closed by the server, and the pool saw
      // it close: the next request gets a new one, and what was
      // committed is there.
      assert.deepEqual((await db.query(Query.table("t"))).column(), [1]);
      db.close();
    } finally {
      await own.stop();
    }
  });
});

describe("against a server that serves TLS", { skip }, () => {
  /** One that serves nothing in the clear, one that serves both, and one that serves no TLS. */
  let required: Server;
  let optional: Server;
  let open: Server;
  const opened: Client[] = [];

  before(async () => {
    [required, optional, open] = await Promise.all([Server.start(path!, "required"), Server.start(path!, "optional"), Server.start(path!)]);
    for (const server of [required, optional, open]) (await server.client("test")).close();
  });

  after(async () => {
    for (const client of opened) client.close();
    await Promise.all([required, optional, open].map((server) => server?.stop()));
  });

  async function connect(target: string | Config): Promise<Client> {
    const db = await Client.connect(target);
    opened.push(db);
    return db;
  }

  const url = (server: Server, host: string, settings: string): string => `database://test:${PASSWORD}@${host}:${server.port}?${settings}`;
  const ca = `tls_ca=${encodeURIComponent(fixture("ca.pem"))}`;

  // R-O8: everything a connection does, it does under TLS.
  test("a client that verifies the server runs its statements under TLS", async () => {
    const db = await connect(url(required, "localhost", ca));
    await db.createTable(new TableDef("notes").column(new ColumnDef("id", Type.Int)).column(new ColumnDef("body", Type.Text)).primaryKey(["id"]));
    // A result of several frames, each far larger than a TLS record, and
    // requests as large.
    const body = "y".repeat(500_000);
    for (let id = 0; id < 12; id++) await db.insert(Insert.into("notes").set("id", id).set("body", `${id}:${body}`));
    const rows = await db.query(Query.table("notes"));
    assert.equal(rows.length, 12);
    assert.ok(rows.rows.every((row) => row.get("body") === `${row.int("id")}:${body}`));
    await db.transaction(async (tx) => {
      const row = await tx.queryOne(Query.table("notes").key([3]));
      await tx.update(Update.table("notes").key([3]).set("body", `${String(row.get("body")).slice(0, 4)}!`));
    });
    assert.equal((await db.queryOne(Query.table("notes").key([3]))).get("body"), "3:yy!");

    const conn = await Conn.open(required.config("test"));
    try {
      await conn.send({ kind: "query", query: Query.table("notes").spec });
      let frames = 0;
      for (let more = true; more; frames++) {
        const frame = await conn.receive();
        assert.equal(frame.kind, "rows");
        more = frame.kind === "rows" && frame.more;
      }
      conn.settle();
      assert.ok(frames > 1);
    } finally {
      conn.close();
    }

    // The certificate carries the loopback address too, and a name the
    // host is not reached by can be given.
    const byAddress = await connect(url(required, "127.0.0.1", ca));
    assert.equal((await byAddress.query(Query.table("notes").key([3]))).length, 1);
    const byName = await connect(url(required, "127.0.0.1", `${ca}&tls_name=database.test`));
    assert.equal((await byName.query(Query.table("notes").key([3]))).length, 1);
    const byNamedAddress = await connect({ ...required.config("test"), host: "localhost", tls: { ca: fixture("ca.pem"), name: "::1" } });
    assert.equal((await byNamedAddress.query(Query.table("notes").key([3]))).length, 1);
  });

  // R-O9: with TLS required, a greeting in the clear is refused whatever
  // password it carries, and the answer says why.
  test("a server that requires TLS refuses a client in the clear", async () => {
    for (const password of [PASSWORD, "not-the-password"]) {
      const refused = await code(ErrorCode.AccessDenied, Client.connect(required.config("test", password, null)));
      assert.match(refused.message, /only under TLS/);
    }
  });

  // R-O9: where TLS is optional, one port serves both, and both see the
  // same data.
  test("a server where TLS is optional serves both", async () => {
    const secured = await connect(optional.config("test"));
    const clear = await connect(optional.config("test", PASSWORD, null));
    await secured.createTable(new TableDef("notes").column(new ColumnDef("id", Type.Int)).primaryKey(["id"]));
    await secured.insert(Insert.into("notes").set("id", 1));
    await clear.insert(Insert.into("notes").set("id", 2));
    assert.deepEqual((await secured.query(Query.table("notes"))).column(), [1, 2]);
    assert.deepEqual((await clear.query(Query.table("notes"))).column(), [1, 2]);
  });

  // R-O8: a server the client cannot verify is not sent a greeting, also
  // where the server would have served the client in the clear.
  test("a client refuses a server it cannot verify", async () => {
    for (const server of [required, optional]) {
      for (const settings of [
        // Signed by an authority the client was not given.
        `tls_ca=${encodeURIComponent(fixture("stranger-ca.pem"))}`,
        // The system's authorities have not signed it either.
        "tls=on",
        // The right authority, for a name the certificate does not carry.
        `${ca}&tls_name=elsewhere.test`,
      ]) {
        const refused = await code(ErrorCode.Connection, Client.connect(url(server, "localhost", settings)));
        assert.match(refused.message, new RegExp(`the TLS handshake with the database at localhost:${server.port} failed: .`), settings);
      }
    }
    const unread = await code(ErrorCode.Connection, Client.connect(url(required, "localhost", "tls_ca=%2Fnowhere%2Fca.pem")));
    assert.match(unread.message, /TLS cannot be set up: .*\/nowhere\/ca\.pem/);
    const empty = await code(ErrorCode.Connection, Client.connect(url(required, "localhost", `tls_ca=${encodeURIComponent(fixture("server.key"))}`)));
    assert.match(empty.message, /TLS cannot be set up: .*server\.key holds no certificate/);
  });

  // R-O8: a client told to use TLS never talks in the clear instead.
  test("a client asked for TLS does not talk to a server that serves none", async () => {
    const refused = await code(ErrorCode.Connection, Client.connect(url(open, "127.0.0.1", ca)));
    assert.match(refused.message, /the TLS handshake with the database at 127\.0\.0\.1:\d+ failed: .*does not serve TLS/);
    // The same client without the setting is served: it was the TLS.
    const clear = await connect(open.config("test"));
    await clear.ping();
  });
});
