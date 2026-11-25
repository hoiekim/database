"""How Python values stand for the database's, and the client's own parts
that need no server: URLs and rows."""

from __future__ import annotations

import ast as python_ast
import datetime as dt
import gc
import threading
import time
import unittest
import uuid
from decimal import Decimal, getcontext
from pathlib import Path
from typing import Any
from unittest import mock

from database_client import Config, Date, DbError, ErrorCode, Json, Rows, Timestamp, Tls, ast, codec
from database_client import conn as connection


def encode(value: Any) -> bytes:
    w = codec.Writer()
    codec.put_value(w, value)
    return bytes(w.buf)


def decode(data: bytes, exact: bool = False) -> Any:
    r = codec.Reader(data, exact)
    value = codec.get_value(r)
    r.finish()
    return value


def back(value: Any, exact: bool = False) -> Any:
    return decode(encode(value), exact)


class ValueMapping(unittest.TestCase):
    def test_each_python_type_has_one_kind(self) -> None:
        tags = {
            codec.TAG_NULL: None,
            codec.TAG_TRUE: True,
            codec.TAG_FALSE: False,
            codec.TAG_INT: 7,
            codec.TAG_FLOAT: 7.0,
            codec.TAG_DECIMAL: Decimal("7"),
            codec.TAG_TEXT: "7",
            codec.TAG_BYTES: b"7",
            codec.TAG_UUID: uuid.UUID(int=7),
            codec.TAG_TIMESTAMP: dt.datetime(2026, 1, 1),
            codec.TAG_DATE: dt.date(2026, 1, 1),
            codec.TAG_JSON: {"seven": 7},
            codec.TAG_LIST: [7],
        }
        for tag, value in tags.items():
            self.assertEqual(encode(value)[0], tag, value)
        self.assertEqual(encode(bytearray(b"7")), encode(b"7"))
        self.assertEqual(encode(memoryview(b"7")), encode(b"7"))
        self.assertEqual(encode((7,)), encode([7]))
        self.assertEqual(encode(Json([7]))[0], codec.TAG_JSON)
        self.assertEqual(encode(Json("7"))[0], codec.TAG_JSON)
        self.assertEqual(encode(Timestamp(7))[0], codec.TAG_TIMESTAMP)
        self.assertEqual(encode(Date(7))[0], codec.TAG_DATE)

    def test_integers_are_64_bit(self) -> None:
        for n in [0, 1, -1, 63, 64, -64, -65, 2**53, 2**63 - 1, -(2**63)]:
            self.assertEqual(back(n), n)
            self.assertIs(type(back(n)), int)
        for n in [2**63, -(2**63) - 1]:
            with self.assertRaises(ValueError):
                encode(n)
        self.assertEqual(encode(0), b"\x03\x00")
        self.assertEqual(encode(-1), b"\x03\x01")
        self.assertEqual(encode(1), b"\x03\x02")

    def test_decimals_are_exact_whatever_the_context(self) -> None:
        getcontext().prec = 5
        try:
            for text in ["0", "-12345.67", "0.000001", "5.00", "123456789012345678.90", "170141183460469231731687303715884105727", "-0.000000000000000000000000000001"]:
                self.assertEqual(format(back(Decimal(text)), "f"), text)
        finally:
            getcontext().prec = 28
        self.assertEqual(str(back(Decimal("1E+3"))), "1000")
        self.assertEqual(str(back(Decimal("-0"))), "0")
        for bad in ["NaN", "Infinity", "1e-31", "170141183460469231731687303715884105728", "1E+39", "1234567890123456789012345678901234567890"]:
            with self.assertRaises(ValueError):
                encode(Decimal(bad))
        self.assertEqual(back(Decimal("1E+38")), Decimal(10**38))
        self.assertEqual(back(Decimal("0E+100000000")), Decimal(0))
        # A large exponent is refused before its power of ten is worked out.
        started = time.monotonic()
        for bad in ["1E+10000000", "1E+999999999999999999"]:
            with self.assertRaises(ValueError):
                encode(Decimal(bad))
        self.assertLess(time.monotonic() - started, 1.0)

    def test_timestamps_are_microseconds_in_utc(self) -> None:
        tokyo = dt.timezone(dt.timedelta(hours=9))
        moment = dt.datetime(2026, 3, 5, 19, 20, 30, 123456, tzinfo=tokyo)
        self.assertEqual(Timestamp.from_datetime(moment).micros, 1_772_706_030_123_456)
        read = back(moment)
        self.assertEqual(read, moment)
        self.assertEqual((read.tzinfo, read.hour), (dt.timezone.utc, 10))
        # Without a zone: UTC, not the machine's local time.
        self.assertEqual(Timestamp.from_datetime(dt.datetime(1970, 1, 1, 0, 0, 1)).micros, 1_000_000)
        self.assertEqual(back(dt.datetime(1969, 12, 31, 23, 59, 59, 999999)), dt.datetime(1969, 12, 31, 23, 59, 59, 999999, tzinfo=dt.timezone.utc))
        # Exact at the edges of datetime, and beyond them as Timestamp.
        self.assertEqual(back(dt.datetime.max.replace(tzinfo=dt.timezone.utc)), dt.datetime.max.replace(tzinfo=dt.timezone.utc))
        self.assertEqual(back(dt.datetime.min.replace(tzinfo=dt.timezone.utc)), dt.datetime.min.replace(tzinfo=dt.timezone.utc))
        self.assertEqual(back(Timestamp(2**63 - 1)), Timestamp(2**63 - 1))
        self.assertEqual(back(Timestamp(0)), dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc))
        self.assertEqual(back(Timestamp(0), exact=True), Timestamp(0))

    def test_dates_are_days(self) -> None:
        self.assertEqual(Date.from_date(dt.date(2026, 3, 5)).days, 20_517)
        self.assertEqual(back(dt.date(1969, 12, 31)), dt.date(1969, 12, 31))
        self.assertEqual(back(dt.date.min), dt.date.min)
        self.assertEqual(back(dt.date.max), dt.date.max)
        self.assertEqual(back(Date(2**31 - 1)), Date(2**31 - 1))
        self.assertEqual(back(Date(0), exact=True), Date(0))
        with self.assertRaises(ValueError):
            encode(Date(2**31))

    def test_json_is_written_compactly_and_read_parsed(self) -> None:
        self.assertEqual(encode({"a": [1, 2.5, None, {"b": "c"}], "é": 'ü\n"\\'})[2:].decode(), '{"a":[1,2.5,null,{"b":"c"}],"é":"ü\\n\\"\\\\"}')
        self.assertEqual(back(Json([1, "two"])), [1, "two"])
        self.assertEqual(back(Json(None)), None)
        self.assertEqual(back(Json("text"), exact=True), Json("text"))
        with self.assertRaises(ValueError):
            encode({"x": float("nan")})
        # The server keeps JSON integers within 64 bits, signed or not, and
        # reads a larger one as a float: one is refused, not changed.
        self.assertEqual(back({"a": [2**64 - 1, -(2**63)], "b": True}), {"a": [2**64 - 1, -(2**63)], "b": True})
        for bad in [{"big": 2**64}, {"a": [1, {"b": -(2**63) - 1}]}, Json([2**70]), Json(2**70)]:
            with self.assertRaises(ValueError):
                encode(bad)
        self.assertEqual(back({"big": float(2**70)}), {"big": float(2**70)})
        looped: Any = []
        looped.append(looped)
        with self.assertRaises(ValueError):
            encode(Json(looped))
        # Half a surrogate pair has no UTF-8 form.
        for text in ["a\ud800b", Json("\udc00"), {"\ud800": 1}]:
            with self.assertRaises(ValueError):
                encode(text)

    def test_what_has_no_kind_is_refused(self) -> None:
        for bad in [object(), {1, 2}, 1 + 2j, dt.time(1, 2), dt.timedelta(1)]:
            with self.assertRaises(TypeError):
                encode(bad)


