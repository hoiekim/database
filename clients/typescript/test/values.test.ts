// How JavaScript values stand for the database's, and the client's own
// parts that need no server: URLs and rows.

import assert from "node:assert/strict";
import { test } from "node:test";

import { Reader, Writer, getValue, putValue } from "../src/codec.js";
import { CalendarDate, DbError, Decimal, ErrorCode, Rows, Timestamp, Uuid, configFromUrl, float, fromWire, json, toWire, type Value } from "../src/index.js";

function back(value: Value): unknown {
  const w = new Writer();
  putValue(w, toWire(value));
  const r = new Reader(w.bytes());
  const read = getValue(r);
  r.finish();
  return fromWire(read);
}

const kind = (value: Value): string => toWire(value).kind;

test("each JavaScript type has one kind", () => {
  assert.equal(kind(null), "null");
  assert.equal(kind(true), "bool");
  assert.equal(kind(7), "int");
  assert.equal(kind(7n), "int");
  assert.equal(kind(7.5), "float");
  assert.equal(kind(float(7)), "float");
  assert.equal(kind(Decimal.parse("7")), "decimal");
  assert.equal(kind("7"), "text");
  assert.equal(kind(new Uint8Array([7])), "bytes");
  assert.equal(kind(Buffer.from([7])), "bytes");
  assert.equal(kind(Uuid.parse("6f9619ff-8b86-d011-b42d-00c04fc964ff")), "uuid");
  assert.equal(kind(new Timestamp(7n)), "timestamp");
  assert.equal(kind(new Date(7)), "timestamp");
  assert.equal(kind(new CalendarDate(7)), "date");
  assert.equal(kind({ seven: 7 }), "json");
  assert.equal(kind(json([7])), "json");
  assert.equal(kind(json("7")), "json");
  assert.equal(kind([7]), "list");
  for (const bad of [undefined, () => 1, Symbol("x"), new Map(), new (class Thing {})()]) {
    assert.throws(() => toWire(bad as never), TypeError);
  }
});

test("a number is an int exactly when it is a safe integer", () => {
  assert.deepEqual(toWire(2 ** 53 - 1), { kind: "int", value: 9007199254740991n });
  assert.deepEqual(toWire(-0), { kind: "int", value: 0n });
  for (const n of [2 ** 53, 0.5, NaN, Infinity, 1e300]) assert.equal(kind(n), "float");
  // Read back: a number when it is safe, a bigint when it is not.
  assert.equal(back(2 ** 53 - 1), 2 ** 53 - 1);
  assert.equal(back(-(2 ** 53) + 1), -(2 ** 53) + 1);
  assert.equal(back(2n ** 53n), 2n ** 53n);
  assert.equal(back(2n ** 63n - 1n), 2n ** 63n - 1n);
  assert.equal(back(-(2n ** 63n)), -(2n ** 63n));
  assert.equal(back(5n), 5);
  assert.throws(() => toWire(2n ** 63n), RangeError);
  assert.equal(back(float(2)), 2);
  assert.ok(Number.isNaN(back(NaN)));
  assert.ok(Object.is(back(float(-0)), -0));
});

test("decimals are exact", () => {
  for (const text of ["0", "-12345.67", "0.000001", "5.00", "123456789012345678.90", "170141183460469231731687303715884105727", "-0.000000000000000000000000000001"]) {
    const read = back(Decimal.parse(text));
    assert.ok(read instanceof Decimal);
    assert.equal(read.toString(), text);
  }
  assert.equal(Decimal.parse(".5").toString(), "0.5");
  assert.equal(Decimal.parse("1e3").toString(), "1000");
  assert.equal(Decimal.parse("2.5E-4").toString(), "0.00025");
  assert.equal(Decimal.parse("+7").toString(), "7");
  assert.equal(Decimal.parse("12.50").toNumber(), 12.5);
  assert.ok(Decimal.parse("1.50").equals(Decimal.parse("1.5")));
  assert.equal(JSON.stringify({ amount: Decimal.parse("1.50") }), '{"amount":"1.50"}');
  for (const bad of ["", ".", "-", "1.2.3", "abc", "1e", "0x10"]) assert.throws(() => Decimal.parse(bad), SyntaxError, bad);
  for (const bad of ["170141183460469231731687303715884105728", "1234567890123456789012345678901234567890", "1e39", "1e2147483648"]) {
    assert.throws(() => Decimal.parse(bad), RangeError, bad);
  }
});

test("decimals are read from text as the server reads them", () => {
  // protocol/src/decimal.rs: digits after the point beyond what can be
  // kept are rounded away, halves away from zero.
  assert.ok(Decimal.parse("-2.00000000000000000000000000000049").equals(Decimal.parse("-2")));
  assert.equal(Decimal.parse("12345678901234567890123456789012345.123456").toString(), "12345678901234567890123456789012345.12");
  assert.equal(Decimal.parse("0.1234567890123456789012345678905555555555").toString(), "0.123456789012345678901234567891");
  assert.equal(Decimal.parse("-0.0000000000000000000000000000005").toString(), "-0.000000000000000000000000000001");
  assert.ok(Decimal.parse("1e-31").equals(Decimal.parse("0")));
  assert.equal(Decimal.parse("1.5e-30").toString(), "0.000000000000000000000000000002");
  assert.equal(Decimal.parse("1e38").toString(), "1" + "0".repeat(38));
  // A large exponent is refused before its power of ten is worked out.
  const started = Date.now();
  assert.throws(() => Decimal.parse("1e100000000"), RangeError);
  assert.throws(() => Decimal.parse("0e100000000"), RangeError);
  assert.ok(Date.now() - started < 1000);
});

