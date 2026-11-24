"""How values, expressions, queries and definitions are written as bytes.

This is ``docs/PROTOCOL.md`` in code, and the counterpart of the server's
``protocol/src/codec.rs``: integers are little-endian or varints, strings
and lists carry their length first, and every choice between kinds is one
tag byte. ``protocol/fixtures/vectors.json`` holds messages and the bytes
they must come out as; ``tests/test_vectors.py`` checks this file against
it.
"""

from __future__ import annotations

import datetime as _dt
import decimal as _decimal
import json as _json
import struct
import uuid as _uuid
from typing import Any, Callable, List, Optional, Sequence, Tuple, TypeVar

from . import ast
from .errors import DbError, ErrorCode
from .values import Date, Json, MAX_SCALE, Timestamp, check_int, decimal_from_parts, decimal_parts

T = TypeVar("T")

#: How deep expressions, queries, lists and list types may nest.
MAX_DEPTH = 100

_U64_MAX = (1 << 64) - 1
_I128_MIN = -(1 << 127)


class ProtocolError(DbError):
    """The server sent something this client cannot read."""

    def __init__(self, what: str) -> None:
        super().__init__(ErrorCode.CONNECTION, f"malformed message: {what}")


# ----------------------------------------------------------------- writer


class Writer:
    def __init__(self) -> None:
        self.buf = bytearray()

    def u8(self, v: int) -> None:
        self.buf.append(v)

    def flag(self, v: bool) -> None:
        self.buf.append(1 if v else 0)

    def u16(self, v: int) -> None:
        self.buf += struct.pack("<H", v)

    def u32(self, v: int) -> None:
        self.buf += struct.pack("<I", v)

    def u64(self, v: int) -> None:
        self.buf += struct.pack("<Q", v)

    def varint(self, v: int) -> None:
        if not 0 <= v <= _U64_MAX:
            raise ValueError(f"{v} is not an unsigned 64-bit number")
        while v >= 0x80:
            self.buf.append((v & 0x7F) | 0x80)
            v >>= 7
        self.buf.append(v)

    def zigzag(self, v: int) -> None:
        """A signed integer, small in either direction staying short."""
        self.varint(((v << 1) ^ (v >> 63)) & _U64_MAX)

    def blob(self, v: bytes) -> None:
        self.varint(len(v))
        self.buf += v

    def text(self, v: str) -> None:
        self.blob(v.encode("utf-8"))

    def opt_text(self, v: Optional[str]) -> None:
        if v is None:
            self.u8(0)
        else:
            self.u8(1)
            self.text(v)

    def texts(self, v: Sequence[str]) -> None:
        self.varint(len(v))
        for s in v:
            self.text(s)


# ----------------------------------------------------------------- reader


class Reader:
    def __init__(self, buf: bytes, exact: bool = False) -> None:
        self.buf = buf
        self.pos = 0
        self.depth = 0
        #: Read JSON, timestamps and dates as ``Json``, ``Timestamp`` and
        #: ``Date``, which lose nothing, instead of the plain Python values.
        self.exact = exact

    def remaining(self) -> int:
        return len(self.buf) - self.pos

    def take(self, n: int) -> bytes:
        if self.remaining() < n:
            raise ProtocolError("ends early")
        out = self.buf[self.pos : self.pos + n]
        self.pos += n
        return out

    def u8(self) -> int:
        return self.take(1)[0]

    def flag(self) -> bool:
        v = self.u8()
        if v > 1:
            raise ProtocolError("flag is neither 0 nor 1")
        return v == 1

    def u16(self) -> int:
        return int(struct.unpack("<H", self.take(2))[0])

    def u32(self) -> int:
        return int(struct.unpack("<I", self.take(4))[0])

    def u64(self) -> int:
        return int(struct.unpack("<Q", self.take(8))[0])

    def varint(self) -> int:
        v = 0
        shift = 0
        while True:
            byte = self.u8()
            if shift == 63 and byte > 1:
                raise ProtocolError("number too large")
            v |= (byte & 0x7F) << shift
            if byte & 0x80 == 0:
                return v
            shift += 7
            if shift > 63:
                raise ProtocolError("number too long")

    def zigzag(self) -> int:
        v = self.varint()
        return (v >> 1) ^ -(v & 1)

    def count(self) -> int:
        """A count of items that each take at least one byte, so a count
        larger than what is left is a lie."""
        n = self.varint()
        if n > self.remaining():
            raise ProtocolError("count exceeds the message")
        return n

    def blob(self) -> bytes:
        return bytes(self.take(self.count()))

    def text(self) -> str:
        try:
            return self.blob().decode("utf-8")
        except UnicodeDecodeError:
            raise ProtocolError("text is not UTF-8") from None

    def opt_text(self) -> Optional[str]:
        return self.text() if self.flag() else None

    def texts(self) -> Tuple[str, ...]:
        return tuple(self.text() for _ in range(self.count()))

    def enter(self) -> None:
        self.depth += 1
        if self.depth > MAX_DEPTH:
            raise ProtocolError("nested too deeply")

    def leave(self) -> None:
        self.depth -= 1

    def finish(self) -> None:
        if self.remaining():
            raise ProtocolError("bytes left over")