class FakeConn:
    """Stands in for a connection where only the pool's bookkeeping is tested."""

    def __init__(self, config: Config, context: object = None) -> None:
        self.broken = False
        self.closed = False

    def close(self) -> None:
        self.closed = True


class PoolWithoutServer(unittest.TestCase):
    def test_a_forgotten_lease_collected_inside_the_pool_does_not_deadlock(self) -> None:
        with mock.patch.object(connection, "Conn", FakeConn):
            pool = connection.Pool(Config("127.0.0.1", pool_size=1))
            lease = pool.acquire()
            conn = lease.conn
            lease.held = True
            # Forgotten in a cycle: only the cycle collector finalizes it,
            # whenever an allocation sets it off, on whatever thread.
            lease.cycle = lease  # type: ignore[attr-defined]
            del lease

            def collect_inside_the_pool() -> None:
                with pool._lock:
                    gc.collect()

            worker = threading.Thread(target=collect_inside_the_pool, daemon=True)
            worker.start()
            worker.join(5)
            self.assertFalse(worker.is_alive(), "the collector deadlocked on the pool's lock")
            # The connection held a transaction: closed, and its slot is free.
            self.assertTrue(conn.closed)  # type: ignore[union-attr]
            again = pool.acquire()
            self.assertIsNot(again.conn, conn)
            again.release()


