// The vocabulary requests are written in: `Query.table("mails")`,
// `col("date").desc()`, `Insert.into("users").set("name", "ann")`.
//
// It follows the Rust client's, name for name in camelCase. Every builder
// is immutable: a method returns a new one, so a query kept in a variable
// can be narrowed twice without the two getting in each other's way.
// `.spec` (`.node` for an expression) is the plain data the wire carries.

import type * as ast from "./ast.js";
import { toWire, type Value } from "./values.js";

/**
 * The name of a table's primary key where an index name is asked for: as
 * an upsert's conflict target, or in a unique violation.
 */
export const PRIMARY = "primary";

/** In a `returning`: every column of the row, under its own name. */
export const ALL = "*";

/** What may be given where an expression is expected: an `Expr`, or a plain value for a constant. */
export type Arg = Expr | Value;

/** The expression a value stands for: an `Expr` as it is, anything else as a constant. */
export function node(value: Arg): ast.ExprNode {
  if (value instanceof Expr) return value.node;
  if (value instanceof Query || value instanceof Order || value instanceof KeyRange) {
    throw new TypeError(`a ${value.constructor.name} is not a value; wrap a query in scalar(), exists() or listOf()`);
  }
  if (value === undefined) throw new TypeError("undefined is not a value; use null for NULL");
  return { kind: "value", value: toWire(value) };
}

function nodes(values: Iterable<Arg>): ast.ExprNode[] {
  if (typeof values === "string" || values === null || typeof (values as Iterable<Arg>)[Symbol.iterator] !== "function") {
    throw new TypeError("expected a list of values; a key of one column is written [value]");
  }
  return Array.from(values, node);
}

/**
 * An expression: what a filter tests, what an update assigns, what a
 * query returns. NULL behaves as in SQL.
 */
export class Expr {
  constructor(readonly node: ast.ExprNode) {}

  // ---- comparisons

  private cmp(op: ast.CmpOp, other: Arg): Expr {
    return new Expr({ kind: "cmp", op, left: this.node, right: node(other) });
  }
  eq(other: Arg): Expr {
    return this.cmp("eq", other);
  }
  ne(other: Arg): Expr {
    return this.cmp("ne", other);
  }
  lt(other: Arg): Expr {
    return this.cmp("lt", other);
  }
  le(other: Arg): Expr {
    return this.cmp("le", other);
  }
  gt(other: Arg): Expr {
    return this.cmp("gt", other);
  }
  ge(other: Arg): Expr {
    return this.cmp("ge", other);
  }
  /** Not equal, counting NULL as a value. Never NULL. */
  isDistinctFrom(other: Arg): Expr {
    return this.cmp("is_distinct_from", other);
  }
  /** Equal, counting NULL as a value. Never NULL. */
  isNotDistinctFrom(other: Arg): Expr {
    return this.cmp("is_not_distinct_from", other);
  }
  isNull(): Expr {
    return new Expr({ kind: "is_null", expr: this.node, negated: false });
  }
  isNotNull(): Expr {
    return new Expr({ kind: "is_null", expr: this.node, negated: true });
  }
  /** True unless the value is true: NULL counts as false. */
  isNotTrue(): Expr {
    return this.isDistinctFrom(true);
  }
  and(other: Arg): Expr {
    const parts = this.node.kind === "and" ? this.node.parts : [this.node];
    return new Expr({ kind: "and", parts: [...parts, node(other)] });
  }
  or(other: Arg): Expr {
    const parts = this.node.kind === "or" ? this.node.parts : [this.node];
    return new Expr({ kind: "or", parts: [...parts, node(other)] });
  }
  not(): Expr {
    return new Expr({ kind: "not", expr: this.node });
  }
  /** `low <= this AND this <= high`. */
  between(low: Arg, high: Arg): Expr {
    return new Expr({ kind: "and", parts: [this.ge(low).node, this.le(high).node] });
  }
  /** Whether the value equals an item of the list: an array, a list column, or `listOf(query)`. */
  inList(list: Arg): Expr {
    return new Expr({ kind: "in", expr: this.node, list: node(list), negated: false });
  }
  notInList(list: Arg): Expr {
    return new Expr({ kind: "in", expr: this.node, list: node(list), negated: true });
  }
  /** SQL `LIKE`: `%` matches any run of characters, `_` any one, and a backslash makes the next character literal. */
  like(pattern: Arg): Expr {
    return new Expr({ kind: "like", expr: this.node, pattern: node(pattern), caseInsensitive: false, negated: false });
  }
  notLike(pattern: Arg): Expr {
    return new Expr({ kind: "like", expr: this.node, pattern: node(pattern), caseInsensitive: false, negated: true });
  }
  /** `LIKE` without regard to case. */
  ilike(pattern: Arg): Expr {
    return new Expr({ kind: "like", expr: this.node, pattern: node(pattern), caseInsensitive: true, negated: false });
  }