# ----------------------------------------------------------------- values

TAG_NULL, TAG_FALSE, TAG_TRUE, TAG_INT, TAG_FLOAT, TAG_DECIMAL, TAG_TEXT = 0, 1, 2, 3, 4, 5, 6
TAG_BYTES, TAG_UUID, TAG_TIMESTAMP, TAG_DATE, TAG_JSON, TAG_LIST = 7, 8, 9, 10, 11, 12


def json_text(value: Any) -> str:
    """JSON as this client writes it: compact, and not escaping what need
    not be. The server reads any JSON text."""
    return _json.dumps(value, separators=(",", ":"), ensure_ascii=False, allow_nan=False)


def put_value(w: Writer, v: Any) -> None:
    # bool before int, datetime before date: each is a kind of the other.
    if v is None:
        w.u8(TAG_NULL)
    elif v is True:
        w.u8(TAG_TRUE)
    elif v is False:
        w.u8(TAG_FALSE)
    elif isinstance(v, int):
        w.u8(TAG_INT)
        w.zigzag(check_int(v))
    elif isinstance(v, float):
        w.u8(TAG_FLOAT)
        w.buf += struct.pack("<d", v)
    elif isinstance(v, _decimal.Decimal):
        mantissa, scale = decimal_parts(v)
        w.u8(TAG_DECIMAL)
        w.u8(scale)
        w.buf += mantissa.to_bytes(16, "little", signed=True)
    elif isinstance(v, str):
        w.u8(TAG_TEXT)
        w.text(v)
    elif isinstance(v, (bytes, bytearray, memoryview)):
        w.u8(TAG_BYTES)
        w.blob(bytes(v))
    elif isinstance(v, _uuid.UUID):
        w.u8(TAG_UUID)
        w.buf += v.bytes
    elif isinstance(v, (Timestamp, _dt.datetime)):
        micros = v.micros if isinstance(v, Timestamp) else Timestamp.from_datetime(v).micros
        w.u8(TAG_TIMESTAMP)
        w.zigzag(check_int(micros))
    elif isinstance(v, (Date, _dt.date)):
        days = v.days if isinstance(v, Date) else Date.from_date(v).days
        if not -(1 << 31) <= days < (1 << 31):
            raise ValueError(f"day {days} is beyond what a date can hold")
        w.u8(TAG_DATE)
        w.zigzag(days)
    elif isinstance(v, (Json, dict)):
        w.u8(TAG_JSON)
        w.text(json_text(v.value if isinstance(v, Json) else v))
    elif isinstance(v, (list, tuple)):
        w.u8(TAG_LIST)
        w.varint(len(v))
        for item in v:
            put_value(w, item)
    else:
        raise TypeError(f"the database has no value for a {type(v).__name__}: {v!r}")


