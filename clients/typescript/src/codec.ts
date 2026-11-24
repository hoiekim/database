// How values, expressions, queries and definitions are written as bytes.
//
// This is docs/PROTOCOL.md in code, and the counterpart of the server's
// protocol/src/codec.rs: integers are little-endian or varints, strings
// and lists carry their length first, and every choice between kinds is
// one tag byte. protocol/fixtures/vectors.json holds messages and the
// bytes they must come out as; test/vectors.test.ts checks this file
// against it.

import * as ast from "./ast.js";
import { DbError, ErrorCode, errorCodeFromWire } from "./errors.js";
import { MAX_SCALE, type WireValue } from "./values.js";

/** The version of the protocol this client speaks. */
export const PROTOCOL_VERSION = 2;
/** The largest frame either side accepts. */
export const MAX_FRAME = 256 * 1024 * 1024;
/** How deep expressions, queries, lists and list types may nest. */
export const MAX_DEPTH = 100;

const U64_MAX = (1n << 64n) - 1n;
const I64_MIN = -(1n << 63n);
const I64_MAX = (1n << 63n) - 1n;
const I128_MIN = -(1n << 127n);
const I128_MAX = (1n << 127n) - 1n;

/** Refuses a number a fixed-width field would wrap: `maxDepth(-1)`, a precision of 300. */
function fits(v: number, max: number): number {
  if (!Number.isInteger(v) || v < 0 || v > max) throw new RangeError(`${v} is not a whole number from 0 to ${max}`);
  return v;
}

/** A lone surrogate, which UTF-8 cannot carry: `TextEncoder` would put U+FFFD in its place. */
const LONE_SURROGATE = /\p{Cs}/u;

/** The server sent something this client cannot read. */
export class ProtocolError extends DbError {
  constructor(what: string) {
    super(ErrorCode.Connection, `malformed message: ${what}`);
    this.name = "ProtocolError";
  }
}

function numbers<K extends string>(table: Readonly<Record<K, number>>): Map<number, K> {
  return new Map((Object.entries(table) as [K, number][]).map(([name, n]) => [n, name]));
}

const CMP_BY_NUMBER = numbers(ast.CMP_OPS);
const ARITH_BY_NUMBER = numbers(ast.ARITH_OPS);
const AGG_BY_NUMBER = numbers(ast.AGG_FUNCS);
const SUBQUERY_BY_NUMBER = numbers(ast.SUBQUERY_KINDS);
const FUNC_BY_NUMBER = numbers(ast.FUNCS);
const DIRECTION_BY_NUMBER = numbers(ast.DIRECTIONS);
const JOIN_BY_NUMBER = numbers(ast.JOIN_KINDS);
const ON_DELETE_BY_NUMBER = numbers(ast.ON_DELETE);
const TEXT_CONFIG_BY_NUMBER = numbers(ast.TEXT_CONFIGS);

function named<K extends string>(table: Map<number, K>, n: number, what: string): K {
  const name = table.get(n);
  if (name === undefined) throw new ProtocolError(`unknown ${what}`);
  return name;
}

function number<K extends string>(table: Readonly<Record<K, number>>, name: K, what: string): number {
  const n = table[name];
  if (n === undefined) throw new TypeError(`no ${what} is called ${JSON.stringify(name)}`);
  return n;
}

// ----------------------------------------------------------------- writer

const utf8 = new TextEncoder();
const fromUtf8 = new TextDecoder("utf-8", { fatal: true });

export class Writer {
  private buf = new Uint8Array(256);
  private view = new DataView(this.buf.buffer);
  private len = 0;

  private room(n: number): number {
    const at = this.len;
    if (at + n > this.buf.length) {
      const grown = new Uint8Array(Math.max(this.buf.length * 2, at + n));
      grown.set(this.buf.subarray(0, at));
      this.buf = grown;
      this.view = new DataView(grown.buffer);
    }
    this.len = at + n;
    return at;
  }

  /** What has been written. */
  bytes(): Uint8Array {
    return this.buf.slice(0, this.len);
  }

  // The room is made before the buffer is named: making it may replace
  // the buffer.
  u8(v: number): void {
    fits(v, 0xff);
    const at = this.room(1);
    this.buf[at] = v;
  }

  flag(v: boolean): void {
    this.u8(v ? 1 : 0);
  }

  u16(v: number): void {
    fits(v, 0xffff);
    const at = this.room(2);
    this.view.setUint16(at, v, true);
  }