test("timestamps are microseconds in UTC", () => {
  const t = new Timestamp(1_772_706_030_123_456n);
  assert.equal(t.toISOString(), "2026-03-05T10:20:30.123456Z");
  assert.equal(t.toDate().toISOString(), "2026-03-05T10:20:30.123Z");
  assert.equal(new Timestamp(-1n).toISOString(), "1969-12-31T23:59:59.999999Z");
  assert.equal(new Timestamp(-1n).millis(), -1n);
  assert.equal(new Timestamp(0n).toISOString(), "1970-01-01T00:00:00.000000Z");
  assert.equal(Timestamp.fromDate(new Date("2026-03-05T12:20:30.123+02:00")).micros, 1_772_706_030_123_000n);
  const read = back(t);
  assert.ok(read instanceof Timestamp && read.equals(t));
  const fromDate = back(new Date(1500));
  assert.ok(fromDate instanceof Timestamp && fromDate.micros === 1_500_000n);
  assert.equal(JSON.stringify([t]), '["2026-03-05T10:20:30.123456Z"]');
  assert.throws(() => Timestamp.fromDate(new Date(NaN)), RangeError);
  assert.throws(() => new Timestamp(2n ** 63n), RangeError);
  // Held exactly even where a Date cannot follow.
  const far = back(new Timestamp(2n ** 63n - 1n));
  assert.ok(far instanceof Timestamp && far.micros === 2n ** 63n - 1n);
  assert.throws(() => far.toDate(), RangeError);
  // Printed as protocol/src/time.rs prints it, wherever a Date cannot follow.
  assert.equal(far.toISOString(), "294247-01-10T04:00:54.775807Z");
  assert.equal(new Timestamp(-(2n ** 63n)).toISOString(), "-290308-12-21T19:59:05.224192Z");
  assert.equal(new Timestamp(253_402_300_800_000_000n).toISOString(), "10000-01-01T00:00:00.000000Z");
  assert.equal(new Timestamp(-62_135_596_800_000_001n).toISOString(), "0000-12-31T23:59:59.999999Z");
});

test("dates are days", () => {
  assert.equal(CalendarDate.fromYmd(2026, 3, 5).days, 20_517);
  assert.equal(CalendarDate.parse("2024-02-29").days, 19_782);
  assert.equal(new CalendarDate(-1).toString(), "1969-12-31");
  assert.equal(CalendarDate.parse("0001-01-01").toString(), "0001-01-01");
  assert.deepEqual(new CalendarDate(20_517).ymd(), [2026, 3, 5]);
  assert.equal(CalendarDate.fromDate(new Date("2026-03-05T23:59:59Z")).days, 20_517);
  for (const bad of ["2026-02-30", "2026-13-01"]) assert.throws(() => CalendarDate.parse(bad), RangeError, bad);
  assert.throws(() => CalendarDate.parse("2026-3-5"), SyntaxError);
  const read = back(new CalendarDate(2 ** 31 - 1));
  assert.ok(read instanceof CalendarDate && read.days === 2 ** 31 - 1);
  // Printed as protocol/src/time.rs prints it, beyond the years a Date holds too.
  assert.equal(CalendarDate.fromYmd(-7, 12, 21).days, -721_730);
  assert.equal(CalendarDate.fromYmd(-7, 12, 21).toString(), "-007-12-21");
  assert.deepEqual(CalendarDate.fromYmd(-7, 12, 21).ymd(), [-7, 12, 21]);
  assert.equal(CalendarDate.fromYmd(10000, 1, 1).toString(), "10000-01-01");
  assert.equal(new CalendarDate(200_000_000).toString(), "549551-05-28");
  assert.equal(new CalendarDate(2 ** 31 - 1).toString(), "5881580-07-11");
  assert.equal(new CalendarDate(-(2 ** 31)).toString(), "-5877641-06-23");
});

test("uuids print in the usual form", () => {
  const u = Uuid.parse("6F9619FF-8B86-D011-B42D-00C04FC964FF");
  assert.equal(u.toString(), "6f9619ff-8b86-d011-b42d-00c04fc964ff");
  assert.ok(Uuid.parse("6f9619ff8b86d011b42d00c04fc964ff").equals(u));
  assert.equal(`${u}`, "6f9619ff-8b86-d011-b42d-00c04fc964ff");
  for (const bad of ["6f9619ff-8b86-d011-b42d-00c04fc964f", "6f9619ff8-b86-d011-b42d-00c04fc964ff", ""]) assert.throws(() => Uuid.parse(bad), SyntaxError);
  const read = back(u);
  assert.ok(read instanceof Uuid && read.equals(u));
});