def get_value(r: Reader) -> Any:
    tag = r.u8()
    if tag == TAG_NULL:
        return None
    if tag == TAG_FALSE:
        return False
    if tag == TAG_TRUE:
        return True
    if tag == TAG_INT:
        return r.zigzag()
    if tag == TAG_FLOAT:
        return struct.unpack("<d", r.take(8))[0]
    if tag == TAG_DECIMAL:
        scale = r.u8()
        mantissa = int.from_bytes(r.take(16), "little", signed=True)
        if scale > MAX_SCALE or mantissa == _I128_MIN:
            raise ProtocolError("decimal out of range")
        return decimal_from_parts(mantissa, scale)
    if tag == TAG_TEXT:
        return r.text()
    if tag == TAG_BYTES:
        return r.blob()
    if tag == TAG_UUID:
        return _uuid.UUID(bytes=bytes(r.take(16)))
    if tag == TAG_TIMESTAMP:
        stamp = Timestamp(r.zigzag())
        if r.exact:
            return stamp
        try:
            return stamp.to_datetime()
        except OverflowError:
            return stamp
    if tag == TAG_DATE:
        days = r.zigzag()
        if not -(1 << 31) <= days < (1 << 31):
            raise ProtocolError("date out of range")
        day = Date(days)
        if r.exact:
            return day
        try:
            return day.to_date()
        except (OverflowError, ValueError):
            return day
    if tag == TAG_JSON:
        text = r.text()
        try:
            parsed = _json.loads(text)
        except (ValueError, RecursionError):
            raise ProtocolError("invalid JSON") from None
        return Json(parsed) if r.exact else parsed
    if tag == TAG_LIST:
        r.enter()
        items = [get_value(r) for _ in range(r.count())]
        r.leave()
        return items
    raise ProtocolError("unknown value tag")


def put_type(w: Writer, t: ast.Type) -> None:
    w.u8(t.tag)
    if t.tag == 11:
        assert t.of is not None
        put_type(w, t.of)
    elif t.tag == 12:
        assert t.precision is not None and t.scale is not None
        w.u8(t.precision)
        w.u8(t.scale)
    elif not 1 <= t.tag <= 10:
        raise ValueError(f"no type has the tag {t.tag}")


def get_type(r: Reader) -> ast.Type:
    tag = r.u8()
    if 1 <= tag <= 10:
        return ast.Type(tag)
    if tag == 11:
        r.enter()
        of = get_type(r)
        r.leave()
        return ast.Type.list(of)
    if tag == 12:
        precision, scale = r.u8(), r.u8()
        if scale > precision or scale > MAX_SCALE:
            raise ProtocolError("decimal precision and scale do not fit")
        return ast.Type.decimal(precision, scale)
    raise ProtocolError("unknown type tag")


# ------------------------------------------------------------ expressions


def _put_list(w: Writer, items: Sequence[T], put: Callable[[Writer, T], None]) -> None:
    w.varint(len(items))
    for item in items:
        put(w, item)


def _get_list(r: Reader, get: Callable[[Reader], T]) -> Tuple[T, ...]:
    return tuple(get(r) for _ in range(r.count()))


def put_exprs(w: Writer, exprs: Sequence[ast.ExprNode]) -> None:
    _put_list(w, exprs, put_expr)


def get_exprs(r: Reader) -> Tuple[ast.ExprNode, ...]:
    return _get_list(r, get_expr)


def put_opt_expr(w: Writer, e: Optional[ast.ExprNode]) -> None:
    if e is None:
        w.u8(0)
    else:
        w.u8(1)
        put_expr(w, e)


def get_opt_expr(r: Reader) -> Optional[ast.ExprNode]:
    return get_expr(r) if r.flag() else None


def put_expr(w: Writer, e: ast.ExprNode) -> None:
    if isinstance(e, ast.Lit):
        w.u8(1)
        put_value(w, e.value)
    elif isinstance(e, ast.Column):
        w.u8(2)
        w.opt_text(e.table)
        w.text(e.name)
    elif isinstance(e, ast.Excluded):
        w.u8(3)
        w.text(e.name)
    elif isinstance(e, ast.Old):
        w.u8(4)
        w.text(e.name)
    elif isinstance(e, ast.Depth):
        w.u8(5)
    elif isinstance(e, ast.Rank):
        w.u8(6)
    elif isinstance(e, ast.Not):
        w.u8(7)
        put_expr(w, e.expr)
    elif isinstance(e, ast.And):
        w.u8(8)
        put_exprs(w, e.parts)
    elif isinstance(e, ast.Or):
        w.u8(9)
        put_exprs(w, e.parts)
    elif isinstance(e, ast.Cmp):
        w.u8(10)
        w.u8(e.op)
        put_expr(w, e.left)
        put_expr(w, e.right)
    elif isinstance(e, ast.IsNull):
        w.u8(11)
        w.flag(e.negated)
        put_expr(w, e.expr)
    elif isinstance(e, ast.In):
        w.u8(12)
        w.flag(e.negated)
        put_expr(w, e.expr)
        put_expr(w, e.list)
    elif isinstance(e, ast.Like):
        w.u8(13)
        w.flag(e.case_insensitive)
        w.flag(e.negated)
        put_expr(w, e.expr)
        put_expr(w, e.pattern)
    elif isinstance(e, ast.Arith):
        w.u8(14)
        w.u8(e.op)
        put_expr(w, e.left)
        put_expr(w, e.right)
    elif isinstance(e, ast.Neg):
        w.u8(15)
        put_expr(w, e.expr)
    elif isinstance(e, ast.Call):
        w.u8(16)
        w.u16(e.func)
        put_exprs(w, e.args)
    elif isinstance(e, ast.Case):
        w.u8(17)
        w.varint(len(e.branches))
        for condition, value in e.branches:
            put_expr(w, condition)
            put_expr(w, value)
        put_opt_expr(w, e.otherwise)
    elif isinstance(e, ast.Cast):
        w.u8(18)
        w.flag(e.lenient)
        put_type(w, e.to)
        put_expr(w, e.expr)
    elif isinstance(e, ast.ListOf):
        w.u8(19)
        put_exprs(w, e.items)
    elif isinstance(e, ast.Agg):
        w.u8(20)
        w.u8(e.func)
        w.flag(e.distinct)
        put_opt_expr(w, e.arg)
        put_opt_expr(w, e.filter)
    elif isinstance(e, ast.Subquery):
        w.u8(21)
        w.u8(e.kind)
        put_query(w, e.query)
    else:
        raise TypeError(f"not an expression: {e!r}")