  u32(v: number): void {
    fits(v, 0xffff_ffff);
    const at = this.room(4);
    this.view.setUint32(at, v, true);
  }

  f64(v: number): void {
    const at = this.room(8);
    this.view.setFloat64(at, v, true);
  }

  i128(v: bigint): void {
    if (v < -I128_MAX || v > I128_MAX) throw new RangeError("a decimal's digits must fit 127 bits");
    const at = this.room(16);
    const bits = BigInt.asUintN(128, v);
    this.view.setBigUint64(at, bits & U64_MAX, true);
    this.view.setBigUint64(at + 8, bits >> 64n, true);
  }

  raw(v: Uint8Array): void {
    const at = this.room(v.length);
    this.buf.set(v, at);
  }

  varint(value: number | bigint): void {
    if (typeof value === "number" && value >= 0 && value < 0x8000_0000 && Number.isInteger(value)) {
      let v = value;
      while (v >= 0x80) {
        this.u8((v & 0x7f) | 0x80);
        v >>>= 7;
      }
      this.u8(v);
      return;
    }
    let v = typeof value === "bigint" ? value : Number.isSafeInteger(value) ? BigInt(value) : -1n;
    if (v < 0n || v > U64_MAX) throw new RangeError(`${value} is not an unsigned 64-bit number`);
    while (v >= 0x80n) {
      this.u8(Number(v & 0x7fn) | 0x80);
      v >>= 7n;
    }
    this.u8(Number(v));
  }

  /** A signed integer, small in either direction staying short. */
  zigzag(v: bigint): void {
    if (v < I64_MIN || v > I64_MAX) throw new RangeError(`${v} does not fit the database's 64-bit integers`);
    this.varint(BigInt.asUintN(64, (v << 1n) ^ (v >> 63n)));
  }

  blob(v: Uint8Array): void {
    this.varint(v.length);
    this.raw(v);
  }

  text(v: string): void {
    if (LONE_SURROGATE.test(v)) throw new RangeError(`${JSON.stringify(v)} holds half of a surrogate pair, which is not text`);
    this.blob(utf8.encode(v));
  }

  optText(v: string | null): void {
    if (v === null) {
      this.u8(0);
    } else {
      this.u8(1);
      this.text(v);
    }
  }

  texts(v: readonly string[]): void {
    this.varint(v.length);
    for (const s of v) this.text(s);
  }

  list<T>(items: readonly T[], put: (w: Writer, item: T) => void): void {
    this.varint(items.length);
    for (const item of items) put(this, item);
  }
}

// ----------------------------------------------------------------- reader

export class Reader {
  private readonly view: DataView;
  pos = 0;
  private depth = 0;

  constructor(private readonly buf: Uint8Array) {
    this.view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  }

  remaining(): number {
    return this.buf.length - this.pos;
  }

  private skip(n: number): number {
    if (this.remaining() < n) throw new ProtocolError("ends early");
    const at = this.pos;
    this.pos += n;
    return at;
  }

  take(n: number): Uint8Array {
    const at = this.skip(n);
    // A copy, and a plain Uint8Array whatever the frame was read into.
    return new Uint8Array(this.buf.subarray(at, at + n));
  }

  u8(): number {
    return this.buf[this.skip(1)]!;
  }

  flag(): boolean {
    const v = this.u8();
    if (v > 1) throw new ProtocolError("flag is neither 0 nor 1");
    return v === 1;
  }

  u16(): number {
    return this.view.getUint16(this.skip(2), true);
  }

  u32(): number {
    return this.view.getUint32(this.skip(4), true);
  }

  f64(): number {
    return this.view.getFloat64(this.skip(8), true);
  }

  i128(): bigint {
    const at = this.skip(16);
    return BigInt.asIntN(128, (this.view.getBigUint64(at + 8, true) << 64n) | this.view.getBigUint64(at, true));
  }

  varint(): bigint {
    let v = 0n;
    let shift = 0n;
    for (;;) {
      const byte = this.u8();
      if (shift === 63n && byte > 1) throw new ProtocolError("number too large");
      v |= BigInt(byte & 0x7f) << shift;
      if ((byte & 0x80) === 0) return v;
      shift += 7n;
      if (shift > 63n) throw new ProtocolError("number too long");
    }
  }

  zigzag(): bigint {
    const v = this.varint();
    return (v >> 1n) ^ -(v & 1n);
  }

  /** A count of items that each take at least one byte, so a count larger than what is left is a lie. */
  count(): number {
    const n = this.varint();
    if (n > BigInt(this.remaining())) throw new ProtocolError("count exceeds the message");
    return Number(n);
  }