  // ---- arithmetic

  private arith(op: ast.ArithOp, other: Arg): Expr {
    return new Expr({ kind: "arith", op, left: this.node, right: node(other) });
  }
  add(other: Arg): Expr {
    return this.arith("add", other);
  }
  sub(other: Arg): Expr {
    return this.arith("sub", other);
  }
  mul(other: Arg): Expr {
    return this.arith("mul", other);
  }
  div(other: Arg): Expr {
    return this.arith("div", other);
  }
  mod(other: Arg): Expr {
    return this.arith("mod", other);
  }
  neg(): Expr {
    return new Expr({ kind: "neg", expr: this.node });
  }

  // ---- conversions and functions

  cast(to: ast.Type): Expr {
    return new Expr({ kind: "cast", expr: this.node, to, lenient: false });
  }
  /** Converts, giving NULL where `cast` would fail. */
  tryCast(to: ast.Type): Expr {
    return new Expr({ kind: "cast", expr: this.node, to, lenient: true });
  }
  private call(func: ast.Func, ...others: Arg[]): Expr {
    return new Expr({ kind: "call", func, args: [this.node, ...others.map(node)] });
  }
  lower(): Expr {
    return this.call("lower");
  }
  upper(): Expr {
    return this.call("upper");
  }
  /** Length in characters. */
  length(): Expr {
    return this.call("length");
  }
  /** Length in bytes. Does not load a large value. */
  octetLength(): Expr {
    return this.call("octet_length");
  }
  trim(): Expr {
    return this.call("trim");
  }
  /** The first `chars` characters. */
  left(chars: number): Expr {
    return this.call("left", chars);
  }
  /** Counting characters from 1. */
  substr(start: Arg, length: Arg): Expr {
    return this.call("substr", start, length);
  }
  /** Counting bytes from 1; gives bytes. */
  substrBytes(start: Arg, length: Arg): Expr {
    return this.call("substr_bytes", start, length);
  }
  strpos(needle: Arg): Expr {
    return this.call("strpos", needle);
  }
  /** This value, or `fallback` where it is NULL. */
  orElse(fallback: Arg): Expr {
    return this.call("coalesce", fallback);
  }
  /** A member of a JSON value: `col("detail").json(["device", "name"])`. */
  json(path: Iterable<Arg>): Expr {
    return new Expr({ kind: "call", func: "json_get", args: [this.node, ...nodes(path)] });
  }
  /** The same member as text. */
  jsonText(path: Iterable<Arg>): Expr {
    return new Expr({ kind: "call", func: "json_get_text", args: [this.node, ...nodes(path)] });
  }
  jsonContains(other: Arg): Expr {
    return this.call("json_contains", other);
  }
  jsonHasKey(key: Arg): Expr {
    return this.call("json_has_key", key);
  }
  jsonHasAny(keys: Arg): Expr {
    return this.call("json_has_any", keys);
  }
  jsonHasAll(keys: Arg): Expr {
    return this.call("json_has_all", keys);
  }
  jsonMerge(other: Arg): Expr {
    return this.call("json_merge", other);
  }
  jsonLen(): Expr {
    return this.call("json_len");
  }
  jsonPluck(key: string): Expr {
    return this.call("json_pluck", key);
  }
  /** A JSON array's items as a list of text. */
  jsonTexts(): Expr {
    return this.call("json_texts");
  }
  listOverlaps(other: Arg): Expr {
    return this.call("list_overlaps", other);
  }
  listConcat(other: Arg): Expr {
    return this.call("list_concat", other);
  }

  // ---- aggregates

  /** An aggregate over only the rows where the condition is true. */
  filter(condition: Arg): Expr {
    return this.node.kind === "agg" ? new Expr({ ...this.node, filter: node(condition) }) : this;
  }
  /** An aggregate over each distinct value once. */
  distinct(): Expr {
    return this.node.kind === "agg" ? new Expr({ ...this.node, distinct: true }) : this;
  }

  // ---- ordering

  /** For `orderBy`: this expression, descending. */
  desc(): Order {
    return new Order({ expr: this.node, desc: true, nullsLast: null });
  }
  asc(): Order {
    return new Order({ expr: this.node, desc: false, nullsLast: null });
  }
}

/** One key of an `orderBy`. */
export class Order {
  constructor(readonly spec: ast.OrderSpec) {}
  nullsLast(): Order {
    return new Order({ ...this.spec, nullsLast: true });
  }
  nullsFirst(): Order {
    return new Order({ ...this.spec, nullsLast: false });
  }
}

function order(value: Order | Expr): ast.OrderSpec {
  return value instanceof Order ? value.spec : { expr: node(value), desc: false, nullsLast: null };
}

// ------------------------------------------------------- free functions

