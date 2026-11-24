// Requests as plain data: what the builders make and the codec writes.
//
// Each type here is one structure of docs/PROTOCOL.md, field for field.
// The fluent builders (`Query`, `Insert`, `col` and the rest) produce
// these; `describe()` returns them. They hold no behaviour.

import type { DbError } from "./errors.js";
import type { WireValue } from "./values.js";

// ------------------------------------------------------------------ enums
//
// Each choice the wire makes with a number has a name here. The tables
// give the numbers; protocol/fixtures/vectors.json holds the same tables,
// and the tests compare them.

export const CMP_OPS = { eq: 0, ne: 1, lt: 2, le: 3, gt: 4, ge: 5, is_distinct_from: 6, is_not_distinct_from: 7 } as const;
export type CmpOp = keyof typeof CMP_OPS;

export const ARITH_OPS = { add: 0, sub: 1, mul: 2, div: 3, mod: 4 } as const;
export type ArithOp = keyof typeof ARITH_OPS;

export const AGG_FUNCS = { count: 0, sum: 1, min: 2, max: 3, avg: 4, collect: 5, bool_or: 6, bool_and: 7 } as const;
export type AggFunc = keyof typeof AGG_FUNCS;

export const SUBQUERY_KINDS = { scalar: 0, exists: 1, list: 2 } as const;
export type SubqueryKind = keyof typeof SUBQUERY_KINDS;

/** The functions an expression can call. docs/PROTOCOL.md says what each takes and gives. */
export const FUNCS = {
  lower: 1,
  upper: 2,
  length: 3,
  octet_length: 4,
  trim: 5,
  left: 6,
  substr: 7,
  substr_bytes: 8,
  strpos: 9,
  concat: 10,
  replace: 11,
  hex: 12,
  similarity: 13,
  abs: 20,
  ceil: 21,
  floor: 22,
  round: 23,
  power: 24,
  least: 25,
  greatest: 26,
  sign: 27,
  coalesce: 30,
  nullif: 31,
  now: 40,
  parse_timestamp: 41,
  random_uuid: 42,
  date_part: 43,
  json_get: 50,
  json_get_text: 51,
  json_typeof: 52,
  json_len: 53,
  json_contains: 54,
  json_has_key: 55,
  json_has_any: 56,
  json_has_all: 57,
  json_merge: 58,
  json_strip_nulls: 59,
  json_remove: 60,
  json_elems: 61,
  json_pluck: 62,
  json_array: 63,
  json_object: 64,
  to_json: 65,
  json_texts: 66,
  list_len: 80,
  list_overlaps: 81,
  list_concat: 82,
  list_distinct: 83,
  list_position: 84,
  matches: 90,
  match_rank: 91,
  headline: 92,
} as const;
export type Func = keyof typeof FUNCS;

export const DIRECTIONS = { up: 0, down: 1 } as const;
/** `up`: from a row to its parent, and on up. `down`: to its children, and on down. */
export type Direction = keyof typeof DIRECTIONS;

export const JOIN_KINDS = { inner: 0, left: 1 } as const;
/** `inner` drops rows without a match; `left` keeps them, with NULL for the joined columns. */
export type JoinKind = keyof typeof JOIN_KINDS;

export const ON_DELETE = { restrict: 0, cascade: 1, set_null: 2 } as const;
/** What happens to a child row when its parent is deleted. */
export type OnDelete = keyof typeof ON_DELETE;

export const TEXT_CONFIGS = { simple: 0, english: 1 } as const;
/** How text is cut into words: `simple`, or `english` without common words and endings. */
export type TextConfig = keyof typeof TEXT_CONFIGS;

export const TXN_KINDS = { read: 0, exclusive: 1, optimistic: 2 } as const;
/** How a transaction stands to the database's writers. */
export type TxnKind = keyof typeof TXN_KINDS;

// ------------------------------------------------------------------ types

/** A column's type. */
export type Type =
  | { kind: "bool" | "int" | "float" | "text" | "bytes" | "uuid" | "timestamp" | "date" | "json" }
  /** With precision and scale, SQL's `NUMERIC(precision, scale)`; with both `null`, unlimited. */
  | { kind: "decimal"; precision: number | null; scale: number | null }
  | { kind: "list"; of: Type };

// ------------------------------------------------------------ expressions