  blob(): Uint8Array {
    return this.take(this.count());
  }

  text(): string {
    const n = this.count();
    const at = this.skip(n);
    try {
      return fromUtf8.decode(this.buf.subarray(at, at + n));
    } catch {
      throw new ProtocolError("text is not UTF-8");
    }
  }

  optText(): string | null {
    return this.flag() ? this.text() : null;
  }

  texts(): string[] {
    return this.list((r) => r.text());
  }

  list<T>(get: (r: Reader) => T): T[] {
    const n = this.count();
    const out: T[] = [];
    for (let i = 0; i < n; i++) out.push(get(this));
    return out;
  }

  enter(): void {
    if (++this.depth > MAX_DEPTH) throw new ProtocolError("nested too deeply");
  }

  leave(): void {
    this.depth--;
  }

  finish(): void {
    if (this.remaining() !== 0) throw new ProtocolError("bytes left over");
  }
}

// ----------------------------------------------------------------- values

export function putValue(w: Writer, v: WireValue): void {
  switch (v.kind) {
    case "null":
      return w.u8(0);
    case "bool":
      return w.u8(v.value ? 2 : 1);
    case "int":
      w.u8(3);
      return w.zigzag(v.value);
    case "float":
      w.u8(4);
      return w.f64(v.value);
    case "decimal":
      w.u8(5);
      w.u8(v.scale);
      return w.i128(v.mantissa);
    case "text":
      w.u8(6);
      return w.text(v.value);
    case "bytes":
      w.u8(7);
      return w.blob(v.value);
    case "uuid":
      if (v.value.length !== 16) throw new RangeError("a uuid is 16 bytes");
      w.u8(8);
      return w.raw(v.value);
    case "timestamp":
      w.u8(9);
      return w.zigzag(v.micros);
    case "date":
      w.u8(10);
      return w.zigzag(BigInt(v.days));
    case "json":
      w.u8(11);
      return w.text(v.text);
    case "list":
      w.u8(12);
      return w.list(v.items, putValue);
  }
}

export function getValue(r: Reader): WireValue {
  const tag = r.u8();
  switch (tag) {
    case 0:
      return { kind: "null" };
    case 1:
      return { kind: "bool", value: false };
    case 2:
      return { kind: "bool", value: true };
    case 3:
      return { kind: "int", value: r.zigzag() };
    case 4:
      return { kind: "float", value: r.f64() };
    case 5: {
      const scale = r.u8();
      const mantissa = r.i128();
      if (scale > MAX_SCALE || mantissa === I128_MIN) throw new ProtocolError("decimal out of range");
      return { kind: "decimal", mantissa, scale };
    }
    case 6:
      return { kind: "text", value: r.text() };
    case 7:
      return { kind: "bytes", value: r.blob() };
    case 8:
      return { kind: "uuid", value: r.take(16) };
    case 9:
      return { kind: "timestamp", micros: r.zigzag() };
    case 10: {
      const days = r.zigzag();
      if (days < -(1n << 31n) || days >= 1n << 31n) throw new ProtocolError("date out of range");
      return { kind: "date", days: Number(days) };
    }
    case 11: {
      const text = r.text();
      try {
        JSON.parse(text);
      } catch {
        throw new ProtocolError("invalid JSON");
      }
      return { kind: "json", text };
    }
    case 12: {
      r.enter();
      const items = r.list(getValue);
      r.leave();
      return { kind: "list", items };
    }
    default:
      throw new ProtocolError("unknown value tag");
  }
}

const SIMPLE_TYPES = { bool: 1, int: 2, float: 3, text: 5, bytes: 6, uuid: 7, timestamp: 8, date: 9, json: 10 } as const;
const SIMPLE_TYPE_BY_TAG = numbers(SIMPLE_TYPES);

export function putType(w: Writer, t: ast.Type): void {
  if (t.kind === "list") {
    w.u8(11);
    putType(w, t.of);
  } else if (t.kind === "decimal") {
    if (t.precision === null || t.scale === null) {
      w.u8(4);
    } else {
      w.u8(12);
      w.u8(t.precision);
      w.u8(t.scale);
    }
  } else {
    w.u8(number(SIMPLE_TYPES, t.kind, "type"));
  }
}