class Urls(unittest.TestCase):
    def test_reads_a_url(self) -> None:
        c = Config.from_url("database://shop:s3cr%40t@db:6000?pool=3&statement_timeout=1.5&name=shop-1")
        self.assertEqual((c.host, c.port, c.database, c.password), ("db", 6000, "shop", "s3cr@t"))
        self.assertEqual((c.pool_size, c.statement_timeout, c.name), (3, 1.5, "shop-1"))
        c = Config.from_url("database://anything:pw@localhost/blog")
        self.assertEqual((c.port, c.database), (5454, "blog"))
        admin = Config.from_url("database://adminpw@localhost")
        self.assertEqual((admin.database, admin.password), ("", "adminpw"))
        c = Config.from_url("database://d:a%3Ab%40c@h?connect_timeout=2&lock_wait=0.25")
        self.assertEqual((c.password, c.connect_timeout, c.lock_wait), ("a:b@c", 2.0, 0.25))
        self.assertIsNone(c.tls)
        self.assertIsNone(Config.from_url("database://shop:pw@db?tls=off").tls)
        self.assertEqual(Config.from_url("database://shop:pw@db?tls=on").tls, Tls())
        authority = Config.from_url("database://shop:pw@10.0.0.5?tls_ca=%2Fetc%2Fdatabase%2Fca.pem&tls_name=db.example.com")
        self.assertEqual(authority.tls, Tls(ca="/etc/database/ca.pem", name="db.example.com"))
        self.assertEqual(Config.from_url("database://shop:pw@db?tls_name=db.example.com&tls=on").tls, Tls(name="db.example.com"))
        for bad in [
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
            "database://a:b@h/d?tls=yes",
            "database://a:b@h/d?tls=",
            "database://a:b@h/d?tls=off&tls_ca=ca.pem",
            "database://a:b@h/d?tls_name=db&tls=off",
            "database://a:b@h/d?tls_ca=",
            "database://a:b@h/d?tls_name=",
            "database://a:b@h/d?tls_cert=server.pem",
        ]:
            with self.assertRaises(DbError, msg=bad) as caught:
                Config.from_url(bad)
            self.assertEqual(caught.exception.code, ErrorCode.INVALID_REQUEST)


class Results(unittest.TestCase):
    def test_rows_and_their_columns(self) -> None:
        rows = Rows(ast.RowSet(["id", "name"], [[1, "ann"], [2, "bob"]], 2, 10))
        self.assertEqual((len(rows), rows.affected, rows.total, rows.columns), (2, 2, 10, ("id", "name")))
        first = rows[0]
        self.assertEqual((first["id"], first[1], first.get("name"), first.get("nothing", 0)), (1, "ann", "ann", 0))
        self.assertEqual(first.as_dict(), {"id": 1, "name": "ann"})
        self.assertEqual(list(first), [1, "ann"])
        identifier, name = first
        self.assertEqual((identifier, name), (1, "ann"))
        with self.assertRaises(KeyError):
            first["nothing"]
        self.assertEqual(rows.column(), [1, 2])
        self.assertEqual(rows.column("name"), ["ann", "bob"])
        self.assertEqual(rows.dicts()[1], {"id": 2, "name": "bob"})
        self.assertEqual(rows.first(), first)
        self.assertEqual(rows.opt(), first)
        with self.assertRaises(DbError) as several:
            rows.one()
        self.assertEqual(several.exception.code, ErrorCode.INVALID_REQUEST)
        empty = Rows(ast.RowSet())
        self.assertIsNone(empty.opt())
        with self.assertRaises(DbError) as none:
            empty.one()
        self.assertEqual(none.exception.code, ErrorCode.NOT_FOUND)

    def test_an_error_code_this_version_does_not_know_is_internal(self) -> None:
        self.assertEqual(ErrorCode.from_wire(200), ErrorCode.INTERNAL)
        self.assertEqual(ErrorCode.from_wire(0), ErrorCode.INTERNAL)
        self.assertEqual(ErrorCode.from_wire(22), ErrorCode.CONFLICT)


class Syntax(unittest.TestCase):
    def test_the_package_is_written_in_the_syntax_of_python_3_9(self) -> None:
        """What pyproject.toml promises. This checks the grammar only: the
        tests themselves run on whatever Python runs them."""
        package = Path(__file__).resolve().parents[1] / "database_client"
        sources = sorted(package.glob("*.py"))
        self.assertGreater(len(sources), 5)
        for source in sources:
            python_ast.parse(source.read_text(encoding="utf-8"), filename=str(source), feature_version=(3, 9))


if __name__ == "__main__":
    unittest.main()
