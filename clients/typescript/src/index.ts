// The TypeScript client for the database server.
//
//     import { Client, ColumnDef, Insert, KeyRange, Query, TableDef, Type } from "database-client";
//
//     const db = await Client.connect("database://shop:secret@database:5454");
//     await db.createTableIfNotExists(
//       new TableDef("users")
//         .column(new ColumnDef("id", Type.Uuid).defaultRandomUuid())
//         .column(new ColumnDef("username", Type.Text))
//         .column(new ColumnDef("created", Type.Timestamp).defaultNow())
//         .primaryKey(["id"])
//         .unique("users_username_key", ["username"]),
//     );
//     const id = (await db.insert(Insert.into("users").set("username", "ann").returning(["id"]))).one().get("id");
//     const ann = await db.queryOpt(Query.table("users").index("users_username_key", KeyRange.eq(["ann"])));
//
// There is no SQL and no planner: a query says which key or index to read
// and in what range, and the server does exactly that. docs/CLIENT.md in
// the repository describes the vocabulary (for the Rust client, which
// this one follows name for name); docs/PROTOCOL.md is the wire protocol.

export * as ast from "./ast.js";
export type { AggFunc, ArithOp, CmpOp, Direction, Func, JoinKind, OnDelete, SubqueryKind, TextConfig, TxnKind } from "./ast.js";
export {
  ALL,
  Bound,
  ColumnDef,
  Delete,
  Expr,
  IndexDef,
  Insert,
  KeyRange,
  NULL,
  Order,
  PRIMARY,
  Query,
  TableDef,
  Type,
  Update,
  all,
  any,
  avg,
  call,
  cases,
  coalesce,
  col,
  collect,
  concat,
  count,
  days,
  depth,
  excluded,
  exists,
  greatest,
  hours,
  least,
  likeEscape,
  list,
  listOf,
  max,
  min,
  minutes,
  node,
  now,
  old,
  rank,
  scalar,
  seconds,
  sum,
  val,
  when,
} from "./builders.js";
export type { Arg } from "./builders.js";
export { Admin, Client, Db, LockGuard, Row, Rows, Transaction } from "./client.js";
export { MAX_FRAME, PROTOCOL_VERSION } from "./codec.js";
export { DEFAULT_PORT, config, configFromUrl } from "./conn.js";
export type { Config } from "./conn.js";
export { DbError, ErrorCode, errorCodeName } from "./errors.js";
export type { ErrorCodeName } from "./errors.js";
export { CalendarDate, Decimal, Float, Json, Timestamp, Uuid, float, fromWire, json, toWire } from "./values.js";
export type { JsonValue, ReadValue, Value, WireValue } from "./values.js";