export function getType(r: Reader): ast.Type {
  const tag = r.u8();
  if (tag === 4) return { kind: "decimal", precision: null, scale: null };
  if (tag === 11) {
    r.enter();
    const of = getType(r);
    r.leave();
    return { kind: "list", of };
  }
  if (tag === 12) {
    const precision = r.u8();
    const scale = r.u8();
    if (scale > precision || scale > MAX_SCALE) throw new ProtocolError("decimal precision and scale do not fit");
    return { kind: "decimal", precision, scale };
  }
  return { kind: named(SIMPLE_TYPE_BY_TAG, tag, "type tag") };
}

// ------------------------------------------------------------ expressions

function putOptExpr(w: Writer, e: ast.ExprNode | null): void {
  if (e === null) {
    w.u8(0);
  } else {
    w.u8(1);
    putExpr(w, e);
  }
}

function getOptExpr(r: Reader): ast.ExprNode | null {
  return r.flag() ? getExpr(r) : null;
}

function putExprs(w: Writer, exprs: readonly ast.ExprNode[]): void {
  w.list(exprs, putExpr);
}

function getExprs(r: Reader): ast.ExprNode[] {
  return r.list(getExpr);
}

export function putExpr(w: Writer, e: ast.ExprNode): void {
  switch (e.kind) {
    case "value":
      w.u8(1);
      return putValue(w, e.value);
    case "column":
      w.u8(2);
      w.optText(e.table);
      return w.text(e.name);
    case "excluded":
      w.u8(3);
      return w.text(e.name);
    case "old":
      w.u8(4);
      return w.text(e.name);
    case "depth":
      return w.u8(5);
    case "rank":
      return w.u8(6);
    case "not":
      w.u8(7);
      return putExpr(w, e.expr);
    case "and":
      w.u8(8);
      return putExprs(w, e.parts);
    case "or":
      w.u8(9);
      return putExprs(w, e.parts);
    case "cmp":
      w.u8(10);
      w.u8(number(ast.CMP_OPS, e.op, "comparison"));
      putExpr(w, e.left);
      return putExpr(w, e.right);
    case "is_null":
      w.u8(11);
      w.flag(e.negated);
      return putExpr(w, e.expr);
    case "in":
      w.u8(12);
      w.flag(e.negated);
      putExpr(w, e.expr);
      return putExpr(w, e.list);
    case "like":
      w.u8(13);
      w.flag(e.caseInsensitive);
      w.flag(e.negated);
      putExpr(w, e.expr);
      return putExpr(w, e.pattern);
    case "arith":
      w.u8(14);
      w.u8(number(ast.ARITH_OPS, e.op, "operator"));
      putExpr(w, e.left);
      return putExpr(w, e.right);
    case "neg":
      w.u8(15);
      return putExpr(w, e.expr);
    case "call":
      w.u8(16);
      w.u16(number(ast.FUNCS, e.func, "function"));
      return putExprs(w, e.args);
    case "case":
      w.u8(17);
      w.varint(e.branches.length);
      for (const branch of e.branches) {
        putExpr(w, branch.when);
        putExpr(w, branch.then);
      }
      return putOptExpr(w, e.otherwise);
    case "cast":
      w.u8(18);
      w.flag(e.lenient);
      putType(w, e.to);
      return putExpr(w, e.expr);
    case "list":
      w.u8(19);
      return putExprs(w, e.items);
    case "agg":
      w.u8(20);
      w.u8(number(ast.AGG_FUNCS, e.func, "aggregate"));
      w.flag(e.distinct);
      putOptExpr(w, e.arg);
      return putOptExpr(w, e.filter);
    case "subquery":
      w.u8(21);
      w.u8(number(ast.SUBQUERY_KINDS, e.subquery, "subquery kind"));
      return putQuery(w, e.query);
  }
}

export function getExpr(r: Reader): ast.ExprNode {
  r.enter();
  const e = getExprInner(r);
  r.leave();
  return e;
}

