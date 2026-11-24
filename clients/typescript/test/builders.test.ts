// The builders say what the Rust client's builders say.
//
// Each request here is written with this client's builders the way
// protocol/tests/vectors.rs writes it with the Rust ones, and must come
// out as the same bytes: so `Query.table(..).filter(..).filter(..)`,
// `KeyRange.within`, `returning` and the rest mean the same in both.

import assert from "node:assert/strict";
import { test } from "node:test";

import * as tables from "../src/ast.js";
import { encodeRequest } from "../src/codec.js";
import {
  Bound,
  ColumnDef,
  Delete,
  Expr,
  IndexDef,
  Insert,
  KeyRange,
  NULL,
  PRIMARY,
  Query,
  TableDef,
  Type,
  Update,
  Uuid,
  all,
  any,
  call,
  cases,
  col,
  count,
  days,
  depth,
  excluded,
  exists,
  likeEscape,
  list,
  listOf,
  max,
  minutes,
  now,
  old,
  rank,
  scalar,
  seconds,
  val,
  when,
  type ast,
} from "../src/index.js";
import { hex, load } from "./describe.js";

const vectors = new Map(load().vectors.map((v) => [v.name, v]));

function same(name: string, request: ast.Request): void {
  const vector = vectors.get(`request.${name}`);
  assert.ok(vector, name);
  assert.equal(hex(encodeRequest(request, BigInt(vector.timeout_ms!))), vector.hex, name);
}

const query = (name: string, q: Query): void => same(`query.${name}`, { kind: "query", query: q.spec });

function sampleQuery(): Query {
  return Query.table("mails")
    .alias("m")
    .index("mails_user_date", KeyRange.within([new Uuid(new Uint8Array(16).fill(1))], Bound.Unbounded, Bound.excluded([5])))
    .reverse()
    .join(Query.table("map").alias("x").key([col("m.user_id"), "INBOX", col("m.mail_id")]))
    .leftJoin(Query.table("users").alias("u").key([col("m.user_id")]).limit(1))
    .each("address", col("to").jsonPluck("address"))
    .filter(col("expunged").eq(false).and(col("subject").ilike("%hi%")).and(col("cc").isNull().not()))
    .filter(any([col("a").inList([1, 2]), exists(Query.table("t").key([col("m.id")]))]))
    .groupBy([col("user_id")])
    .having(count().gt(1))
    .orderBy(col("date").desc().nullsLast())
    .orderBy(max(col("date")).filter(col("read").eq(false)))
    .offset(3)
    .limit(10)
    .select("n", count().distinct())
    .select("score", when(col("x").gt(1), col("x").mul(2).add(1), NULL).cast(Type.Float))
    .select("j", col("doc").jsonText(["a", 0]))
    .select("s", scalar(Query.table("t").select("m", max(col("v")))))
    .distinct()
    .withTotal();
}

function sampleTable(): TableDef {
  return new TableDef("users")
    .column(new ColumnDef("id", Type.Uuid).defaultRandomUuid())
    .column(new ColumnDef("email", Type.Text).nullable())
    .column(new ColumnDef("n", Type.Int).autoIncrement())
    .column(new ColumnDef("amount", Type.decimal(15, 2)).default(0))
    .column(new ColumnDef("ratio", Type.Decimal).nullable())
    .column(new ColumnDef("tags", Type.list(Type.Text)).default([]))
    .column(new ColumnDef("grid", Type.list(Type.list(Type.Int))).nullable())
    .column(new ColumnDef("at", Type.Timestamp).defaultNow())
    .column(new ColumnDef("day", Type.Date).nullable())
    .column(new ColumnDef("flag", Type.Bool).default(false))
    .column(new ColumnDef("score", Type.Float).default(1.5))
    .column(new ColumnDef("blob", Type.Bytes).nullable())
    .column(new ColumnDef("doc", Type.Json).nullable())
    .column(new ColumnDef("account", Type.Uuid).nullable())
    .primaryKey(["id"])
    .unique("users_n", ["n"])
    .index(new IndexDef("users_email").onExpr(col("email").lower()).unique().filter(col("email").isNotNull()))
    .index(new IndexDef("users_tags").on(["id"]).onEach(col("tags")).onDesc("at"))
    .index(
      new IndexDef("users_text").on(["id"]).fullText("english", [
        [col("email"), 1.0],
        [col("doc").jsonText(["bio"]), 0.4],
      ]),
    )
    .index(new IndexDef("users_words").fullText("simple", [[col("email"), 1.0]]))
    .foreignKey(["id"], "accounts", "cascade")
    .foreignKey(["account"], "accounts", "set_null")
    .foreignKeyTo(["email"], "addresses", ["address"], "restrict")
    .check("positive", col("n").ge(0))
    .volatile();
}