def _enum(kind: Any, number: int, what: str) -> Any:
    try:
        return kind(number)
    except ValueError:
        raise ProtocolError(f"unknown {what}") from None


def get_expr(r: Reader) -> ast.ExprNode:
    r.enter()
    tag = r.u8()
    e: ast.ExprNode
    if tag == 1:
        e = ast.Lit(get_value(r))
    elif tag == 2:
        e = ast.Column(r.opt_text(), r.text())
    elif tag == 3:
        e = ast.Excluded(r.text())
    elif tag == 4:
        e = ast.Old(r.text())
    elif tag == 5:
        e = ast.Depth()
    elif tag == 6:
        e = ast.Rank()
    elif tag == 7:
        e = ast.Not(get_expr(r))
    elif tag == 8:
        e = ast.And(get_exprs(r))
    elif tag == 9:
        e = ast.Or(get_exprs(r))
    elif tag == 10:
        op = _enum(ast.CmpOp, r.u8(), "comparison")
        e = ast.Cmp(op, get_expr(r), get_expr(r))
    elif tag == 11:
        negated = r.flag()
        e = ast.IsNull(get_expr(r), negated)
    elif tag == 12:
        negated = r.flag()
        e = ast.In(get_expr(r), get_expr(r), negated)
    elif tag == 13:
        case_insensitive, negated = r.flag(), r.flag()
        e = ast.Like(get_expr(r), get_expr(r), case_insensitive, negated)
    elif tag == 14:
        op = _enum(ast.ArithOp, r.u8(), "operator")
        e = ast.Arith(op, get_expr(r), get_expr(r))
    elif tag == 15:
        e = ast.Neg(get_expr(r))
    elif tag == 16:
        func = _enum(ast.Func, r.u16(), "function")
        e = ast.Call(func, get_exprs(r))
    elif tag == 17:
        branches = tuple((get_expr(r), get_expr(r)) for _ in range(r.count()))
        e = ast.Case(branches, get_opt_expr(r))
    elif tag == 18:
        lenient = r.flag()
        to = get_type(r)
        e = ast.Cast(get_expr(r), to, lenient)
    elif tag == 19:
        e = ast.ListOf(get_exprs(r))
    elif tag == 20:
        func = _enum(ast.AggFunc, r.u8(), "aggregate")
        distinct = r.flag()
        e = ast.Agg(func, get_opt_expr(r), get_opt_expr(r), distinct)
    elif tag == 21:
        kind = _enum(ast.SubqueryKind, r.u8(), "subquery kind")
        e = ast.Subquery(get_query(r), kind)
    else:
        raise ProtocolError("unknown expression tag")
    r.leave()
    return e


# ---------------------------------------------------------------- queries


def put_select(w: Writer, item: ast.Select) -> None:
    w.text(item.name)
    put_expr(w, item.expr)


def get_select(r: Reader) -> ast.Select:
    return ast.Select(r.text(), get_expr(r))


def put_selects(w: Writer, items: Sequence[ast.Select]) -> None:
    _put_list(w, items, put_select)