function getExprInner(r: Reader): ast.ExprNode {
  const tag = r.u8();
  switch (tag) {
    case 1:
      return { kind: "value", value: getValue(r) };
    case 2:
      return { kind: "column", table: r.optText(), name: r.text() };
    case 3:
      return { kind: "excluded", name: r.text() };
    case 4:
      return { kind: "old", name: r.text() };
    case 5:
      return { kind: "depth" };
    case 6:
      return { kind: "rank" };
    case 7:
      return { kind: "not", expr: getExpr(r) };
    case 8:
      return { kind: "and", parts: getExprs(r) };
    case 9:
      return { kind: "or", parts: getExprs(r) };
    case 10: {
      const op = named(CMP_BY_NUMBER, r.u8(), "comparison");
      return { kind: "cmp", op, left: getExpr(r), right: getExpr(r) };
    }
    case 11: {
      const negated = r.flag();
      return { kind: "is_null", expr: getExpr(r), negated };
    }
    case 12: {
      const negated = r.flag();
      return { kind: "in", expr: getExpr(r), list: getExpr(r), negated };
    }
    case 13: {
      const caseInsensitive = r.flag();
      const negated = r.flag();
      return { kind: "like", expr: getExpr(r), pattern: getExpr(r), caseInsensitive, negated };
    }
    case 14: {
      const op = named(ARITH_BY_NUMBER, r.u8(), "operator");
      return { kind: "arith", op, left: getExpr(r), right: getExpr(r) };
    }
    case 15:
      return { kind: "neg", expr: getExpr(r) };
    case 16: {
      const func = named(FUNC_BY_NUMBER, r.u16(), "function");
      return { kind: "call", func, args: getExprs(r) };
    }
    case 17: {
      const branches = r.list((r) => ({ when: getExpr(r), then: getExpr(r) }));
      return { kind: "case", branches, otherwise: getOptExpr(r) };
    }
    case 18: {
      const lenient = r.flag();
      const to = getType(r);
      return { kind: "cast", expr: getExpr(r), to, lenient };
    }
    case 19:
      return { kind: "list", items: getExprs(r) };
    case 20: {
      const func = named(AGG_BY_NUMBER, r.u8(), "aggregate");
      const distinct = r.flag();
      return { kind: "agg", func, arg: getOptExpr(r), filter: getOptExpr(r), distinct };
    }
    case 21: {
      const subquery = named(SUBQUERY_BY_NUMBER, r.u8(), "subquery kind");
      return { kind: "subquery", subquery, query: getQuery(r) };
    }
    default:
      throw new ProtocolError("unknown expression tag");
  }
}

// ---------------------------------------------------------------- queries

function putSelect(w: Writer, s: ast.SelectSpec): void {
  w.text(s.name);
  putExpr(w, s.expr);
}

function getSelect(r: Reader): ast.SelectSpec {
  return { name: r.text(), expr: getExpr(r) };
}

function putBound(w: Writer, b: ast.BoundSpec): void {
  if (b.kind === "unbounded") {
    w.u8(0);
  } else {
    w.u8(b.kind === "included" ? 1 : 2);
    putExprs(w, b.values);
  }
}

function getBound(r: Reader): ast.BoundSpec {
  const tag = r.u8();
  if (tag === 0) return { kind: "unbounded" };
  if (tag === 1) return { kind: "included", values: getExprs(r) };
  if (tag === 2) return { kind: "excluded", values: getExprs(r) };
  throw new ProtocolError("unknown bound");
}

function putRange(w: Writer, k: ast.KeyRangeSpec): void {
  putBound(w, k.lower);
  putBound(w, k.upper);
  w.flag(k.nulls);
}

function getRange(r: Reader): ast.KeyRangeSpec {
  return { lower: getBound(r), upper: getBound(r), nulls: r.flag() };
}

function putAccess(w: Writer, a: ast.AccessSpec): void {
  switch (a.kind) {
    case "primary":
      w.u8(1);
      w.flag(a.reverse);
      return w.list(a.ranges, putRange);
    case "index":
      w.u8(2);
      w.text(a.index);
      w.flag(a.reverse);
      return w.list(a.ranges, putRange);
    case "walk":
      w.u8(3);
      w.list(a.start, putExprs);
      w.text(a.parent);
      w.u8(number(ast.DIRECTIONS, a.direction, "direction"));
      w.optText(a.index);
      putOptExpr(w, a.prune);
      putOptExpr(w, a.stop);
      return w.u32(a.maxDepth);
    case "search":
      w.u8(4);
      w.text(a.index);
      putExprs(w, a.prefix);
      putExpr(w, a.query);
      return w.flag(a.matchPrefix);
  }
}

function getAccess(r: Reader): ast.AccessSpec {
  const tag = r.u8();
  switch (tag) {
    case 1: {
      const reverse = r.flag();
      return { kind: "primary", ranges: r.list(getRange), reverse };
    }
    case 2: {
      const index = r.text();
      const reverse = r.flag();
      return { kind: "index", index, ranges: r.list(getRange), reverse };
    }
    case 3: {
      const start = r.list(getExprs);
      const parent = r.text();
      const direction = named(DIRECTION_BY_NUMBER, r.u8(), "direction");
      return { kind: "walk", start, parent, direction, index: r.optText(), prune: getOptExpr(r), stop: getOptExpr(r), maxDepth: r.u32() };
    }
    case 4:
      return { kind: "search", index: r.text(), prefix: getExprs(r), query: getExpr(r), matchPrefix: r.flag() };
    default:
      throw new ProtocolError("unknown access");
  }
}