/** A column: `col("name")`, or `col("m.name")` for the table named `m`. */
export function col(name: string): Expr {
  const dot = name.indexOf(".");
  return new Expr(dot < 0 ? { kind: "column", table: null, name } : { kind: "column", table: name.slice(0, dot), name: name.slice(dot + 1) });
}

/** A constant. Any value the database has a kind for; see `values.ts`. */
export function val(value: Value): Expr {
  if ((value as unknown) instanceof Expr) throw new TypeError("val() takes a plain value, not an expression");
  return new Expr(node(value));
}

/** SQL's NULL as an expression. */
export const NULL: Expr = new Expr({ kind: "value", value: { kind: "null" } });

/** The server's clock when the transaction began, or for a statement outside one, when the statement began. */
export function now(): Expr {
  return new Expr({ kind: "call", func: "now", args: [] });
}

/** In an upsert's update: the value the insert proposed for a column. */
export function excluded(column: string): Expr {
  return new Expr({ kind: "excluded", name: column });
}

/** In an update's `returning`: a column as it was before the update. */
export function old(column: string): Expr {
  return new Expr({ kind: "old", name: column });
}

/** In a tree walk: how many steps from a start row. */
export function depth(): Expr {
  return new Expr({ kind: "depth" });
}

/** In a full-text search: how well the row matches. */
export function rank(): Expr {
  return new Expr({ kind: "rank" });
}

export function call(func: ast.Func, args: Iterable<Arg> = []): Expr {
  return new Expr({ kind: "call", func, args: nodes(args) });
}

/** True when every part is. An empty list is true. */
export function all(parts: Iterable<Arg>): Expr {
  return new Expr({ kind: "and", parts: nodes(parts) });
}

/** True when any part is. An empty list is false. */
export function any(parts: Iterable<Arg>): Expr {
  return new Expr({ kind: "or", parts: nodes(parts) });
}

export function coalesce(args: Iterable<Arg>): Expr {
  return call("coalesce", args);
}

export function least(args: Iterable<Arg>): Expr {
  return call("least", args);
}

export function greatest(args: Iterable<Arg>): Expr {
  return call("greatest", args);
}

export function concat(args: Iterable<Arg>): Expr {
  return call("concat", args);
}

/**
 * A list of expressions' values: `list([col("date"), col("id")]).lt(list([d, id]))`
 * is SQL's `(date, id) < ($1, $2)`.
 */
export function list(items: Iterable<Arg>): Expr {
  return new Expr({ kind: "list", items: nodes(items) });
}

/**
 * `CASE WHEN condition THEN value ... ELSE otherwise END`. Without
 * `otherwise`, NULL where no branch holds.
 */
export function cases(branches: Iterable<readonly [Arg, Arg]>, otherwise?: Arg): Expr {
  return new Expr({
    kind: "case",
    branches: Array.from(branches, ([condition, value]) => ({ when: node(condition), then: node(value) })),
    otherwise: otherwise === undefined ? null : node(otherwise),
  });
}

/** `then` if the condition is true, else `otherwise`. */
export function when(condition: Arg, then: Arg, otherwise: Arg): Expr {
  return cases([[condition, then]], otherwise);
}

/** The count of rows. */
export function count(): Expr {
  return new Expr({ kind: "agg", func: "count", arg: null, filter: null, distinct: false });
}

function agg(func: ast.AggFunc, arg: Arg): Expr {
  return new Expr({ kind: "agg", func, arg: node(arg), filter: null, distinct: false });
}

export function sum(arg: Arg): Expr {
  return agg("sum", arg);
}

export function min(arg: Arg): Expr {
  return agg("min", arg);
}

export function max(arg: Arg): Expr {
  return agg("max", arg);
}

export function avg(arg: Arg): Expr {
  return agg("avg", arg);
}

/** The values as a list, in the order the rows were read. */
export function collect(arg: Arg): Expr {
  return agg("collect", arg);
}

function query(q: Query | ast.QuerySpec): ast.QuerySpec {
  return q instanceof Query ? q.spec : q;
}

/** The first column of a query's first row, or NULL. */
export function scalar(q: Query | ast.QuerySpec): Expr {
  return new Expr({ kind: "subquery", subquery: "scalar", query: query(q) });
}

/** Whether a query has any row. */
export function exists(q: Query | ast.QuerySpec): Expr {
  return new Expr({ kind: "subquery", subquery: "exists", query: query(q) });
}

/** The first column of each of a query's rows, as a list. */
export function listOf(q: Query | ast.QuerySpec): Expr {
  return new Expr({ kind: "subquery", subquery: "list", query: query(q) });
}