test("queries", () => {
  const one = new Uuid(new Uint8Array(16).fill(1));
  query("table", Query.table("mails"));
  query("key", Query.table("mails").key([7]));
  query(
    "keys",
    Query.table("mails").keys([
      [1, "a"],
      [2, "b"],
    ]),
  );
  query("range", Query.table("mails").range(KeyRange.of(Bound.excluded([10]), Bound.Unbounded)).limit(50));
  query("index", Query.table("mails").index("mails_user_date", KeyRange.eq([one])).limit(50));
  query(
    "index_ranges",
    Query.table("mails").indexRanges("mails_user_date", [
      KeyRange.eq([1]),
      KeyRange.is([null]),
      KeyRange.all(),
      KeyRange.of(Bound.included([1, 2]), Bound.excluded([3])).nulls(),
    ]),
  );
  query("sample", sampleQuery());
  query("row", Query.row().select("taken", exists(Query.table("t").key([1]))).select("at", now()));
  query("columns", Query.table("mails").alias("m").columns(["m.subject", "date"]));
  query("ancestors", Query.table("pages").ancestors([[9]], "parent_id"));
  query(
    "descendants",
    Query.table("pages").descendants([[1], [2]], "parent_id", "pages_parent").prune(col("deleted_at").isNull()).stopAt(col("id").eq(9)).maxDepth(5).select("d", depth()),
  );
  query("search", Query.table("mails").search("mails_search", [1], "hello world").matchPrefix().orderBy(rank().desc()));
  query("order", Query.table("t").orderBy(col("a")).orderBy(col("b").desc()).orderBy(col("c").asc().nullsFirst()).orderBy(col("d").desc().nullsLast()));
});

test("every expression", () => {
  query(
    "every_expression",
    Query.table("t")
      .select("value", val("x"))
      .select("column", col("a"))
      .select("qualified", col("t.a"))
      .select("excluded", excluded("a"))
      .select("old", old("a"))
      .select("depth", depth())
      .select("rank", rank())
      .select("not", col("a").not())
      .select("and", all([]))
      .select("or", any([col("a"), col("b"), col("c")]))
      .select("is_null", col("a").isNull())
      .select("is_not_null", col("a").isNotNull())
      .select("in", col("a").inList(["x", "y"]))
      .select("not_in", col("a").notInList(listOf(Query.table("u").columns(["a"]))))
      .select("like", col("a").like("a\\%%"))
      .select("not_like", col("a").notLike("_"))
      .select("ilike", col("a").ilike("%A%"))
      .select("neg", col("a").neg())
      .select(
        "case",
        cases([
          [col("a").gt(1), "many"],
          [col("a").eq(1), "one"],
        ]),
      )
      .select("when", when(col("a").isNull(), 0, col("a")))
      .select("cast", col("a").cast(Type.decimal(15, 2)))
      .select("try_cast", col("a").tryCast(Type.list(Type.Uuid)))
      .select("list", list([col("a"), 1]).lt(list([2, 3])))
      .select("count", count())
      .select("count_distinct", count().distinct().filter(col("a").gt(0)))
      .select("scalar", scalar(Query.table("u").key([col("t.a")]).columns(["b"])))
      .select("exists", exists(Query.table("u").key([col("t.a")])))
      .select("between", col("a").between(1, 9))
      .select("span", now().sub(minutes(5))),
  );
});

test("every function and operator", () => {
  let every = Query.row();
  for (const name of Object.keys(tables.FUNCS) as ast.Func[]) every = every.select(name, call(name, [col("a")]));
  query("every_function", every);
  every = Query.table("t");
  const a = col("a").node;
  for (const op of Object.keys(tables.CMP_OPS) as ast.CmpOp[]) every = every.select(op, new Expr({ kind: "cmp", op, left: a, right: val(1).node }));
  for (const op of Object.keys(tables.ARITH_OPS) as ast.ArithOp[]) every = every.select(op, new Expr({ kind: "arith", op, left: a, right: val(2).node }));
  for (const func of Object.keys(tables.AGG_FUNCS) as ast.AggFunc[]) every = every.select(func, new Expr({ kind: "agg", func, arg: a, filter: null, distinct: false }));
  query("every_operator", every);
});

test("the small helpers", () => {
  assert.deepEqual(seconds(1.5).node, { kind: "value", value: { kind: "int", value: 1_500_000n } });
  assert.deepEqual(seconds(-0.0000005).node, { kind: "value", value: { kind: "int", value: -1n } });
  assert.deepEqual(days(1).node, { kind: "value", value: { kind: "int", value: 86_400_000_000n } });
  assert.equal(likeEscape("50%_\\"), "50\\%\\_\\\\");
  assert.throws(() => Query.table("t").key("abc" as never), TypeError);
  assert.throws(() => Query.table("t").key(7 as never), TypeError);
  assert.throws(() => val(undefined as never), TypeError);
  assert.throws(() => col("a").eq(Query.table("t") as never), TypeError);
  // A path is a list: a string would be spread into its letters.
  assert.deepEqual(col("d").json(["device"]).node, { kind: "call", func: "json_get", args: [col("d").node, val("device").node] });
  for (const path of ["device", 7]) {
    assert.throws(() => col("d").json(path as never), TypeError);
    assert.throws(() => col("d").jsonText(path as never), TypeError);
  }
});