export function putQuery(w: Writer, q: ast.QuerySpec): void {
  w.text(q.table);
  w.optText(q.alias);
  putAccess(w, q.access);
  w.list(q.joins, (w, join) => {
    w.u8(number(ast.JOIN_KINDS, join.kind, "join kind"));
    putQuery(w, join.query);
  });
  if (q.unnest === null) {
    w.u8(0);
  } else {
    w.u8(1);
    putSelect(w, q.unnest);
  }
  putOptExpr(w, q.filter);
  putExprs(w, q.groupBy);
  putOptExpr(w, q.having);
  w.list(q.orderBy, (w, order) => {
    putExpr(w, order.expr);
    w.flag(order.desc);
    w.u8(order.nullsLast === null ? 0 : order.nullsLast ? 2 : 1);
  });
  w.varint(q.offset);
  if (q.limit === null) {
    w.u8(0);
  } else {
    w.u8(1);
    w.varint(q.limit);
  }
  w.list(q.select, putSelect);
  w.flag(q.distinct);
  w.flag(q.withTotal);
}

export function getQuery(r: Reader): ast.QuerySpec {
  r.enter();
  const table = r.text();
  const alias = r.optText();
  const access = getAccess(r);
  const joins = r.list((r): ast.JoinSpec => ({ kind: named(JOIN_BY_NUMBER, r.u8(), "join kind"), query: getQuery(r) }));
  const unnest = r.flag() ? getSelect(r) : null;
  const filter = getOptExpr(r);
  const groupBy = getExprs(r);
  const having = getOptExpr(r);
  const orderBy = r.list((r): ast.OrderSpec => {
    const expr = getExpr(r);
    const desc = r.flag();
    const nulls = r.u8();
    if (nulls > 2) throw new ProtocolError("unknown null order");
    return { expr, desc, nullsLast: nulls === 0 ? null : nulls === 2 };
  });
  const offset = r.varint();
  const limit = r.flag() ? r.varint() : null;
  const select = r.list(getSelect);
  const distinct = r.flag();
  const withTotal = r.flag();
  r.leave();
  return { table, alias, access, joins, unnest, filter, groupBy, having, orderBy, offset, limit, select, distinct, withTotal };
}

function putAssign(w: Writer, a: ast.AssignSpec): void {
  w.text(a.column);
  putExpr(w, a.value);
}

function getAssign(r: Reader): ast.AssignSpec {
  return { column: r.text(), value: getExpr(r) };
}

export function putInsert(w: Writer, i: ast.InsertSpec): void {
  w.text(i.table);
  w.texts(i.columns);
  if (i.source.kind === "values") {
    w.u8(0);
    w.list(i.source.rows, putExprs);
  } else {
    w.u8(1);
    putQuery(w, i.source.query);
  }
  const c = i.onConflict;
  if (c.kind === "error") {
    w.u8(0);
  } else if (c.kind === "do_nothing") {
    w.u8(1);
    w.optText(c.target);
  } else {
    w.u8(2);
    w.text(c.target);
    w.list(c.set, putAssign);
    putOptExpr(w, c.filter);
  }
  w.list(i.returning, putSelect);
}

export function getInsert(r: Reader): ast.InsertSpec {
  const table = r.text();
  const columns = r.texts();
  const sourceTag = r.u8();
  let source: ast.InsertSpec["source"];
  if (sourceTag === 0) source = { kind: "values", rows: r.list(getExprs) };
  else if (sourceTag === 1) source = { kind: "query", query: getQuery(r) };
  else throw new ProtocolError("unknown insert source");
  const action = r.u8();
  let onConflict: ast.OnConflictSpec;
  if (action === 0) onConflict = { kind: "error" };
  else if (action === 1) onConflict = { kind: "do_nothing", target: r.optText() };
  else if (action === 2) onConflict = { kind: "do_update", target: r.text(), set: r.list(getAssign), filter: getOptExpr(r) };
  else throw new ProtocolError("unknown conflict action");
  return { table, columns, source, onConflict, returning: r.list(getSelect) };
}

export function putUpdate(w: Writer, u: ast.UpdateSpec): void {
  putQuery(w, u.rows);
  w.list(u.set, putAssign);
  w.list(u.returning, putSelect);
}