/** A span of time in microseconds, to add to a timestamp: `now().add(seconds(60))`. */
export function seconds(n: number): Expr {
  const micros = n * 1_000_000;
  // A span is a signed 64-bit count of microseconds; 2^63 is the first a double holds beyond it.
  if (!(Math.abs(micros) < 2 ** 63)) throw new RangeError(`${n} seconds is not a span the database holds`);
  // Halves away from zero, as the Rust client rounds a fractional span.
  return new Expr({ kind: "value", value: { kind: "int", value: BigInt(Math.sign(micros) * Math.round(Math.abs(micros))) } });
}

export function minutes(n: number): Expr {
  return seconds(n * 60);
}

export function hours(n: number): Expr {
  return seconds(n * 3600);
}

export function days(n: number): Expr {
  return seconds(n * 86_400);
}

/** Escapes `%`, `_` and `\` so the text matches only itself in a `like`. */
export function likeEscape(text: string): string {
  return text.replace(/[%_\\]/g, "\\$&");
}

// ------------------------------------------------------------------ types

/** A column's type: `Type.Int`, `Type.decimal(15, 2)`, `Type.list(Type.Text)`. */
export const Type = {
  Bool: { kind: "bool" },
  Int: { kind: "int" },
  Float: { kind: "float" },
  /** A decimal with no fixed precision or scale. */
  Decimal: { kind: "decimal", precision: null, scale: null },
  Text: { kind: "text" },
  Bytes: { kind: "bytes" },
  Uuid: { kind: "uuid" },
  Timestamp: { kind: "timestamp" },
  Date: { kind: "date" },
  Json: { kind: "json" },
  /** SQL's `NUMERIC(precision, scale)`. */
  decimal(precision: number, scale: number): ast.Type {
    return { kind: "decimal", precision, scale };
  },
  list(of: ast.Type): ast.Type {
    return { kind: "list", of };
  },
} as const satisfies Record<string, ast.Type | ((...args: never[]) => ast.Type)>;

// ------------------------------------------------------------- key ranges

/** One end of a key range: a prefix of the key. */
export const Bound = {
  Unbounded: { kind: "unbounded" } as ast.BoundSpec,
  included(values: Iterable<Arg>): ast.BoundSpec {
    return { kind: "included", values: nodes(values) };
  },
  excluded(values: Iterable<Arg>): ast.BoundSpec {
    return { kind: "excluded", values: nodes(values) };
  },
} as const;

/**
 * A span of keys. Each bound is a prefix of the key: with an index on
 * `(user_id, date)`, `KeyRange.eq([u])` is every entry of that user.
 *
 * A bound that holds a NULL names no key, so the range is empty; to read
 * the rows whose key is NULL, say so with `KeyRange.is` or `.nulls()`.
 */
export class KeyRange {
  readonly spec: ast.KeyRangeSpec;

  constructor(lower: ast.BoundSpec = Bound.Unbounded, upper: ast.BoundSpec = Bound.Unbounded, nulls = false) {
    this.spec = { lower, upper, nulls };
  }

  static all(): KeyRange {
    return new KeyRange();
  }

  /** Every key that starts with these values. */
  static eq(prefix: Iterable<Arg>): KeyRange {
    const values = nodes(prefix);
    return new KeyRange({ kind: "included", values }, { kind: "included", values });
  }

  /** Like `eq`, but a NULL among the values finds the keys that are NULL there: SQL's `IS NOT DISTINCT FROM`. */
  static is(prefix: Iterable<Arg>): KeyRange {
    return KeyRange.eq(prefix).nulls();
  }

  static of(lower: ast.BoundSpec, upper: ast.BoundSpec): KeyRange {
    return new KeyRange(lower, upper);
  }

  /** Keys starting with `prefix`, whose next part lies between the two bounds. */
  static within(prefix: Iterable<Arg>, lower: ast.BoundSpec, upper: ast.BoundSpec): KeyRange {
    const values = nodes(prefix);
    const extend = (bound: ast.BoundSpec): ast.BoundSpec => {
      if (bound.kind === "unbounded") return values.length === 0 ? bound : { kind: "included", values };
      return { kind: bound.kind, values: [...values, ...bound.values] };
    };
    return new KeyRange(extend(lower), extend(upper));
  }

  /** Makes a NULL in a bound stand for the keys that are NULL, instead of emptying the range. */
  nulls(): KeyRange {
    return new KeyRange(this.spec.lower, this.spec.upper, true);
  }
}

function range(value: KeyRange | ast.KeyRangeSpec): ast.KeyRangeSpec {
  return value instanceof KeyRange ? value.spec : value;
}

// ---------------------------------------------------------------- queries

function selectColumns(names: Iterable<string>, short: boolean): ast.SelectSpec[] {
  if (typeof names === "string") throw new TypeError("expected a list of column names; one column is written [name]");
  return Array.from(names, (name) => ({ name: short ? name.slice(name.lastIndexOf(".") + 1) : name, expr: col(name).node }));
}

const ALL_COLUMNS: ast.SelectSpec[] = [{ name: ALL, expr: { kind: "column", table: null, name: ALL } }];