export type ExprNode =
  /** A constant. */
  | { kind: "value"; value: WireValue }
  /** A column, optionally of one named table of the query. */
  | { kind: "column"; table: string | null; name: string }
  /** In an upsert's update: the row that could not be inserted. */
  | { kind: "excluded"; name: string }
  /** In an update's `returning`: the row as it was before. */
  | { kind: "old"; name: string }
  /** In a tree walk: how many steps from a start row. */
  | { kind: "depth" }
  /** In a full-text search: how well the row matches. */
  | { kind: "rank" }
  | { kind: "not"; expr: ExprNode }
  | { kind: "and"; parts: ExprNode[] }
  | { kind: "or"; parts: ExprNode[] }
  | { kind: "cmp"; op: CmpOp; left: ExprNode; right: ExprNode }
  /** `IS NULL`, or `IS NOT NULL` when negated. */
  | { kind: "is_null"; expr: ExprNode; negated: boolean }
  | { kind: "in"; expr: ExprNode; list: ExprNode; negated: boolean }
  | { kind: "like"; expr: ExprNode; pattern: ExprNode; caseInsensitive: boolean; negated: boolean }
  | { kind: "arith"; op: ArithOp; left: ExprNode; right: ExprNode }
  | { kind: "neg"; expr: ExprNode }
  | { kind: "call"; func: Func; args: ExprNode[] }
  /** The first branch whose condition is true, else `otherwise`, else NULL. */
  | { kind: "case"; branches: { when: ExprNode; then: ExprNode }[]; otherwise: ExprNode | null }
  /** Converts to a type; when lenient, a value that cannot be converted gives NULL. */
  | { kind: "cast"; expr: ExprNode; to: Type; lenient: boolean }
  /** A list of the items' values. */
  | { kind: "list"; items: ExprNode[] }
  | { kind: "agg"; func: AggFunc; arg: ExprNode | null; filter: ExprNode | null; distinct: boolean }
  | { kind: "subquery"; subquery: SubqueryKind; query: QuerySpec };

// ---------------------------------------------------------------- queries

export interface SelectSpec {
  name: string;
  expr: ExprNode;
}

export interface OrderSpec {
  expr: ExprNode;
  desc: boolean;
  /** Where NULLs go. `null` puts them last ascending and first descending, as Postgres does. */
  nullsLast: boolean | null;
}

/** One end of a key range: a prefix of the key. */
export type BoundSpec = { kind: "unbounded" } | { kind: "included"; values: ExprNode[] } | { kind: "excluded"; values: ExprNode[] };

export interface KeyRangeSpec {
  lower: BoundSpec;
  upper: BoundSpec;
  /** Whether a NULL in a bound stands for the keys that are NULL. */
  nulls: boolean;
}

export type AccessSpec =
  /** Rows by primary key, in key order. */
  | { kind: "primary"; ranges: KeyRangeSpec[]; reverse: boolean }
  /** Rows through an index, in the index's order. */
  | { kind: "index"; index: string; ranges: KeyRangeSpec[]; reverse: boolean }
  /** Rows of a tree held in one table. */
  | {
      kind: "walk";
      start: ExprNode[][];
      parent: string;
      direction: Direction;
      index: string | null;
      prune: ExprNode | null;
      stop: ExprNode | null;
      maxDepth: number;
    }
  /** Rows matching a full-text search, through a full-text index. */
  | { kind: "search"; index: string; prefix: ExprNode[]; query: ExprNode; matchPrefix: boolean };

export interface JoinSpec {
  kind: JoinKind;
  query: QuerySpec;
}

export interface QuerySpec {
  table: string;
  alias: string | null;
  access: AccessSpec;
  joins: JoinSpec[];
  unnest: SelectSpec | null;
  filter: ExprNode | null;
  groupBy: ExprNode[];
  having: ExprNode | null;
  orderBy: OrderSpec[];
  offset: bigint;
  limit: bigint | null;
  /** Empty for every column of `table`. */
  select: SelectSpec[];
  distinct: boolean;
  withTotal: boolean;
}

export interface AssignSpec {
  column: string;
  value: ExprNode;
}

export type OnConflictSpec =
  /** A row that breaks a unique index fails the statement. */
  | { kind: "error" }
  /** Such a row is skipped: on the named index, or on any unique index when none is named. */
  | { kind: "do_nothing"; target: string | null }
  /** The existing row is updated instead, if `filter` holds for it. */
  | { kind: "do_update"; target: string; set: AssignSpec[]; filter: ExprNode | null };

