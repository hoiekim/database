// The golden vectors: this client writes the bytes the server's own code
// writes, and reads what it writes as the same messages.
//
// protocol/fixtures/vectors.json is kept by a test of the Rust crate
// (protocol/tests/vectors.rs). Every request in it is built here from its
// description and must encode to the same bytes; every response is
// decoded here from its bytes and must come out as described.

import assert from "node:assert/strict";
import { test } from "node:test";

import * as ast from "../src/ast.js";
import * as codec from "../src/codec.js";
import { ErrorCode } from "../src/index.js";
import { MAX_SCALE } from "../src/values.js";
import { hex, load, requestFrom, responseTo, toJson, unhex } from "./describe.js";

const fixtures = load();

test("every name has the server's number", () => {
  const tables = fixtures.tables;
  assert.deepEqual({ ...ast.FUNCS }, tables.functions);
  assert.deepEqual({ ...ast.CMP_OPS }, tables.comparisons);
  assert.deepEqual({ ...ast.ARITH_OPS }, tables.arithmetic);
  assert.deepEqual({ ...ast.AGG_FUNCS }, tables.aggregates);
  assert.deepEqual({ ...ast.SUBQUERY_KINDS }, tables.subqueries);
  assert.deepEqual({ ...ErrorCode }, tables.error_codes);
  assert.equal(codec.PROTOCOL_VERSION, fixtures.protocol_version);
  assert.equal(codec.MAX_FRAME, fixtures.max_frame);
  assert.equal(codec.MAX_DEPTH, fixtures.max_nesting);
  assert.equal(MAX_SCALE, fixtures.max_decimal_scale);
});

test("every request encodes to the server's bytes", () => {
  let seen = 0;
  for (const vector of fixtures.vectors) {
    if (vector.kind !== "request") continue;
    const encoded = codec.encodeRequest(requestFrom(vector.message), BigInt(vector.timeout_ms!));
    assert.equal(hex(encoded), vector.hex, vector.name);
    seen++;
  }
  assert.ok(seen > 50);
});

test("what is encoded decodes to the same", () => {
  // The statements inside requests, read back with this client's own
  // decoders (which the server's answers to `describe` need) and written
  // again.
  const readers: Record<number, [(r: codec.Reader) => unknown, (w: codec.Writer, v: never) => void, string]> = {
    9: [codec.getQuery, codec.putQuery, "query"],
    10: [codec.getInsert, codec.putInsert, "insert"],
    11: [codec.getUpdate, codec.putUpdate, "update"],
    12: [codec.getDelete, codec.putDelete, "delete"],
  };
  let seen = 0;
  for (const vector of fixtures.vectors) {
    if (vector.kind !== "request") continue;
    const frame = unhex(vector.hex);
    const r = new codec.Reader(frame);
    r.varint();
    const entry = readers[r.u8()];
    if (!entry) continue;
    const [get, put, key] = entry;
    const start = r.pos;
    const decoded = get(r);
    r.finish();
    const w = new codec.Writer();
    put(w, decoded as never);
    assert.equal(hex(w.bytes()), hex(frame.subarray(start)), vector.name);
    assert.deepEqual(toJson(decoded), vector.message[key], vector.name);
    seen++;
  }
  assert.ok(seen > 20);
});

test("every response decodes to what it describes", () => {
  let seen = 0;
  for (const vector of fixtures.vectors) {
    if (vector.kind !== "response") continue;
    const response = codec.decodeResponse(unhex(vector.hex));
    assert.deepEqual(responseTo(response), vector.message, vector.name);
    seen++;
  }
  assert.ok(seen > 30);
});

test("tables survive a round trip", () => {
  const vector = fixtures.vectors.find((v) => v.name === "response.tables")!;
  const frame = unhex(vector.hex);
  const response = codec.decodeResponse(frame);
  assert.equal(response.kind, "tables");
  if (response.kind !== "tables") return;
  const w = new codec.Writer();
  w.u8(4);
  w.list(response.tables, codec.putTable);
  assert.equal(hex(w.bytes()), vector.hex);
});

test("a damaged frame is an error, never a crash", () => {
  const frames = fixtures.vectors.filter((v) => v.kind === "response").map((v) => unhex(v.hex));
  for (const frame of frames) {
    for (let cut = 0; cut < frame.length; cut++) {
      assert.throws(() => codec.decodeResponse(frame.subarray(0, cut)), codec.ProtocolError);
    }
    const longer = new Uint8Array(frame.length + 1);
    longer.set(frame);
    assert.throws(() => codec.decodeResponse(longer), codec.ProtocolError);
  }
  const large = frames.filter((f) => f.length > 20);
  let state = 0x1234_5678_9abc_def0n;
  const mask = (1n << 64n) - 1n;
  for (let i = 0; i < 5_000; i++) {
    state ^= (state << 13n) & mask;
    state ^= state >> 7n;
    state ^= (state << 17n) & mask;
    const bad = Uint8Array.from(large[i % large.length]!);
    bad[Number(state % BigInt(bad.length))]! ^= Number((state >> 32n) & 0xffn) | 1;
    try {
      codec.decodeResponse(bad);
    } catch (e) {
      assert.ok(e instanceof codec.ProtocolError, String(e));
    }
  }
});