function everyRow(): ast.AccessSpec {
  return { kind: "primary", ranges: [KeyRange.all().spec], reverse: false };
}

function querySpec(table: string): ast.QuerySpec {
  return {
    table,
    alias: null,
    access: everyRow(),
    joins: [],
    unnest: null,
    filter: null,
    groupBy: [],
    having: null,
    orderBy: [],
    offset: 0n,
    limit: null,
    select: [],
    distinct: false,
    withTotal: false,
  };
}

function andFilter(spec: ast.QuerySpec, condition: Arg): ast.QuerySpec {
  return { ...spec, filter: spec.filter === null ? node(condition) : new Expr(spec.filter).and(condition).node };
}

/**
 * A read as the client states it. A query names its way in (the
 * primary key, one index, a tree walk or a full-text search) and the
 * range of keys to read; the server reads those rows in key order and
 * applies the rest in a fixed sequence: joins, `each`, filter, grouping,
 * sort, offset and limit, select. A query that names no way in
 * (`Query.table` as it stands) is given a range by the server where its
 * filter is one, and answers the same either way.
 */
export class Query {
  constructor(readonly spec: ast.QuerySpec) {}

  private with(changes: Partial<ast.QuerySpec>): Query {
    return new Query({ ...this.spec, ...changes });
  }

  /** Every row of a table, in primary key order, until narrowed. */
  static table(name: string): Query {
    return new Query(querySpec(name));
  }

  /** One row of no table, to compute what needs none. */
  static row(): Query {
    return Query.table("");
  }

  alias(alias: string): Query {
    return this.with({ alias });
  }

  /** The row with this primary key. */
  key(key: Iterable<Arg>): Query {
    return this.with({ access: { kind: "primary", ranges: [KeyRange.eq(key).spec], reverse: false } });
  }

  /** The rows with these primary keys, in the order given. */
  keys(keys: Iterable<Iterable<Arg>>): Query {
    return this.with({ access: { kind: "primary", ranges: Array.from(keys, (k) => KeyRange.eq(k).spec), reverse: false } });
  }

  /** Rows whose primary key is in the range. */
  range(keyRange: KeyRange | ast.KeyRangeSpec): Query {
    return this.with({ access: { kind: "primary", ranges: [range(keyRange)], reverse: false } });
  }

  /** Rows through an index, in its order. */
  index(index: string, keyRange: KeyRange | ast.KeyRangeSpec): Query {
    return this.with({ access: { kind: "index", index, ranges: [range(keyRange)], reverse: false } });
  }

  /** Rows through an index, one range after another: `IN` over a key. */
  indexRanges(index: string, ranges: Iterable<KeyRange | ast.KeyRangeSpec>): Query {
    return this.with({ access: { kind: "index", index, ranges: Array.from(ranges, range), reverse: false } });
  }

  /** Reads the chosen range from its far end. */
  reverse(): Query {
    const access = this.spec.access;
    return access.kind === "primary" || access.kind === "index" ? this.with({ access: { ...access, reverse: true } }) : this;
  }

  /**
   * The rows above the start rows: each one's parent, grandparent, and
   * so on. `parent` is the column holding the parent's primary key.
   */
  ancestors(start: Iterable<Iterable<Arg>>, parent: string): Query {
    return this.with({ access: { kind: "walk", start: Array.from(start, nodes), parent, direction: "up", index: null, prune: null, stop: null, maxDepth: 1000 } });
  }

  /** The rows below the start rows. `index` is an index whose first key is the `parent` column. */
  descendants(start: Iterable<Iterable<Arg>>, parent: string, index: string): Query {
    return this.with({ access: { kind: "walk", start: Array.from(start, nodes), parent, direction: "down", index, prune: null, stop: null, maxDepth: 1000 } });
  }

  /** In a walk: do not reach rows for which this is false, nor anything beyond them. */
  prune(keep: Arg): Query {
    const access = this.spec.access;
    return access.kind === "walk" ? this.with({ access: { ...access, prune: node(keep) } }) : this;
  }

  /** In a walk: reach rows for which this is true, but go no further. */
  stopAt(condition: Arg): Query {
    const access = this.spec.access;
    return access.kind === "walk" ? this.with({ access: { ...access, stop: node(condition) } }) : this;
  }

  maxDepth(depth: number): Query {
    const access = this.spec.access;
    return access.kind === "walk" ? this.with({ access: { ...access, maxDepth: depth } }) : this;
  }

  /** Rows whose indexed text holds every word of `query`. */
  search(index: string, prefix: Iterable<Arg>, query: Arg): Query {
    return this.with({ access: { kind: "search", index, prefix: nodes(prefix), query: node(query), matchPrefix: false } });
  }