export function getUpdate(r: Reader): ast.UpdateSpec {
  return { rows: getQuery(r), set: r.list(getAssign), returning: r.list(getSelect) };
}

export function putDelete(w: Writer, d: ast.DeleteSpec): void {
  putQuery(w, d.rows);
  w.list(d.returning, putSelect);
}

export function getDelete(r: Reader): ast.DeleteSpec {
  return { rows: getQuery(r), returning: r.list(getSelect) };
}

// ----------------------------------------------------------------- schema

export function putColumn(w: Writer, c: ast.ColumnSpec): void {
  w.text(c.name);
  putType(w, c.type);
  w.flag(c.nullable);
  putOptExpr(w, c.default);
  w.flag(c.autoIncrement);
}

export function getColumn(r: Reader): ast.ColumnSpec {
  return { name: r.text(), type: getType(r), nullable: r.flag(), default: getOptExpr(r), autoIncrement: r.flag() };
}

export function putIndex(w: Writer, i: ast.IndexSpec): void {
  w.text(i.name);
  w.list(i.keys, (w, key) => {
    putExpr(w, key.expr);
    w.flag(key.desc);
    w.flag(key.each);
  });
  w.flag(i.unique);
  putOptExpr(w, i.filter);
  if (i.index.kind === "ordered") {
    w.u8(0);
  } else {
    w.u8(1);
    w.u8(number(ast.TEXT_CONFIGS, i.index.config, "text config"));
    w.list(i.index.fields, (w, field) => {
      putExpr(w, field.expr);
      w.f64(field.weight);
    });
  }
}

export function getIndex(r: Reader): ast.IndexSpec {
  const name = r.text();
  const keys = r.list((r): ast.IndexKeySpec => ({ expr: getExpr(r), desc: r.flag(), each: r.flag() }));
  const unique = r.flag();
  const filter = getOptExpr(r);
  const kind = r.u8();
  if (kind === 0) return { name, keys, unique, filter, index: { kind: "ordered" } };
  if (kind !== 1) throw new ProtocolError("unknown index kind");
  const config = named(TEXT_CONFIG_BY_NUMBER, r.u8(), "text config");
  const fields = r.list((r) => ({ expr: getExpr(r), weight: r.f64() }));
  return { name, keys, unique, filter, index: { kind: "full_text", config, fields } };
}

export function putTable(w: Writer, t: ast.TableSpec): void {
  w.text(t.name);
  w.list(t.columns, putColumn);
  w.texts(t.primaryKey);
  w.list(t.indexes, putIndex);
  w.list(t.foreignKeys, (w, fk) => {
    w.text(fk.name);
    w.texts(fk.columns);
    w.text(fk.refTable);
    w.texts(fk.refColumns);
    w.u8(number(ast.ON_DELETE, fk.onDelete, "delete action"));
  });
  w.list(t.checks, (w, check) => {
    w.text(check.name);
    putExpr(w, check.expr);
  });
  w.flag(t.volatile);
}

export function getTable(r: Reader): ast.TableSpec {
  return {
    name: r.text(),
    columns: r.list(getColumn),
    primaryKey: r.texts(),
    indexes: r.list(getIndex),
    foreignKeys: r.list(
      (r): ast.ForeignKeySpec => ({
        name: r.text(),
        columns: r.texts(),
        refTable: r.text(),
        refColumns: r.texts(),
        onDelete: named(ON_DELETE_BY_NUMBER, r.u8(), "delete action"),
      }),
    ),
    checks: r.list((r): ast.CheckSpec => ({ name: r.text(), expr: getExpr(r) })),
    volatile: r.flag(),
  };
}

// --------------------------------------------------------------- messages

function putDdl(w: Writer, d: ast.Ddl): void {
  switch (d.kind) {
    case "create_table":
      w.u8(1);
      putTable(w, d.def);
      return w.flag(d.ifNotExists);
    case "drop_table":
      w.u8(2);
      w.text(d.name);
      return w.flag(d.ifExists);
    case "add_column":
      w.u8(3);
      w.text(d.table);
      putColumn(w, d.column);
      return w.flag(d.ifNotExists);
    case "drop_column":
      w.u8(4);
      w.text(d.table);
      w.text(d.column);
      return w.flag(d.ifExists);
    case "create_index":
      w.u8(5);
      w.text(d.table);
      putIndex(w, d.index);
      return w.flag(d.ifNotExists);
    case "drop_index":
      w.u8(6);
      w.text(d.table);
      w.text(d.index);
      return w.flag(d.ifExists);
  }
}