test("JSON is written compactly and read parsed", () => {
  assert.deepEqual(toWire({ a: [1, 2.5, null, { b: "c" }], é: 'ü\n"\\' }), { kind: "json", text: '{"a":[1,2.5,null,{"b":"c"}],"é":"ü\\n\\"\\\\"}' });
  assert.deepEqual(back(json([1, "two"])), [1, "two"]);
  assert.equal(back(json(null)), null);
  assert.equal(back(json("text")), "text");
  assert.throws(() => toWire(json(undefined)), TypeError);
  // Half a surrogate pair is not text: UTF-8 has no form for it.
  assert.equal(back("a\ud83d\ude00b"), "a\ud83d\ude00b");
  for (const bad of [json("a\ud800b"), { "\udc00": 1 }, { a: ["\ud800"] }]) assert.throws(() => toWire(bad), RangeError);
  assert.throws(() => back("a\ud800b"), RangeError);
  assert.throws(() => back("\udc00"), RangeError);
  assert.deepEqual(back([1, "a", null, [true]]), [1, "a", null, [true]]);
  const bytes = back(new Uint8Array([0, 255, 7]));
  assert.ok(bytes instanceof Uint8Array && !Buffer.isBuffer(bytes));
  assert.deepEqual([...bytes], [0, 255, 7]);
});

test("reads a URL", () => {
  let c = configFromUrl("database://shop:s3cr%40t@db:6000?pool=3&statement_timeout=1.5&name=shop-1");
  assert.deepEqual([c.host, c.port, c.database, c.password], ["db", 6000, "shop", "s3cr@t"]);
  assert.deepEqual([c.poolSize, c.statementTimeout, c.name], [3, 1.5, "shop-1"]);
  c = configFromUrl("database://anything:pw@localhost/blog");
  assert.deepEqual([c.port, c.database], [5454, "blog"]);
  const admin = configFromUrl("database://adminpw@localhost");
  assert.deepEqual([admin.database, admin.password], ["", "adminpw"]);
  c = configFromUrl("database://d:a%3Ab%40c@h?connect_timeout=2&lock_wait=0.25");
  assert.deepEqual([c.password, c.connectTimeout, c.lockWait], ["a:b@c", 2, 0.25]);
  assert.equal(c.role, undefined);
  // A role is a setting, and the password before the host is then the role's.
  const role = configFromUrl("database://shop:rolepw@db?role=reader");
  assert.deepEqual([role.database, role.role, role.password], ["shop", "reader", "rolepw"]);
  assert.equal(configFromUrl("database://rolepw@db/shop?role=night%20reader").role, "night reader");
  for (const bad of [
    "database://a:b@h/d?statement_timeout=-1",
    "database://a:b@h/d?lock_wait=NaN",
    "database://a:b@h/d?connect_timeout=1e400",
    "postgres://a:b@h/d",
    "database://nohost",
    "database://a:b@:1/d",
    "database://a:b@h:x/d",
    "database://a:b@h/d?pool=0",
    "database://a:b@h/d?what=1",
    "database://a:%zz@h/d",
    "database://a:b@h/d?name",
    "database://a:b@h/d?role=",
    "database://a:b@h/d?role",
    // A role is a role of a database, and this URL names none.
    "database://adminpw@h?role=reader",
  ]) {
    assert.throws(
      () => configFromUrl(bad),
      (e) => e instanceof DbError && e.code === ErrorCode.InvalidRequest,
      bad,
    );
  }
});

test("rows and their columns", () => {
  const rows = new Rows({
    columns: ["id", "name"],
    rows: [
      [toWire(1), toWire("ann")],
      [toWire(2n ** 60n), toWire("bob")],
    ],
    affected: 2n,
    total: 10n,
  });
  assert.deepEqual([rows.length, rows.affected, rows.total, rows.columns], [2, 2, 10, ["id", "name"]]);
  const first = rows.rows[0]!;
  assert.deepEqual([first.get("id"), first.at(1), first.int("id"), first.bigint("id")], [1, "ann", 1, 1n]);
  assert.deepEqual(first.toObject(), { id: 1, name: "ann" });
  assert.throws(() => first.get("nothing"), DbError);
  assert.throws(() => first.int("name"), DbError);
  const second = rows.rows[1]!;
  assert.equal(second.bigint("id"), 2n ** 60n);
  assert.throws(() => second.int("id"), DbError);
  assert.deepEqual(rows.column(), [1, 2n ** 60n]);
  assert.deepEqual(rows.column("name"), ["ann", "bob"]);
  assert.deepEqual([...rows].map((row) => row.get("name")), ["ann", "bob"]);
  assert.equal(rows.opt(), first);
  assert.throws(
    () => rows.one(),
    (e) => e instanceof DbError && e.code === ErrorCode.InvalidRequest,
  );
  const empty = new Rows({ columns: [], rows: [], affected: 0n, total: null });
  assert.equal(empty.opt(), null);
  assert.throws(
    () => empty.one(),
    (e) => e instanceof DbError && e.code === ErrorCode.NotFound,
  );
});