def get_selects(r: Reader) -> Tuple[ast.Select, ...]:
    return _get_list(r, get_select)


def put_bound(w: Writer, b: ast.BoundSpec) -> None:
    if b.kind not in (0, 1, 2):
        raise ValueError(f"no bound has the tag {b.kind}")
    w.u8(b.kind)
    if b.kind != 0:
        put_exprs(w, b.values)


def get_bound(r: Reader) -> ast.BoundSpec:
    kind = r.u8()
    if kind == 0:
        return ast.BoundSpec(0)
    if kind in (1, 2):
        return ast.BoundSpec(kind, get_exprs(r))
    raise ProtocolError("unknown bound")


def put_range(w: Writer, k: ast.KeyRangeSpec) -> None:
    put_bound(w, k.lower)
    put_bound(w, k.upper)
    w.flag(k.nulls)


def get_range(r: Reader) -> ast.KeyRangeSpec:
    return ast.KeyRangeSpec(get_bound(r), get_bound(r), r.flag())


def put_access(w: Writer, a: ast.Access) -> None:
    if isinstance(a, ast.Primary):
        w.u8(1)
        w.flag(a.reverse)
        _put_list(w, a.ranges, put_range)
    elif isinstance(a, ast.Index):
        w.u8(2)
        w.text(a.index)
        w.flag(a.reverse)
        _put_list(w, a.ranges, put_range)
    elif isinstance(a, ast.Walk):
        w.u8(3)
        _put_list(w, a.start, put_exprs)
        w.text(a.parent)
        w.u8(a.direction)
        w.opt_text(a.index)
        put_opt_expr(w, a.prune)
        put_opt_expr(w, a.stop)
        w.u32(a.max_depth)
    elif isinstance(a, ast.Search):
        w.u8(4)
        w.text(a.index)
        put_exprs(w, a.prefix)
        put_expr(w, a.query)
        w.flag(a.match_prefix)
    else:
        raise TypeError(f"not a way into a table: {a!r}")


def get_access(r: Reader) -> ast.Access:
    tag = r.u8()
    if tag == 1:
        reverse = r.flag()
        return ast.Primary(_get_list(r, get_range), reverse)
    if tag == 2:
        index = r.text()
        reverse = r.flag()
        return ast.Index(index, _get_list(r, get_range), reverse)
    if tag == 3:
        start = _get_list(r, get_exprs)
        parent = r.text()
        direction = _enum(ast.Direction, r.u8(), "direction")
        return ast.Walk(start, parent, direction, r.opt_text(), get_opt_expr(r), get_opt_expr(r), r.u32())
    if tag == 4:
        return ast.Search(r.text(), get_exprs(r), get_expr(r), r.flag())
    raise ProtocolError("unknown access")


def _put_join(w: Writer, join: ast.JoinSpec) -> None:
    w.u8(join.kind)
    put_query(w, join.query)


def _get_join(r: Reader) -> ast.JoinSpec:
    kind = _enum(ast.JoinKind, r.u8(), "join kind")
    return ast.JoinSpec(kind, get_query(r))


def _put_order(w: Writer, order: ast.OrderSpec) -> None:
    put_expr(w, order.expr)
    w.flag(order.desc)
    w.u8(0 if order.nulls_last is None else 2 if order.nulls_last else 1)


def _get_order(r: Reader) -> ast.OrderSpec:
    expr = get_expr(r)
    desc = r.flag()
    nulls = r.u8()
    if nulls > 2:
        raise ProtocolError("unknown null order")
    return ast.OrderSpec(expr, desc, (None, False, True)[nulls])


def put_query(w: Writer, q: ast.QuerySpec) -> None:
    w.text(q.table)
    w.opt_text(q.alias)
    put_access(w, q.access)
    _put_list(w, q.joins, _put_join)
    if q.unnest is None:
        w.u8(0)
    else:
        w.u8(1)
        put_select(w, q.unnest)
    put_opt_expr(w, q.filter)
    put_exprs(w, q.group_by)
    put_opt_expr(w, q.having)
    _put_list(w, q.order_by, _put_order)
    w.varint(q.offset)
    if q.limit is None:
        w.u8(0)
    else:
        w.u8(1)
        w.varint(q.limit)
    put_selects(w, q.select)
    w.flag(q.distinct)
    w.flag(q.with_total)