test("a number past its field's width is refused, not wrapped", () => {
  const encode = (q: Query): Uint8Array => encodeRequest({ kind: "query", query: q.spec }, 0n);
  // A span beyond a signed 64-bit count of microseconds.
  for (const span of [() => seconds(1e13), () => days(1.1e8), () => seconds(NaN), () => minutes(-Infinity)]) assert.throws(span, RangeError);
  assert.deepEqual(seconds(9.2e12).node, { kind: "value", value: { kind: "int", value: 9_200_000_000_000_000_000n } });
  const walk = Query.table("t").ancestors([[1]], "parent");
  for (const depth of [-1, 2 ** 32, 1.5]) assert.throws(() => encode(walk.maxDepth(depth)), RangeError, String(depth));
  encode(walk.maxDepth(2 ** 32 - 1));
  assert.throws(() => encode(Query.table("t").select("x", col("a").cast(Type.decimal(300, 2)))), RangeError);
  // The same for values handed over as plain data.
  assert.throws(() => encode(Query.table("t").filter(new Expr({ kind: "value", value: { kind: "int", value: 2n ** 63n } }))), RangeError);
  assert.throws(() => encode(Query.table("t").filter(new Expr({ kind: "value", value: { kind: "decimal", mantissa: 2n ** 127n, scale: 0 } }))), RangeError);
});

test("writes", () => {
  same("insert.upsert", {
    kind: "insert",
    insert: Insert.into("t")
      .columns(["a", "b"])
      .values([1, now().add(seconds(5))])
      .values([2, null])
      .onConflictUpdate(PRIMARY)
      .update("b", excluded("b"))
      .updateExcluded(["a"])
      .updateWhere(col("b").lt(excluded("b")))
      .returning(["a"])
      .returningExpr("old_b", old("b")).spec,
  });
  same("insert.set", { kind: "insert", insert: Insert.into("users").set("username", "ann").set("age", 30).returning(["id", "created"]).spec });
  same("insert.from_query", { kind: "insert", insert: Insert.into("t").columns(["a"]).fromQuery(Query.table("s").columns(["a"])).onConflictDoNothing().spec });
  same("insert.do_nothing_on", { kind: "insert", insert: Insert.into("t").set("a", 1).onConflictDoNothingOn("t_a").returningAll().spec });
  same("insert.empty", { kind: "insert", insert: Insert.into("t").spec });
  same("update.rows", { kind: "update", update: Update.rows(sampleQuery()).set("hits", col("hits").add(1)).returning(["hits"]).returningExpr("was", old("hits")).spec });
  same("update.key", {
    kind: "update",
    update: Update.table("auth_codes").key([new Uint8Array([1, 2, 3])]).filter(col("used_at").isNull()).set("used_at", now()).returning(["m.client_id"]).spec,
  });
  same("delete.index", { kind: "delete", delete: Delete.from("t").index("t_a", KeyRange.eq([1])).filter(col("b").lt(now())).returning(["a"]).spec });
  same("delete.key", { kind: "delete", delete: Delete.from("access").key(["u", "svc"]).returningAll().spec });
});

test("a builder is not changed by what is built from it", () => {
  const base = Query.table("t").filter(col("a").eq(1));
  const narrowed = base.filter(col("b").eq(2)).limit(1);
  assert.equal(base.spec.limit, null);
  assert.equal(base.spec.filter?.kind, "cmp");
  assert.equal(narrowed.spec.filter?.kind, "and");
  const insert = Insert.into("t").set("a", 1);
  insert.set("b", 2);
  assert.deepEqual(insert.spec.columns, ["a"]);
});

test("schema", () => {
  same("ddl.create_table", { kind: "ddl", ddl: { kind: "create_table", def: sampleTable().spec, ifNotExists: true } });
  same("ddl.create_table.small", {
    kind: "ddl",
    ddl: { kind: "create_table", def: new TableDef("t").column(new ColumnDef("a", Type.Int)).primaryKey(["a"]).spec, ifNotExists: false },
  });
  same("ddl.add_column", { kind: "ddl", ddl: { kind: "add_column", table: "t", column: new ColumnDef("c", Type.Json).nullable().spec, ifNotExists: true } });
  same("ddl.create_index", { kind: "ddl", ddl: { kind: "create_index", table: "t", index: new IndexDef("i").on(["a"]).spec, ifNotExists: false } });
});