  /** In a search: each word of the query also matches longer words that start with it. */
  matchPrefix(): Query {
    const access = this.spec.access;
    return access.kind === "search" ? this.with({ access: { ...access, matchPrefix: true } }) : this;
  }

  /** Another query, run once per row so far; rows without a match are dropped. */
  join(q: Query | ast.QuerySpec): Query {
    return this.with({ joins: [...this.spec.joins, { kind: "inner", query: query(q) }] });
  }

  /** The same, keeping rows without a match, with NULLs. */
  leftJoin(q: Query | ast.QuerySpec): Query {
    return this.with({ joins: [...this.spec.joins, { kind: "left", query: query(q) }] });
  }

  /**
   * Sees each row once per distinct item of a list computed from it, the
   * item readable as the column `name`: SQL's `unnest`.
   */
  each(name: string, items: Arg): Query {
    return this.with({ unnest: { name, expr: node(items) } });
  }

  /** Keeps rows for which the condition is true. Called again, both conditions must hold. */
  filter(condition: Arg): Query {
    return new Query(andFilter(this.spec, condition));
  }

  groupBy(keys: Iterable<Arg>): Query {
    return this.with({ groupBy: [...this.spec.groupBy, ...nodes(keys)] });
  }

  having(condition: Arg): Query {
    return this.with({ having: node(condition) });
  }

  orderBy(by: Order | Expr): Query {
    return this.with({ orderBy: [...this.spec.orderBy, order(by)] });
  }

  offset(n: number | bigint): Query {
    return this.with({ offset: BigInt(n) });
  }

  limit(n: number | bigint): Query {
    return this.with({ limit: BigInt(n) });
  }

  /** Returns these columns of the table instead of all of them. */
  columns(names: Iterable<string>): Query {
    return this.with({ select: [...this.spec.select, ...selectColumns(names, true)] });
  }

  /** Adds a named expression to what the query returns. */
  select(name: string, expr: Arg): Query {
    return this.with({ select: [...this.spec.select, { name, expr: node(expr) }] });
  }

  distinct(): Query {
    return this.with({ distinct: true });
  }

  /** Also count the rows the query would give without offset and limit. */
  withTotal(): Query {
    return this.with({ withTotal: true });
  }
}

// ----------------------------------------------------------------- writes

export class Insert {
  constructor(readonly spec: ast.InsertSpec) {}

  private with(changes: Partial<ast.InsertSpec>): Insert {
    return new Insert({ ...this.spec, ...changes });
  }

  static into(table: string): Insert {
    return new Insert({ table, columns: [], source: { kind: "values", rows: [] }, onConflict: { kind: "error" }, returning: [] });
  }

  /** The columns the rows give values for. The rest take their defaults. */
  columns(names: Iterable<string>): Insert {
    if (typeof names === "string") throw new TypeError("expected a list of column names");
    return this.with({ columns: Array.from(names) });
  }

  /** Adds a row, its values in the order of `columns`. */
  values(row: Iterable<Arg>): Insert {
    const rows = this.spec.source.kind === "values" ? this.spec.source.rows : [];
    return this.with({ source: { kind: "values", rows: [...rows, nodes(row)] } });
  }

  /** Sets one column of a single-row insert. */
  set(column: string, value: Arg): Insert {
    const rows = this.spec.source.kind === "values" ? this.spec.source.rows : [];
    const first = [...(rows[0] ?? []), node(value)];
    return this.with({ columns: [...this.spec.columns, column], source: { kind: "values", rows: [first, ...rows.slice(1)] } });
  }

  /** One row per row of the query, its columns in order. */
  fromQuery(q: Query | ast.QuerySpec): Insert {
    return this.with({ source: { kind: "query", query: query(q) } });
  }

  /** Skips a row that breaks any unique index. */
  onConflictDoNothing(): Insert {
    return this.with({ onConflict: { kind: "do_nothing", target: null } });
  }

  /** Skips a row that conflicts on the named unique index. */
  onConflictDoNothingOn(index: string): Insert {
    return this.with({ onConflict: { kind: "do_nothing", target: index } });
  }

  /** On a conflict with the named unique index (`PRIMARY` for the primary key), update the existing row instead. */
  onConflictUpdate(index: string): Insert {
    return this.with({ onConflict: { kind: "do_update", target: index, set: [], filter: null } });
  }

  /** In the conflict update: assign a column. */
  update(column: string, value: Arg): Insert {
    const c = this.spec.onConflict;
    return c.kind === "do_update" ? this.with({ onConflict: { ...c, set: [...c.set, { column, value: node(value) }] } }) : this;
  }

  /** In the conflict update: take the proposed values for these columns. */
  updateExcluded(columns: Iterable<string>): Insert {
    const c = this.spec.onConflict;
    if (c.kind !== "do_update") return this;
    const taken = Array.from(columns, (column): ast.AssignSpec => ({ column, value: { kind: "excluded", name: column } }));
    return this.with({ onConflict: { ...c, set: [...c.set, ...taken] } });
  }