export interface InsertSpec {
  table: string;
  /** The columns the rows give values for. The rest take their defaults. */
  columns: string[];
  source: { kind: "values"; rows: ExprNode[][] } | { kind: "query"; query: QuerySpec };
  onConflict: OnConflictSpec;
  returning: SelectSpec[];
}

export interface UpdateSpec {
  rows: QuerySpec;
  set: AssignSpec[];
  returning: SelectSpec[];
}

export interface DeleteSpec {
  rows: QuerySpec;
  returning: SelectSpec[];
}

// ----------------------------------------------------------------- schema

export interface ColumnSpec {
  name: string;
  type: Type;
  nullable: boolean;
  default: ExprNode | null;
  autoIncrement: boolean;
}

export interface IndexKeySpec {
  expr: ExprNode;
  desc: boolean;
  /** The expression gives a list, and the row is indexed once under each distinct item. */
  each: boolean;
}

export interface IndexSpec {
  name: string;
  keys: IndexKeySpec[];
  unique: boolean;
  filter: ExprNode | null;
  index: { kind: "ordered" } | { kind: "full_text"; config: TextConfig; fields: { expr: ExprNode; weight: number }[] };
}

export interface ForeignKeySpec {
  name: string;
  columns: string[];
  refTable: string;
  /** Empty, when sent, for the parent's primary key; the server fills it in. */
  refColumns: string[];
  onDelete: OnDelete;
}

export interface CheckSpec {
  name: string;
  expr: ExprNode;
}

export interface TableSpec {
  name: string;
  columns: ColumnSpec[];
  primaryKey: string[];
  indexes: IndexSpec[];
  foreignKeys: ForeignKeySpec[];
  checks: CheckSpec[];
  volatile: boolean;
}

// --------------------------------------------------------------- messages

export type Ddl =
  | { kind: "create_table"; def: TableSpec; ifNotExists: boolean }
  | { kind: "drop_table"; name: string; ifExists: boolean }
  | { kind: "add_column"; table: string; column: ColumnSpec; ifNotExists: boolean }
  | { kind: "drop_column"; table: string; column: string; ifExists: boolean }
  | { kind: "create_index"; table: string; index: IndexSpec; ifNotExists: boolean }
  | { kind: "drop_index"; table: string; index: string; ifExists: boolean };

export type AdminRequest =
  | { kind: "create_database"; name: string; password: string; ifNotExists: boolean }
  | { kind: "drop_database"; name: string; ifExists: boolean }
  | { kind: "set_password"; name: string; password: string }
  | { kind: "list_databases" }
  | { kind: "checkpoint" }
  | { kind: "check" }
  | { kind: "stats" }
  | { kind: "dump"; databases: string[] }
  | { kind: "restore_begin"; replace: boolean }
  | { kind: "restore_chunk"; bytes: Uint8Array }
  | { kind: "restore_end" };

export type Request =
  | { kind: "hello"; version: number; database: string; password: string; client: string }
  | { kind: "ping" }
  | { kind: "begin"; txn: TxnKind; lockWaitMs: bigint }
  | { kind: "commit" }
  | { kind: "rollback" }
  | { kind: "savepoint"; name: string }
  | { kind: "rollback_to"; name: string }
  | { kind: "release"; name: string }
  | { kind: "query"; query: QuerySpec }
  | { kind: "insert"; insert: InsertSpec }
  | { kind: "update"; update: UpdateSpec }
  | { kind: "delete"; delete: DeleteSpec }
  | { kind: "ddl"; ddl: Ddl }
  | { kind: "describe" }
  | { kind: "lock"; name: string; waitMs: bigint }
  | { kind: "unlock"; name: string }
  | { kind: "now" }
  | { kind: "admin"; admin: AdminRequest };

export interface RowSet {
  columns: string[];
  rows: WireValue[][];
  /** Rows inserted, updated or deleted by a write; rows returned by a read. */
  affected: bigint;
  /** For a query `withTotal`: rows before offset and limit. */
  total: bigint | null;
}

export type Response =
  | { kind: "ok" }
  | { kind: "hello"; version: number; server: string }
  /** One frame of a result; `more` says whether another frame of the same result follows. */
  | ({ kind: "rows"; more: boolean } & RowSet)
  | { kind: "tables"; tables: TableSpec[] }
  | { kind: "flag"; value: boolean }
  | { kind: "time"; micros: bigint }
  | { kind: "names"; names: string[] }
  | { kind: "text"; text: string }
  | { kind: "chunk"; bytes: Uint8Array }
  | { kind: "error"; error: DbError };