function putAdmin(w: Writer, a: ast.AdminRequest): void {
  switch (a.kind) {
    case "create_database":
      w.u8(1);
      w.text(a.name);
      w.text(a.password);
      return w.flag(a.ifNotExists);
    case "drop_database":
      w.u8(2);
      w.text(a.name);
      return w.flag(a.ifExists);
    case "set_password":
      w.u8(3);
      w.text(a.name);
      return w.text(a.password);
    case "list_databases":
      return w.u8(4);
    case "checkpoint":
      return w.u8(5);
    case "check":
      return w.u8(6);
    case "stats":
      return w.u8(7);
    case "dump":
      w.u8(8);
      return w.texts(a.databases);
    case "restore_begin":
      w.u8(9);
      return w.flag(a.replace);
    case "restore_chunk":
      w.u8(10);
      return w.blob(a.bytes);
    case "restore_end":
      return w.u8(11);
  }
}

/**
 * A request as the payload of one frame: how long the server may spend
 * on it, in milliseconds (0 for no limit beyond the server's own), then
 * the request.
 */
export function encodeRequest(request: ast.Request, timeoutMs: number | bigint = 0): Uint8Array {
  const w = new Writer();
  w.varint(timeoutMs);
  switch (request.kind) {
    case "hello":
      w.u8(1);
      w.u32(request.version);
      w.text(request.database);
      w.text(request.password);
      w.text(request.client);
      break;
    case "ping":
      w.u8(2);
      break;
    case "begin":
      w.u8(3);
      w.u8(number(ast.TXN_KINDS, request.txn, "kind of transaction"));
      w.varint(request.lockWaitMs);
      break;
    case "commit":
      w.u8(4);
      break;
    case "rollback":
      w.u8(5);
      break;
    case "savepoint":
      w.u8(6);
      w.text(request.name);
      break;
    case "rollback_to":
      w.u8(7);
      w.text(request.name);
      break;
    case "release":
      w.u8(8);
      w.text(request.name);
      break;
    case "query":
      w.u8(9);
      putQuery(w, request.query);
      break;
    case "insert":
      w.u8(10);
      putInsert(w, request.insert);
      break;
    case "update":
      w.u8(11);
      putUpdate(w, request.update);
      break;
    case "delete":
      w.u8(12);
      putDelete(w, request.delete);
      break;
    case "ddl":
      w.u8(13);
      putDdl(w, request.ddl);
      break;
    case "describe":
      w.u8(14);
      break;
    case "lock":
      w.u8(15);
      w.text(request.name);
      w.varint(request.waitMs);
      break;
    case "unlock":
      w.u8(16);
      w.text(request.name);
      break;
    case "now":
      w.u8(17);
      break;
    case "admin":
      w.u8(18);
      putAdmin(w, request.admin);
      break;
  }
  return w.bytes();
}

/** Reads the payload of one frame from the server. */
export function decodeResponse(frame: Uint8Array): ast.Response {
  const r = new Reader(frame);
  const response = getResponse(r);
  r.finish();
  return response;
}

function getResponse(r: Reader): ast.Response {
  const tag = r.u8();
  switch (tag) {
    case 1:
      return { kind: "ok" };
    case 2:
      return { kind: "hello", version: r.u32(), server: r.text() };
    case 3: {
      const more = r.flag();
      const columns = r.texts();
      const affected = r.varint();
      const total = r.flag() ? r.varint() : null;
      const n = r.varint();
      const rows: WireValue[][] = [];
      for (let i = 0n; i < n; i++) {
        const row: WireValue[] = [];
        for (let c = 0; c < columns.length; c++) row.push(getValue(r));
        rows.push(row);
        // A row of no columns takes no bytes, so its count cannot be
        // checked against the frame.
        if (columns.length === 0 && rows.length > 1_000_000) throw new ProtocolError("too many empty rows");
      }
      return { kind: "rows", more, columns, rows, affected, total };
    }
    case 4:
      return { kind: "tables", tables: r.list(getTable) };
    case 5:
      return { kind: "flag", value: r.flag() };
    case 6:
      return { kind: "time", micros: r.zigzag() };
    case 7:
      return { kind: "names", names: r.texts() };
    case 8:
      return { kind: "text", text: r.text() };
    case 9:
      return { kind: "chunk", bytes: r.blob() };
    case 10: {
      const code = errorCodeFromWire(r.u8());
      return { kind: "error", error: new DbError(code, r.text(), r.optText(), r.optText()) };
    }
    default:
      throw new ProtocolError("unknown response");
  }
}
