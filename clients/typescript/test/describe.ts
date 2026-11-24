// Between the golden vectors' JSON descriptions and this client's data.
//
// protocol/fixtures/vectors.json describes each message in JSON (the form
// protocol/tests/vectors.rs writes). This client's plain data (`ast.ts`)
// has the same shape with camelCase keys, so most of the way between the
// two is renaming keys; what is left is the leaves JSON cannot hold:
// 64-bit integers (strings there, bigints here), bytes (hex there),
// floats (their bits there) and values, which have a form of their own.

import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import type { ast } from "../src/index.js";
import { type WireValue, errorCodeName } from "../src/index.js";

// dist/test/describe.js -> the repository's root.
const here = dirname(fileURLToPath(import.meta.url));
export const REPOSITORY = resolve(here, "..", "..", "..", "..");

export interface Vector {
  name: string;
  kind: "request" | "response";
  canonical: boolean;
  timeout_ms?: string;
  message: Record<string, unknown>;
  hex: string;
}

export interface Fixtures {
  protocol_version: number;
  max_frame: number;
  max_nesting: number;
  max_decimal_scale: number;
  tables: Record<"functions" | "error_codes" | "comparisons" | "arithmetic" | "aggregates" | "subqueries", Record<string, number>>;
  vectors: Vector[];
}

export function load(): Fixtures {
  return JSON.parse(readFileSync(resolve(REPOSITORY, "protocol", "fixtures", "vectors.json"), "utf8")) as Fixtures;
}

export function hex(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("hex");
}

export function unhex(text: string): Uint8Array {
  return new Uint8Array(Buffer.from(text, "hex"));
}

function floatFrom(bits: string): number {
  return new DataView(unhex(bits).buffer).getFloat64(0, false);
}

function floatTo(value: number): string {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setFloat64(0, value, false);
  return hex(bytes);
}

type Json = Record<string, unknown>;

export function valueFrom(d: Json): WireValue {
  switch (d.kind) {
    case "null":
      return { kind: "null" };
    case "bool":
      return { kind: "bool", value: d.value as boolean };
    case "int":
      return { kind: "int", value: BigInt(d.value as string) };
    case "float":
      return { kind: "float", value: floatFrom(d.bits as string) };
    case "decimal":
      return { kind: "decimal", mantissa: BigInt(d.mantissa as string), scale: d.scale as number };
    case "text":
      return { kind: "text", value: d.value as string };
    case "bytes":
      return { kind: "bytes", value: unhex(d.hex as string) };
    case "uuid":
      return { kind: "uuid", value: unhex(d.hex as string) };
    case "timestamp":
      return { kind: "timestamp", micros: BigInt(d.micros as string) };
    case "date":
      return { kind: "date", days: d.days as number };
    case "json":
      return { kind: "json", text: d.text as string };
    case "list":
      return { kind: "list", items: (d.items as Json[]).map(valueFrom) };
    default:
      throw new Error(`a value of kind ${String(d.kind)}`);
  }
}

export function valueTo(v: WireValue): Json {
  switch (v.kind) {
    case "null":
      return { kind: "null" };
    case "bool":
    case "text":
      return { kind: v.kind, value: v.value };
    case "int":
      return { kind: "int", value: v.value.toString() };
    case "float":
      return { kind: "float", bits: floatTo(v.value) };
    case "decimal":
      return { kind: "decimal", mantissa: v.mantissa.toString(), scale: v.scale };
    case "bytes":
    case "uuid":
      return { kind: v.kind, hex: hex(v.value) };
    case "timestamp":
      return { kind: "timestamp", micros: v.micros.toString() };
    case "date":
      return { kind: "date", days: v.days };
    case "json":
      return { kind: "json", text: v.text };
    case "list":
      return { kind: "list", items: v.items.map(valueTo) };
  }
}

const camel = (key: string): string => key.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase());
const snake = (key: string): string => key.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`);

/** The fields that hold unsigned 64-bit numbers, written as strings in the fixtures. */
const U64 = new Set(["offset", "limit", "lock_wait_ms", "wait_ms"]);

/**
 * A request, or any part of one, from its description. Not for
 * responses: their rows hold values outside any expression.
 */
export function fromJson(d: unknown): unknown {
  if (Array.isArray(d)) return d.map(fromJson);
  if (d === null || typeof d !== "object") return d;
  const o = d as Json;
  // A constant in an expression: its value has a form of its own.
  if (o.kind === "value") return { kind: "value", value: valueFrom(o.value as Json) };
  const out: Json = {};
  for (const [key, v] of Object.entries(o)) {
    if (U64.has(key)) out[camel(key)] = v === null ? null : BigInt(v as string);
    else if (key === "hex") out.bytes = unhex(v as string);
    else if (key === "weight_bits") out.weight = floatFrom(v as string);
    else out[camel(key)] = fromJson(v);
  }
  return out;
}

/** The description of a query, a definition, or any part of one. */
export function toJson(d: unknown): unknown {
  if (Array.isArray(d)) return d.map(toJson);
  if (typeof d === "bigint") return d.toString();
  if (d === null || typeof d !== "object") return d;
  const o = d as Json;
  if (o.kind === "value") return { kind: "value", value: valueTo(o.value as WireValue) };
  const out: Json = {};
  for (const [key, v] of Object.entries(o)) {
    if (key === "weight") out.weight_bits = floatTo(v as number);
    else if (v instanceof Uint8Array) out.hex = hex(v);
    else out[snake(key)] = toJson(v);
  }
  return out;
}

export function requestFrom(d: Json): ast.Request {
  return fromJson(d) as ast.Request;
}

/** Describes a response as this client decoded it. */
export function responseTo(r: ast.Response): Json {
  switch (r.kind) {
    case "rows":
      return {
        kind: "rows",
        more: r.more,
        columns: r.columns,
        affected: r.affected.toString(),
        total: r.total === null ? null : r.total.toString(),
        rows: r.rows.map((row) => row.map(valueTo)),
      };
    case "tables":
      return { kind: "tables", tables: r.tables.map(toJson) };
    case "time":
      return { kind: "time", micros: r.micros.toString() };
    case "chunk":
      return { kind: "chunk", hex: hex(r.bytes) };
    case "error":
      return {
        kind: "error",
        code: r.error.code,
        code_name: errorCodeName(r.error.code),
        message: r.error.message,
        constraint: r.error.constraint,
        table: r.error.table,
      };
    default:
      return { ...r };
  }
}