  /** In the conflict update: only update when this holds for the existing row. */
  updateWhere(condition: Arg): Insert {
    const c = this.spec.onConflict;
    return c.kind === "do_update" ? this.with({ onConflict: { ...c, filter: node(condition) } }) : this;
  }

  returning(columns: Iterable<string>): Insert {
    return this.with({ returning: [...this.spec.returning, ...selectColumns(columns, false)] });
  }

  returningExpr(name: string, expr: Arg): Insert {
    return this.with({ returning: [...this.spec.returning, { name, expr: node(expr) }] });
  }

  /** Returns every column of each row, as SQL's `RETURNING *`. */
  returningAll(): Insert {
    return this.with({ returning: ALL_COLUMNS });
  }
}

/**
 * Changes the rows a query finds. The query's table is the one updated;
 * its joins only help choose rows and supply values.
 */
export class Update {
  constructor(readonly spec: ast.UpdateSpec) {}

  private rowsOf(rows: Query): Update {
    return new Update({ ...this.spec, rows: rows.spec });
  }

  static rows(rows: Query | ast.QuerySpec): Update {
    return new Update({ rows: query(rows), set: [], returning: [] });
  }

  /** Every row of the table, until narrowed with `key`, `index` or `filter`. */
  static table(name: string): Update {
    return Update.rows(querySpec(name));
  }

  key(key: Iterable<Arg>): Update {
    return this.rowsOf(new Query(this.spec.rows).key(key));
  }

  range(keyRange: KeyRange | ast.KeyRangeSpec): Update {
    return this.rowsOf(new Query(this.spec.rows).range(keyRange));
  }

  index(index: string, keyRange: KeyRange | ast.KeyRangeSpec): Update {
    return this.rowsOf(new Query(this.spec.rows).index(index, keyRange));
  }

  filter(condition: Arg): Update {
    return this.rowsOf(new Query(this.spec.rows).filter(condition));
  }

  set(column: string, value: Arg): Update {
    return new Update({ ...this.spec, set: [...this.spec.set, { column, value: node(value) }] });
  }

  returning(columns: Iterable<string>): Update {
    return new Update({ ...this.spec, returning: [...this.spec.returning, ...selectColumns(columns, true)] });
  }

  returningExpr(name: string, expr: Arg): Update {
    return new Update({ ...this.spec, returning: [...this.spec.returning, { name, expr: node(expr) }] });
  }

  returningAll(): Update {
    return new Update({ ...this.spec, returning: ALL_COLUMNS });
  }
}

/** Removes the rows a query finds from the query's table. */
export class Delete {
  constructor(readonly spec: ast.DeleteSpec) {}

  private rowsOf(rows: Query): Delete {
    return new Delete({ ...this.spec, rows: rows.spec });
  }

  static rows(rows: Query | ast.QuerySpec): Delete {
    return new Delete({ rows: query(rows), returning: [] });
  }

  static from(table: string): Delete {
    return Delete.rows(querySpec(table));
  }

  key(key: Iterable<Arg>): Delete {
    return this.rowsOf(new Query(this.spec.rows).key(key));
  }

  range(keyRange: KeyRange | ast.KeyRangeSpec): Delete {
    return this.rowsOf(new Query(this.spec.rows).range(keyRange));
  }

  index(index: string, keyRange: KeyRange | ast.KeyRangeSpec): Delete {
    return this.rowsOf(new Query(this.spec.rows).index(index, keyRange));
  }

  filter(condition: Arg): Delete {
    return this.rowsOf(new Query(this.spec.rows).filter(condition));
  }

  returning(columns: Iterable<string>): Delete {
    return new Delete({ ...this.spec, returning: [...this.spec.returning, ...selectColumns(columns, false)] });
  }

  returningExpr(name: string, expr: Arg): Delete {
    return new Delete({ ...this.spec, returning: [...this.spec.returning, { name, expr: node(expr) }] });
  }

  returningAll(): Delete {
    return new Delete({ ...this.spec, returning: ALL_COLUMNS });
  }
}

// ----------------------------------------------------------------- schema

/** A column that cannot be NULL and has no default, until told otherwise. */
export class ColumnDef {
  readonly spec: ast.ColumnSpec;

  constructor(name: string, type: ast.Type);
  constructor(spec: ast.ColumnSpec);
  constructor(name: string | ast.ColumnSpec, type?: ast.Type) {
    this.spec = typeof name === "string" ? { name, type: type!, nullable: false, default: null, autoIncrement: false } : name;
  }

  private with(changes: Partial<ast.ColumnSpec>): ColumnDef {
    return new ColumnDef({ ...this.spec, ...changes });
  }

  nullable(): ColumnDef {
    return this.with({ nullable: true });
  }