def get_query(r: Reader) -> ast.QuerySpec:
    r.enter()
    table = r.text()
    alias = r.opt_text()
    access = get_access(r)
    joins = _get_list(r, _get_join)
    unnest = get_select(r) if r.flag() else None
    where = get_opt_expr(r)
    group_by = get_exprs(r)
    having = get_opt_expr(r)
    order_by = _get_list(r, _get_order)
    offset = r.varint()
    limit = r.varint() if r.flag() else None
    select = get_selects(r)
    distinct = r.flag()
    with_total = r.flag()
    r.leave()
    return ast.QuerySpec(table, alias, access, joins, unnest, where, group_by, having, order_by, offset, limit, select, distinct, with_total)


def _put_assign(w: Writer, a: ast.Assign) -> None:
    w.text(a.column)
    put_expr(w, a.value)


def _get_assign(r: Reader) -> ast.Assign:
    return ast.Assign(r.text(), get_expr(r))


def put_insert(w: Writer, i: ast.InsertSpec) -> None:
    w.text(i.table)
    w.texts(i.columns)
    if i.query is None:
        w.u8(0)
        _put_list(w, i.rows, put_exprs)
    else:
        w.u8(1)
        put_query(w, i.query)
    c = i.on_conflict
    if c.kind == 0:
        w.u8(0)
    elif c.kind == 1:
        w.u8(1)
        w.opt_text(c.target)
    elif c.kind == 2:
        if c.target is None:
            raise ValueError("an upsert must name the unique index it updates on")
        w.u8(2)
        w.text(c.target)
        _put_list(w, c.set, _put_assign)
        put_opt_expr(w, c.filter)
    else:
        raise ValueError(f"no conflict action has the tag {c.kind}")
    put_selects(w, i.returning)


def get_insert(r: Reader) -> ast.InsertSpec:
    table = r.text()
    columns = r.texts()
    rows: Tuple[Tuple[ast.ExprNode, ...], ...] = ()
    query = None
    source = r.u8()
    if source == 0:
        rows = _get_list(r, get_exprs)
    elif source == 1:
        query = get_query(r)
    else:
        raise ProtocolError("unknown insert source")
    action = r.u8()
    if action == 0:
        on_conflict = ast.OnConflictSpec()
    elif action == 1:
        on_conflict = ast.OnConflictSpec(1, r.opt_text())
    elif action == 2:
        on_conflict = ast.OnConflictSpec(2, r.text(), _get_list(r, _get_assign), get_opt_expr(r))
    else:
        raise ProtocolError("unknown conflict action")
    return ast.InsertSpec(table, columns, rows, query, on_conflict, get_selects(r))


def put_update(w: Writer, u: ast.UpdateSpec) -> None:
    put_query(w, u.rows)
    _put_list(w, u.set, _put_assign)
    put_selects(w, u.returning)


def get_update(r: Reader) -> ast.UpdateSpec:
    return ast.UpdateSpec(get_query(r), _get_list(r, _get_assign), get_selects(r))


def put_delete(w: Writer, d: ast.DeleteSpec) -> None:
    put_query(w, d.rows)
    put_selects(w, d.returning)


def get_delete(r: Reader) -> ast.DeleteSpec:
    return ast.DeleteSpec(get_query(r), get_selects(r))


# ----------------------------------------------------------------- schema


def put_column(w: Writer, c: ast.ColumnSpec) -> None:
    w.text(c.name)
    put_type(w, c.type)
    w.flag(c.nullable)
    put_opt_expr(w, c.default)
    w.flag(c.auto_increment)


def get_column(r: Reader) -> ast.ColumnSpec:
    return ast.ColumnSpec(r.text(), get_type(r), r.flag(), get_opt_expr(r), r.flag())


def _put_index_key(w: Writer, key: ast.IndexKey) -> None:
    put_expr(w, key.expr)
    w.flag(key.desc)
    w.flag(key.each)


def _put_text_field(w: Writer, f: ast.TextField) -> None:
    put_expr(w, f.expr)
    w.buf += struct.pack("<d", f.weight)


def put_index(w: Writer, i: ast.IndexSpec) -> None:
    w.text(i.name)
    _put_list(w, i.keys, _put_index_key)
    w.flag(i.unique)
    put_opt_expr(w, i.filter)
    if i.text_config is None:
        w.u8(0)
    else:
        w.u8(1)
        w.u8(i.text_config)
        _put_list(w, i.text_fields, _put_text_field)


