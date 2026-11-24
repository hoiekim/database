"""The golden vectors: this client writes the bytes the server's own code
writes, and reads what it writes as the same messages.

``protocol/fixtures/vectors.json`` is kept by a test of the Rust crate
(``protocol/tests/vectors.rs``). Every request in it is built here from
its description and must encode to the same bytes; every response is
decoded here from its bytes and must come out as described.
"""

from __future__ import annotations

import re
import unittest
from typing import Any, Callable, Dict

from database_client import ErrorCode, ast, codec

from . import describe

FIXTURES = describe.load()
VECTORS = FIXTURES["vectors"]


def snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).upper()


class Tables(unittest.TestCase):
    """The numbers this client gives its names are the server's."""

    def same(self, enum: Any, table: Dict[str, int], rename: Callable[[str], str] = str.upper) -> None:
        ours = {member.name: int(member) for member in enum}
        theirs = {rename(name): number for name, number in table.items()}
        self.assertEqual(ours, theirs)

    def test_every_name_has_the_servers_number(self) -> None:
        tables = FIXTURES["tables"]
        self.same(ast.Func, tables["functions"])
        self.same(ast.CmpOp, tables["comparisons"])
        self.same(ast.ArithOp, tables["arithmetic"])
        self.same(ast.AggFunc, tables["aggregates"])
        self.same(ast.SubqueryKind, tables["subqueries"])
        self.same(ErrorCode, tables["error_codes"], snake)

    def test_the_limits_are_the_servers(self) -> None:
        self.assertEqual(codec.PROTOCOL_VERSION, FIXTURES["protocol_version"])
        self.assertEqual(codec.MAX_FRAME, FIXTURES["max_frame"])
        self.assertEqual(codec.MAX_DEPTH, FIXTURES["max_nesting"])


class Requests(unittest.TestCase):
    def test_every_request_encodes_to_the_servers_bytes(self) -> None:
        seen = 0
        for vector in VECTORS:
            if vector["kind"] != "request":
                continue
            with self.subTest(vector["name"]):
                request = describe.request_from(vector["message"])
                encoded = codec.encode_request(request, int(vector["timeout_ms"]))
                self.assertEqual(encoded.hex(), vector["hex"])
                seen += 1
        self.assertGreater(seen, 50)

    def test_what_is_encoded_decodes_to_the_same(self) -> None:
        """The statements and definitions inside requests, read back with
        this client's own decoders (which the server's answers to
        ``describe`` need) and written again."""
        readers: Dict[int, Callable[[codec.Reader], Any]] = {
            codec.QUERY: codec.get_query,
            codec.INSERT: codec.get_insert,
            codec.UPDATE: codec.get_update,
            codec.DELETE: codec.get_delete,
        }
        writers: Dict[int, Callable[[codec.Writer, Any], None]] = {
            codec.QUERY: codec.put_query,
            codec.INSERT: codec.put_insert,
            codec.UPDATE: codec.put_update,
            codec.DELETE: codec.put_delete,
        }
        seen = 0
        for vector in VECTORS:
            if vector["kind"] != "request":
                continue
            frame = bytes.fromhex(vector["hex"])
            r = codec.Reader(frame, exact=True)
            r.varint()
            tag = r.u8()
            if tag not in readers:
                continue
            with self.subTest(vector["name"]):
                start = r.pos
                decoded = readers[tag](r)
                r.finish()
                w = codec.Writer()
                writers[tag](w, decoded)
                self.assertEqual(bytes(w.buf), frame[start:])
                if tag == codec.QUERY:
                    self.assertEqual(describe.query_to(decoded), vector["message"]["query"])
                seen += 1
        self.assertGreater(seen, 20)


class Responses(unittest.TestCase):
    def test_every_response_decodes_to_what_it_describes(self) -> None:
        names = {number: name for name, number in FIXTURES["tables"]["error_codes"].items()}
        seen = 0
        for vector in VECTORS:
            if vector["kind"] != "response":
                continue
            with self.subTest(vector["name"]):
                response = codec.decode_response(bytes.fromhex(vector["hex"]), exact=True)
                self.assertEqual(describe.response_to(response, names), vector["message"])
                seen += 1
        self.assertGreater(seen, 30)

    def test_tables_survive_a_round_trip(self) -> None:
        for vector in VECTORS:
            if vector["name"] != "response.tables":
                continue
            frame = bytes.fromhex(vector["hex"])
            response = codec.decode_response(frame, exact=True)
            w = codec.Writer()
            w.u8(codec.R_TABLES)
            w.varint(len(response.value))
            for table in response.value:
                codec.put_table(w, table)
            self.assertEqual(bytes(w.buf), frame)
            self.assertEqual([describe.table_from(t) for t in vector["message"]["tables"]], response.value)

    def test_a_damaged_frame_is_an_error_never_a_crash(self) -> None:
        for vector in VECTORS:
            if vector["kind"] != "response":
                continue
            frame = bytes.fromhex(vector["hex"])
            for cut in range(len(frame)):
                with self.assertRaises(codec.ProtocolError):
                    codec.decode_response(frame[:cut])
            with self.assertRaises(codec.ProtocolError):
                codec.decode_response(frame + b"\x00")
        state = 0x1234_5678_9ABC_DEF0
        frames = [bytes.fromhex(v["hex"]) for v in VECTORS if v["kind"] == "response" and len(v["hex"]) > 40]
        for i in range(5_000):
            state ^= (state << 13) & 0xFFFF_FFFF_FFFF_FFFF
            state ^= state >> 7
            state ^= (state << 17) & 0xFFFF_FFFF_FFFF_FFFF
            bad = bytearray(frames[i % len(frames)])
            bad[state % len(bad)] ^= ((state >> 32) & 0xFF) | 1
            try:
                codec.decode_response(bytes(bad))
            except codec.ProtocolError:
                pass


if __name__ == "__main__":
    unittest.main()