  /** What an insert that leaves the column out stores: a constant, or an expression evaluated per row. */
  default(value: Arg): ColumnDef {
    return this.with({ default: node(value) });
  }

  defaultNow(): ColumnDef {
    return this.default(now());
  }

  defaultRandomUuid(): ColumnDef {
    return this.default(call("random_uuid"));
  }

  /** An integer column that numbers rows. */
  autoIncrement(): ColumnDef {
    return this.with({ autoIncrement: true });
  }
}

export class IndexDef {
  readonly spec: ast.IndexSpec;

  constructor(name: string | ast.IndexSpec) {
    this.spec = typeof name === "string" ? { name, keys: [], unique: false, filter: null, index: { kind: "ordered" } } : name;
  }

  private with(changes: Partial<ast.IndexSpec>): IndexDef {
    return new IndexDef({ ...this.spec, ...changes });
  }

  private key(expr: ast.ExprNode, desc = false, each = false): IndexDef {
    return this.with({ keys: [...this.spec.keys, { expr, desc, each }] });
  }

  /** Adds plain columns as keys, ascending. */
  on(columns: Iterable<string>): IndexDef {
    if (typeof columns === "string") throw new TypeError("expected a list of column names");
    return this.with({ keys: [...this.spec.keys, ...Array.from(columns, (c) => ({ expr: col(c).node, desc: false, each: false }))] });
  }

  /** Adds an expression as a key. */
  onExpr(expr: Arg): IndexDef {
    return this.key(node(expr));
  }

  /** Adds a column as a key, descending. */
  onDesc(column: string): IndexDef {
    return this.key(col(column).node, true);
  }

  /** Adds a list-valued expression as a key, indexing the row once under each distinct item. */
  onEach(expr: Arg): IndexDef {
    return this.key(node(expr), false, true);
  }

  unique(): IndexDef {
    return this.with({ unique: true });
  }

  /** Makes the index partial: only rows for which this is true. */
  filter(condition: Arg): IndexDef {
    return this.with({ filter: node(condition) });
  }

  /**
   * Makes this a full-text index over the given text expressions, each
   * with the weight a match in it counts for.
   */
  fullText(config: ast.TextConfig, fields: Iterable<readonly [Arg, number]>): IndexDef {
    return this.with({ index: { kind: "full_text", config, fields: Array.from(fields, ([expr, weight]) => ({ expr: node(expr), weight })) } });
  }
}

export class TableDef {
  readonly spec: ast.TableSpec;

  constructor(name: string | ast.TableSpec) {
    this.spec = typeof name === "string" ? { name, columns: [], primaryKey: [], indexes: [], foreignKeys: [], checks: [], volatile: false } : name;
  }

  private with(changes: Partial<ast.TableSpec>): TableDef {
    return new TableDef({ ...this.spec, ...changes });
  }

  column(column: ColumnDef | ast.ColumnSpec): TableDef {
    return this.with({ columns: [...this.spec.columns, column instanceof ColumnDef ? column.spec : column] });
  }

  primaryKey(columns: Iterable<string>): TableDef {
    if (typeof columns === "string") throw new TypeError("expected a list of column names");
    return this.with({ primaryKey: Array.from(columns) });
  }

  index(index: IndexDef | ast.IndexSpec): TableDef {
    return this.with({ indexes: [...this.spec.indexes, index instanceof IndexDef ? index.spec : index] });
  }

  /** A unique index over plain columns. */
  unique(name: string, columns: Iterable<string>): TableDef {
    return this.index(new IndexDef(name).on(columns).unique());
  }

  /** A foreign key to the parent's primary key, named `<table>_<column>_fkey`. */
  foreignKey(columns: Iterable<string>, refTable: string, onDelete: ast.OnDelete): TableDef {
    return this.foreignKeyTo(columns, refTable, [], onDelete);
  }

  /**
   * A foreign key to columns other than the primary key: they must be
   * the columns of a unique index of `refTable`.
   */
  foreignKeyTo(columns: Iterable<string>, refTable: string, refColumns: Iterable<string>, onDelete: ast.OnDelete): TableDef {
    if (typeof columns === "string" || typeof refColumns === "string") throw new TypeError("expected lists of column names");
    const own = Array.from(columns);
    const key: ast.ForeignKeySpec = { name: `${this.spec.name}_${own.join("_")}_fkey`, columns: own, refTable, refColumns: Array.from(refColumns), onDelete };
    return this.with({ foreignKeys: [...this.spec.foreignKeys, key] });
  }

  /** A row is refused when this is false. NULL passes, as in SQL. */
  check(name: string, expr: Arg): TableDef {
    return this.with({ checks: [...this.spec.checks, { name, expr: node(expr) }] });
  }

  /** A table whose rows need not survive a crash or a restart. */
  volatile(): TableDef {
    return this.with({ volatile: true });
  }
}