def get_index(r: Reader) -> ast.IndexSpec:
    name = r.text()
    keys = _get_list(r, lambda r: ast.IndexKey(get_expr(r), r.flag(), r.flag()))
    unique = r.flag()
    where = get_opt_expr(r)
    kind = r.u8()
    if kind == 0:
        return ast.IndexSpec(name, keys, unique, where)
    if kind == 1:
        config = _enum(ast.TextConfig, r.u8(), "text config")
        fields = _get_list(r, lambda r: ast.TextField(get_expr(r), struct.unpack("<d", r.take(8))[0]))
        return ast.IndexSpec(name, keys, unique, where, config, fields)
    raise ProtocolError("unknown index kind")


def _put_foreign_key(w: Writer, fk: ast.ForeignKey) -> None:
    w.text(fk.name)
    w.texts(fk.columns)
    w.text(fk.ref_table)
    w.texts(fk.ref_columns)
    w.u8(fk.on_delete)


def _get_foreign_key(r: Reader) -> ast.ForeignKey:
    return ast.ForeignKey(r.text(), r.texts(), r.text(), r.texts(), _enum(ast.OnDelete, r.u8(), "delete action"))


def _put_check(w: Writer, c: ast.Check) -> None:
    w.text(c.name)
    put_expr(w, c.expr)


def put_table(w: Writer, t: ast.TableSpec) -> None:
    w.text(t.name)
    _put_list(w, t.columns, put_column)
    w.texts(t.primary_key)
    _put_list(w, t.indexes, put_index)
    _put_list(w, t.foreign_keys, _put_foreign_key)
    _put_list(w, t.checks, _put_check)
    w.flag(t.volatile)


def get_table(r: Reader) -> ast.TableSpec:
    return ast.TableSpec(
        r.text(),
        _get_list(r, get_column),
        r.texts(),
        _get_list(r, get_index),
        _get_list(r, _get_foreign_key),
        _get_list(r, lambda r: ast.Check(r.text(), get_expr(r))),
        r.flag(),
    )


# ----------------------------------------------------------------- errors


def get_error(r: Reader) -> DbError:
    return DbError(ErrorCode.from_wire(r.u8()), r.text(), r.opt_text(), r.opt_text())


# --------------------------------------------------------------- messages

#: The version of the protocol this client speaks.
PROTOCOL_VERSION = 2
#: The largest frame either side accepts.
MAX_FRAME = 256 * 1024 * 1024

HELLO, PING, BEGIN, COMMIT, ROLLBACK, SAVEPOINT, ROLLBACK_TO, RELEASE, QUERY = 1, 2, 3, 4, 5, 6, 7, 8, 9
INSERT, UPDATE, DELETE, DDL, DESCRIBE, LOCK, UNLOCK, NOW, ADMIN = 10, 11, 12, 13, 14, 15, 16, 17, 18

CREATE_TABLE, DROP_TABLE, ADD_COLUMN, DROP_COLUMN, CREATE_INDEX, DROP_INDEX = 1, 2, 3, 4, 5, 6

CREATE_DATABASE, DROP_DATABASE, SET_PASSWORD, LIST_DATABASES, CHECKPOINT, CHECK, STATS = 1, 2, 3, 4, 5, 6, 7
DUMP, RESTORE_BEGIN, RESTORE_CHUNK, RESTORE_END = 8, 9, 10, 11

R_OK, R_HELLO, R_ROWS, R_TABLES, R_FLAG, R_TIME, R_NAMES, R_TEXT, R_CHUNK, R_ERROR = 1, 2, 3, 4, 5, 6, 7, 8, 9, 10


def request(kind: int, *args: Any) -> ast.Request:
    return ast.Request(kind, args)


def _put_ddl(w: Writer, sub: int, args: Sequence[Any]) -> None:
    w.u8(sub)
    if sub == CREATE_TABLE:
        table, if_not_exists = args
        put_table(w, table)
        w.flag(if_not_exists)
    elif sub == DROP_TABLE:
        name, if_exists = args
        w.text(name)
        w.flag(if_exists)
    elif sub == ADD_COLUMN:
        table_name, column, if_not_exists = args
        w.text(table_name)
        put_column(w, column)
        w.flag(if_not_exists)
    elif sub == DROP_COLUMN:
        table_name, column_name, if_exists = args
        w.text(table_name)
        w.text(column_name)
        w.flag(if_exists)
    elif sub == CREATE_INDEX:
        table_name, index, if_not_exists = args
        w.text(table_name)
        put_index(w, index)
        w.flag(if_not_exists)
    elif sub == DROP_INDEX:
        table_name, index_name, if_exists = args
        w.text(table_name)
        w.text(index_name)
        w.flag(if_exists)
    else:
        raise ValueError(f"no schema change has the tag {sub}")


def _put_admin(w: Writer, sub: int, args: Sequence[Any]) -> None:
    w.u8(sub)
    if sub == CREATE_DATABASE:
        name, password, if_not_exists = args
        w.text(name)
        w.text(password)
        w.flag(if_not_exists)
    elif sub == DROP_DATABASE:
        name, if_exists = args
        w.text(name)
        w.flag(if_exists)
    elif sub == SET_PASSWORD:
        name, password = args
        w.text(name)
        w.text(password)
    elif sub in (LIST_DATABASES, CHECKPOINT, CHECK, STATS, RESTORE_END):
        pass
    elif sub == DUMP:
        (databases,) = args
        w.texts(databases)
    elif sub == RESTORE_BEGIN:
        (replace,) = args
        w.flag(replace)
    elif sub == RESTORE_CHUNK:
        (chunk,) = args
        w.blob(chunk)
    else:
        raise ValueError(f"no admin request has the tag {sub}")


def encode_request(req: ast.Request, timeout_ms: int = 0) -> bytes:
    """A request as the payload of one frame: how long the server may
    spend on it, in milliseconds (0 for no limit beyond the server's own),
    then the request."""
    w = Writer()
    w.varint(timeout_ms)
    kind, args = req.kind, req.args
    w.u8(kind)
    if kind == HELLO:
        version, database, password, client = args
        w.u32(version)
        w.text(database)
        w.text(password)
        w.text(client)
    elif kind in (PING, COMMIT, ROLLBACK, DESCRIBE, NOW):
        pass
    elif kind == BEGIN:
        txn, lock_wait_ms = args
        w.u8(txn)
        w.varint(lock_wait_ms)
    elif kind in (SAVEPOINT, ROLLBACK_TO, RELEASE, UNLOCK):
        (name,) = args
        w.text(name)
    elif kind == QUERY:
        put_query(w, args[0])
    elif kind == INSERT:
        put_insert(w, args[0])
    elif kind == UPDATE:
        put_update(w, args[0])
    elif kind == DELETE:
        put_delete(w, args[0])
    elif kind == DDL:
        _put_ddl(w, args[0], args[1:])
    elif kind == LOCK:
        name, wait_ms = args
        w.text(name)
        w.varint(wait_ms)
    elif kind == ADMIN:
        _put_admin(w, args[0], args[1:])
    else:
        raise ValueError(f"no request has the tag {kind}")
    return bytes(w.buf)


def decode_response(frame: bytes, exact: bool = False) -> ast.Response:
    """Reads the payload of one frame from the server."""
    r = Reader(frame, exact)
    tag = r.u8()
    response: ast.Response
    if tag == R_OK:
        response = ast.Response(R_OK)
    elif tag == R_HELLO:
        response = ast.Response(R_HELLO, (r.u32(), r.text()))
    elif tag == R_ROWS:
        more = r.flag()
        columns = list(r.texts())
        affected = r.varint()
        total = r.varint() if r.flag() else None
        n = r.varint()
        width = len(columns)
        rows: List[List[Any]] = []
        for _ in range(n):
            rows.append([get_value(r) for _ in range(width)])
            # A row of no columns takes no bytes, so its count cannot be
            # checked against the frame.
            if width == 0 and len(rows) > 1_000_000:
                raise ProtocolError("too many empty rows")
        response = ast.Response(R_ROWS, ast.RowSet(columns, rows, affected, total), more)
    elif tag == R_TABLES:
        response = ast.Response(R_TABLES, list(_get_list(r, get_table)))
    elif tag == R_FLAG:
        response = ast.Response(R_FLAG, r.flag())
    elif tag == R_TIME:
        response = ast.Response(R_TIME, r.zigzag())
    elif tag == R_NAMES:
        response = ast.Response(R_NAMES, list(r.texts()))
    elif tag == R_TEXT:
        response = ast.Response(R_TEXT, r.text())
    elif tag == R_CHUNK:
        response = ast.Response(R_CHUNK, r.blob())
    elif tag == R_ERROR:
        response = ast.Response(R_ERROR, get_error(r))
    else:
        raise ProtocolError("unknown response")
    r.finish()
    return response
