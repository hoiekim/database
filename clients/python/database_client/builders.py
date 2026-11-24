"""The vocabulary requests are written in: ``Query.table("mails")``,
``col("date").desc()``, ``Insert.into("users").set("name", "ann")``.

It follows the Rust client's, name for name where Python allows it. Where
a name is taken by the language, an underscore follows it: ``and_``,
``or_``, ``not_``, ``is_``, ``from_``, ``all_``, ``any_``, ``list_``,
``sum_``, ``min_``, ``max_``.

Every builder is immutable: a method returns a new one, so a query kept
in a variable can be narrowed twice without the two getting in each
other's way. ``.spec`` (``.node`` for an expression) is the plain data
the wire carries.
"""

from __future__ import annotations

import math
from dataclasses import replace
from typing import Any, Iterable, Sequence, Tuple, Union

from . import ast
from .ast import AggFunc, ArithOp, CmpOp, Func, OnDelete, SubqueryKind, TextConfig, Type

#: The name of a table's primary key where an index name is asked for: as
#: an upsert's conflict target, or in a unique violation.
PRIMARY = "primary"

#: In a ``returning``: every column of the row, under its own name.
ALL = "*"

_NODES = (
    ast.Lit,
    ast.Column,
    ast.Excluded,
    ast.Old,
    ast.Depth,
    ast.Rank,
    ast.Not,
    ast.And,
    ast.Or,
    ast.Cmp,
    ast.IsNull,
    ast.In,
    ast.Like,
    ast.Arith,
    ast.Neg,
    ast.Call,
    ast.Case,
    ast.Cast,
    ast.ListOf,
    ast.Agg,
    ast.Subquery,
)


def node(value: Any) -> ast.ExprNode:
    """The expression a value stands for: an ``Expr`` as it is, anything
    else as a constant."""
    if isinstance(value, Expr):
        return value.node
    if isinstance(value, _NODES):
        return value
    if isinstance(value, (Query, Order, KeyRange, Insert, Update, Delete)):
        raise TypeError(f"a {type(value).__name__} is not a value; wrap a query in scalar(), exists() or list_of()")
    return ast.Lit(value)


def _nodes(values: Iterable[Any]) -> Tuple[ast.ExprNode, ...]:
    if isinstance(values, (str, bytes)) or not isinstance(values, Iterable):
        raise TypeError(f"expected a list of values, got {values!r}; a key of one column is written [value]")
    return tuple(node(v) for v in values)


class Expr:
    """An expression: what a filter tests, what an update assigns, what a
    query returns. NULL behaves as in SQL."""

    __slots__ = ("node",)

    def __init__(self, node: ast.ExprNode) -> None:
        self.node = node

    def __repr__(self) -> str:
        return f"Expr({self.node!r})"

    def __bool__(self) -> bool:
        raise TypeError("an expression has no truth of its own: combine expressions with .and_(), .or_() and .not_(), not with and, or and not")

    # ---- comparisons

    def _cmp(self, op: CmpOp, other: Any) -> "Expr":
        return Expr(ast.Cmp(op, self.node, node(other)))

    def eq(self, other: Any) -> "Expr":
        return self._cmp(CmpOp.EQ, other)

    def ne(self, other: Any) -> "Expr":
        return self._cmp(CmpOp.NE, other)

    def lt(self, other: Any) -> "Expr":
        return self._cmp(CmpOp.LT, other)

    def le(self, other: Any) -> "Expr":
        return self._cmp(CmpOp.LE, other)

    def gt(self, other: Any) -> "Expr":
        return self._cmp(CmpOp.GT, other)

    def ge(self, other: Any) -> "Expr":
        return self._cmp(CmpOp.GE, other)

    def is_distinct_from(self, other: Any) -> "Expr":
        """Not equal, counting NULL as a value. Never NULL."""
        return self._cmp(CmpOp.IS_DISTINCT_FROM, other)

    def is_not_distinct_from(self, other: Any) -> "Expr":
        """Equal, counting NULL as a value. Never NULL."""
        return self._cmp(CmpOp.IS_NOT_DISTINCT_FROM, other)

    def is_null(self) -> "Expr":
        return Expr(ast.IsNull(self.node, False))

    def is_not_null(self) -> "Expr":
        return Expr(ast.IsNull(self.node, True))

    def is_not_true(self) -> "Expr":
        """True unless the value is true: NULL counts as false."""
        return self.is_distinct_from(True)

    def and_(self, other: Any) -> "Expr":
        if isinstance(self.node, ast.And):
            return Expr(ast.And(self.node.parts + (node(other),)))
        return Expr(ast.And((self.node, node(other))))

    def or_(self, other: Any) -> "Expr":
        if isinstance(self.node, ast.Or):
            return Expr(ast.Or(self.node.parts + (node(other),)))
        return Expr(ast.Or((self.node, node(other))))

    def not_(self) -> "Expr":
        return Expr(ast.Not(self.node))

    def between(self, low: Any, high: Any) -> "Expr":
        """``low <= self AND self <= high``."""
        return Expr(ast.And((self.ge(low).node, self.le(high).node)))

    def in_list(self, items: Any) -> "Expr":
        """Whether the value equals an item of the list: a Python list, a
        list column, or ``list_of(query)``."""
        return Expr(ast.In(self.node, node(items), False))

    def not_in_list(self, items: Any) -> "Expr":
        return Expr(ast.In(self.node, node(items), True))

    def like(self, pattern: Any) -> "Expr":
        """SQL ``LIKE``: ``%`` matches any run of characters, ``_`` any
        one, and a backslash makes the next character literal."""
        return Expr(ast.Like(self.node, node(pattern), False, False))

    def not_like(self, pattern: Any) -> "Expr":
        return Expr(ast.Like(self.node, node(pattern), False, True))

    def ilike(self, pattern: Any) -> "Expr":
        """``LIKE`` without regard to case."""
        return Expr(ast.Like(self.node, node(pattern), True, False))

    # ---- arithmetic

    def _arith(self, op: ArithOp, other: Any) -> "Expr":
        return Expr(ast.Arith(op, self.node, node(other)))

    def add(self, other: Any) -> "Expr":
        return self._arith(ArithOp.ADD, other)

    def sub(self, other: Any) -> "Expr":
        return self._arith(ArithOp.SUB, other)

    def mul(self, other: Any) -> "Expr":
        return self._arith(ArithOp.MUL, other)

    def div(self, other: Any) -> "Expr":
        return self._arith(ArithOp.DIV, other)

    def mod(self, other: Any) -> "Expr":
        return self._arith(ArithOp.MOD, other)

    __add__ = add
    __sub__ = sub
    __mul__ = mul
    __truediv__ = div
    __mod__ = mod

    def __radd__(self, other: Any) -> "Expr":
        return Expr(ast.Arith(ArithOp.ADD, node(other), self.node))

    def __rsub__(self, other: Any) -> "Expr":
        return Expr(ast.Arith(ArithOp.SUB, node(other), self.node))

    def __rmul__(self, other: Any) -> "Expr":
        return Expr(ast.Arith(ArithOp.MUL, node(other), self.node))

    def __rtruediv__(self, other: Any) -> "Expr":
        return Expr(ast.Arith(ArithOp.DIV, node(other), self.node))

    def __rmod__(self, other: Any) -> "Expr":
        return Expr(ast.Arith(ArithOp.MOD, node(other), self.node))

    def __neg__(self) -> "Expr":
        return Expr(ast.Neg(self.node))

    def __invert__(self) -> "Expr":
        return self.not_()

    # ---- conversions and functions

    def cast(self, to: Type) -> "Expr":
        return Expr(ast.Cast(self.node, to, False))

    def try_cast(self, to: Type) -> "Expr":
        """Converts, giving NULL where ``cast`` would fail."""
        return Expr(ast.Cast(self.node, to, True))

    def _call(self, func: Func, *others: Any) -> "Expr":
        return Expr(ast.Call(func, (self.node,) + tuple(node(o) for o in others)))

    def lower(self) -> "Expr":
        return self._call(Func.LOWER)

    def upper(self) -> "Expr":
        return self._call(Func.UPPER)

    def length(self) -> "Expr":
        """Length in characters."""
        return self._call(Func.LENGTH)

    def octet_length(self) -> "Expr":
        """Length in bytes. Does not load a large value."""
        return self._call(Func.OCTET_LENGTH)

    def trim(self) -> "Expr":
        return self._call(Func.TRIM)

    def left(self, chars: int) -> "Expr":
        """The first ``chars`` characters."""
        return self._call(Func.LEFT, chars)

    def substr(self, start: Any, length: Any) -> "Expr":
        """Counting characters from 1."""
        return self._call(Func.SUBSTR, start, length)

    def substr_bytes(self, start: Any, length: Any) -> "Expr":
        """Counting bytes from 1; gives bytes."""
        return self._call(Func.SUBSTR_BYTES, start, length)

    def strpos(self, needle: Any) -> "Expr":
        return self._call(Func.STRPOS, needle)

    def or_else(self, fallback: Any) -> "Expr":
        """This value, or ``fallback`` where it is NULL."""
        return self._call(Func.COALESCE, fallback)

    def json(self, path: Iterable[Any]) -> "Expr":
        """A member of a JSON value: ``col("detail").json(["device", "name"])``."""
        return self._call(Func.JSON_GET, *path)

    def json_text(self, path: Iterable[Any]) -> "Expr":
        """The same member as text."""
        return self._call(Func.JSON_GET_TEXT, *path)

    def json_contains(self, other: Any) -> "Expr":
        return self._call(Func.JSON_CONTAINS, other)

    def json_has_key(self, key: Any) -> "Expr":
        return self._call(Func.JSON_HAS_KEY, key)

    def json_has_any(self, keys: Any) -> "Expr":
        return self._call(Func.JSON_HAS_ANY, keys)

    def json_has_all(self, keys: Any) -> "Expr":
        return self._call(Func.JSON_HAS_ALL, keys)

    def json_merge(self, other: Any) -> "Expr":
        return self._call(Func.JSON_MERGE, other)

    def json_len(self) -> "Expr":
        return self._call(Func.JSON_LEN)

    def json_pluck(self, key: str) -> "Expr":
        return self._call(Func.JSON_PLUCK, key)

    def json_texts(self) -> "Expr":
        """A JSON array's items as a list of text."""
        return self._call(Func.JSON_TEXTS)

    def list_overlaps(self, other: Any) -> "Expr":
        return self._call(Func.LIST_OVERLAPS, other)

    def list_concat(self, other: Any) -> "Expr":
        return self._call(Func.LIST_CONCAT, other)

    # ---- aggregates

    def filter(self, condition: Any) -> "Expr":
        """An aggregate over only the rows where the condition is true."""
        if isinstance(self.node, ast.Agg):
            return Expr(replace(self.node, filter=node(condition)))
        return self

    def distinct(self) -> "Expr":
        """An aggregate over each distinct value once."""
        if isinstance(self.node, ast.Agg):
            return Expr(replace(self.node, distinct=True))
        return self

    # ---- ordering

    def desc(self) -> "Order":
        """For ``order_by``: this expression, descending."""
        return Order(ast.OrderSpec(self.node, True, None))

    def asc(self) -> "Order":
        return Order(ast.OrderSpec(self.node, False, None))


class Order:
    """One key of an ``order_by``."""

    __slots__ = ("spec",)

    def __init__(self, spec: ast.OrderSpec) -> None:
        self.spec = spec

    def nulls_last(self) -> "Order":
        return Order(replace(self.spec, nulls_last=True))

    def nulls_first(self) -> "Order":
        return Order(replace(self.spec, nulls_last=False))

    def __repr__(self) -> str:
        return f"Order({self.spec!r})"


def _order(value: Any) -> ast.OrderSpec:
    if isinstance(value, Order):
        return value.spec
    if isinstance(value, ast.OrderSpec):
        return value
    return ast.OrderSpec(node(value))


# ------------------------------------------------------- free functions


def col(name: str) -> Expr:
    """A column: ``col("name")``, or ``col("m.name")`` for the table named ``m``."""
    table, dot, column = name.partition(".")
    return Expr(ast.Column(table, column) if dot else ast.Column(None, name))


def val(value: Any) -> Expr:
    """A constant. Any Python value the database has a kind for; see
    ``database_client.values``."""
    if isinstance(value, Expr) or isinstance(value, _NODES):
        raise TypeError("val() takes a plain value, not an expression")
    return Expr(ast.Lit(value))


def null() -> Expr:
    return Expr(ast.Lit(None))


def now() -> Expr:
    """The server's clock when the transaction began, or for a statement
    outside one, when the statement began."""
    return Expr(ast.Call(Func.NOW, ()))


def excluded(column: str) -> Expr:
    """In an upsert's update: the value the insert proposed for a column."""
    return Expr(ast.Excluded(column))


def old(column: str) -> Expr:
    """In an update's ``returning``: a column as it was before the update."""
    return Expr(ast.Old(column))


def depth() -> Expr:
    """In a tree walk: how many steps from a start row."""
    return Expr(ast.Depth())


def rank() -> Expr:
    """In a full-text search: how well the row matches."""
    return Expr(ast.Rank())


def call(func: Func, args: Iterable[Any] = ()) -> Expr:
    return Expr(ast.Call(func, _nodes(args)))


def all_(parts: Iterable[Any]) -> Expr:
    """True when every part is. An empty list is true."""
    return Expr(ast.And(_nodes(parts)))


def any_(parts: Iterable[Any]) -> Expr:
    """True when any part is. An empty list is false."""
    return Expr(ast.Or(_nodes(parts)))


def coalesce(args: Iterable[Any]) -> Expr:
    return call(Func.COALESCE, args)


def least(args: Iterable[Any]) -> Expr:
    return call(Func.LEAST, args)


def greatest(args: Iterable[Any]) -> Expr:
    return call(Func.GREATEST, args)


def concat(args: Iterable[Any]) -> Expr:
    return call(Func.CONCAT, args)


def list_(items: Iterable[Any]) -> Expr:
    """A list of expressions' values: ``list_([col("date"), col("id")]).lt(list_([d, id]))``
    is SQL's ``(date, id) < ($1, $2)``."""
    return Expr(ast.ListOf(_nodes(items)))


def case(branches: Iterable[Tuple[Any, Any]], otherwise: Any = None) -> Expr:
    """``CASE WHEN condition THEN value ... ELSE otherwise END``. Without
    ``otherwise``, NULL where no branch holds."""
    pairs = tuple((node(c), node(v)) for c, v in branches)
    return Expr(ast.Case(pairs, None if otherwise is None else node(otherwise)))


def when(condition: Any, then: Any, otherwise: Any) -> Expr:
    """``then`` if the condition is true, else ``otherwise``."""
    return Expr(ast.Case(((node(condition), node(then)),), node(otherwise)))


def count() -> Expr:
    """The count of rows."""
    return Expr(ast.Agg(AggFunc.COUNT))


def _agg(func: AggFunc, arg: Any) -> Expr:
    return Expr(ast.Agg(func, node(arg)))


def sum_(arg: Any) -> Expr:
    return _agg(AggFunc.SUM, arg)


def min_(arg: Any) -> Expr:
    return _agg(AggFunc.MIN, arg)


def max_(arg: Any) -> Expr:
    return _agg(AggFunc.MAX, arg)


def avg(arg: Any) -> Expr:
    return _agg(AggFunc.AVG, arg)


def collect(arg: Any) -> Expr:
    """The values as a list, in the order the rows were read."""
    return _agg(AggFunc.COLLECT, arg)


def _query(query: Any) -> ast.QuerySpec:
    if isinstance(query, Query):
        return query.spec
    if isinstance(query, ast.QuerySpec):
        return query
    raise TypeError(f"expected a Query, got {query!r}")


def scalar(query: Any) -> Expr:
    """The first column of a query's first row, or NULL."""
    return Expr(ast.Subquery(_query(query), SubqueryKind.SCALAR))


def exists(query: Any) -> Expr:
    """Whether a query has any row."""
    return Expr(ast.Subquery(_query(query), SubqueryKind.EXISTS))


def list_of(query: Any) -> Expr:
    """The first column of each of a query's rows, as a list."""
    return Expr(ast.Subquery(_query(query), SubqueryKind.LIST))


def seconds(n: Union[int, float]) -> Expr:
    """A span of time in microseconds, to add to a timestamp:
    ``now() + seconds(60)``."""
    if isinstance(n, int):
        return Expr(ast.Lit(n * 1_000_000))
    micros = n * 1_000_000.0
    return Expr(ast.Lit(int(math.copysign(math.floor(abs(micros) + 0.5), micros))))


def minutes(n: int) -> Expr:
    return seconds(n * 60)


def hours(n: int) -> Expr:
    return seconds(n * 3600)


def days(n: int) -> Expr:
    return seconds(n * 86_400)


def like_escape(text: str) -> str:
    """Escapes ``%``, ``_`` and ``\\`` so the text matches only itself in a ``like``."""
    return "".join("\\" + c if c in "%_\\" else c for c in text)


# ------------------------------------------------------------- key ranges


class Bound:
    """One end of a key range: a prefix of the key."""

    UNBOUNDED = ast.BoundSpec(0)

    @staticmethod
    def included(values: Iterable[Any]) -> ast.BoundSpec:
        return ast.BoundSpec(1, _nodes(values))

    @staticmethod
    def excluded(values: Iterable[Any]) -> ast.BoundSpec:
        return ast.BoundSpec(2, _nodes(values))


class KeyRange:
    """A span of keys. Each bound is a prefix of the key: with an index on
    ``(user_id, date)``, ``KeyRange.eq([u])`` is every entry of that user.

    A bound that holds a NULL names no key, so the range is empty; to read
    the rows whose key is NULL, say so with ``KeyRange.is_`` or ``.nulls()``."""

    __slots__ = ("spec",)

    def __init__(self, lower: ast.BoundSpec = Bound.UNBOUNDED, upper: ast.BoundSpec = Bound.UNBOUNDED, nulls: bool = False) -> None:
        self.spec = ast.KeyRangeSpec(lower, upper, nulls)

    @staticmethod
    def all() -> "KeyRange":
        return KeyRange()

    @staticmethod
    def eq(prefix: Iterable[Any]) -> "KeyRange":
        """Every key that starts with these values."""
        values = _nodes(prefix)
        return KeyRange(ast.BoundSpec(1, values), ast.BoundSpec(1, values))

    @staticmethod
    def is_(prefix: Iterable[Any]) -> "KeyRange":
        """Like ``eq``, but a NULL among the values finds the keys that
        are NULL there: SQL's ``IS NOT DISTINCT FROM``."""
        return KeyRange.eq(prefix).nulls()

    @staticmethod
    def new(lower: ast.BoundSpec, upper: ast.BoundSpec) -> "KeyRange":
        return KeyRange(lower, upper)

    @staticmethod
    def within(prefix: Iterable[Any], lower: ast.BoundSpec, upper: ast.BoundSpec) -> "KeyRange":
        """Keys starting with ``prefix``, whose next part lies between the
        two bounds."""
        values = _nodes(prefix)

        def extend(bound: ast.BoundSpec) -> ast.BoundSpec:
            if bound.kind == 0:
                return ast.BoundSpec(1, values) if values else bound
            return ast.BoundSpec(bound.kind, values + bound.values)

        return KeyRange(extend(lower), extend(upper))

    def nulls(self) -> "KeyRange":
        """Makes a NULL in a bound stand for the keys that are NULL,
        instead of emptying the range."""
        return KeyRange(self.spec.lower, self.spec.upper, True)

    def __repr__(self) -> str:
        return f"KeyRange({self.spec!r})"


def _range(value: Any) -> ast.KeyRangeSpec:
    if isinstance(value, KeyRange):
        return value.spec
    if isinstance(value, ast.KeyRangeSpec):
        return value
    raise TypeError(f"expected a KeyRange, got {value!r}")


# ---------------------------------------------------------------- queries


def _select_columns(names: Iterable[str], short: bool) -> Tuple[ast.Select, ...]:
    if isinstance(names, str):
        raise TypeError("expected a list of column names; one column is written [name]")
    return tuple(ast.Select(name.rsplit(".", 1)[-1] if short else name, col(name).node) for name in names)


_ALL_COLUMNS = (ast.Select(ALL, ast.Column(None, ALL)),)


class Query:
    """A read as the client states it. There is no planner: a query names
    its way in (the primary key, one index, a tree walk or a full-text
    search) and the range of keys to read; the server reads those rows in
    key order and applies the rest in a fixed sequence: joins, ``each``,
    filter, grouping, sort, offset and limit, select."""

    __slots__ = ("spec",)

    def __init__(self, spec: ast.QuerySpec) -> None:
        self.spec = spec

    def __repr__(self) -> str:
        return f"Query({self.spec!r})"

    def _with(self, **changes: Any) -> "Query":
        return Query(replace(self.spec, **changes))

    @staticmethod
    def table(name: str) -> "Query":
        """Every row of a table, in primary key order, until narrowed."""
        return Query(ast.QuerySpec(name))

    @staticmethod
    def row() -> "Query":
        """One row of no table, to compute what needs none."""
        return Query.table("")

    def alias(self, alias: str) -> "Query":
        return self._with(alias=alias)

    def key(self, key: Iterable[Any]) -> "Query":
        """The row with this primary key."""
        return self._with(access=ast.Primary((KeyRange.eq(key).spec,)))

    def keys(self, keys: Iterable[Iterable[Any]]) -> "Query":
        """The rows with these primary keys, in the order given."""
        return self._with(access=ast.Primary(tuple(KeyRange.eq(k).spec for k in keys)))

    def range(self, key_range: Any) -> "Query":
        """Rows whose primary key is in the range."""
        return self._with(access=ast.Primary((_range(key_range),)))

    def index(self, index: str, key_range: Any) -> "Query":
        """Rows through an index, in its order."""
        return self._with(access=ast.Index(index, (_range(key_range),)))

    def index_ranges(self, index: str, ranges: Iterable[Any]) -> "Query":
        """Rows through an index, one range after another: ``IN`` over a key."""
        return self._with(access=ast.Index(index, tuple(_range(r) for r in ranges)))

    def reverse(self) -> "Query":
        """Reads the chosen range from its far end."""
        if isinstance(self.spec.access, (ast.Primary, ast.Index)):
            return self._with(access=replace(self.spec.access, reverse=True))
        return self

    def ancestors(self, start: Iterable[Iterable[Any]], parent: str) -> "Query":
        """The rows above the start rows: each one's parent, grandparent,
        and so on. ``parent`` is the column holding the parent's primary key."""
        return self._with(access=ast.Walk(tuple(_nodes(k) for k in start), parent, ast.Direction.UP))

    def descendants(self, start: Iterable[Iterable[Any]], parent: str, index: str) -> "Query":
        """The rows below the start rows. ``index`` is an index whose
        first key is the ``parent`` column."""
        return self._with(access=ast.Walk(tuple(_nodes(k) for k in start), parent, ast.Direction.DOWN, index))

    def prune(self, keep: Any) -> "Query":
        """In a walk: do not reach rows for which this is false, nor
        anything beyond them."""
        if isinstance(self.spec.access, ast.Walk):
            return self._with(access=replace(self.spec.access, prune=node(keep)))
        return self

    def stop_at(self, condition: Any) -> "Query":
        """In a walk: reach rows for which this is true, but go no further."""
        if isinstance(self.spec.access, ast.Walk):
            return self._with(access=replace(self.spec.access, stop=node(condition)))
        return self

    def max_depth(self, depth: int) -> "Query":
        if isinstance(self.spec.access, ast.Walk):
            return self._with(access=replace(self.spec.access, max_depth=depth))
        return self

    def search(self, index: str, prefix: Iterable[Any], query: Any) -> "Query":
        """Rows whose indexed text holds every word of ``query``."""
        return self._with(access=ast.Search(index, _nodes(prefix), node(query)))

    def match_prefix(self) -> "Query":
        """In a search: each word of the query also matches longer words
        that start with it."""
        if isinstance(self.spec.access, ast.Search):
            return self._with(access=replace(self.spec.access, match_prefix=True))
        return self

    def join(self, query: Any) -> "Query":
        """Another query, run once per row so far; rows without a match
        are dropped."""
        return self._with(joins=self.spec.joins + (ast.JoinSpec(ast.JoinKind.INNER, _query(query)),))

    def left_join(self, query: Any) -> "Query":
        """The same, keeping rows without a match, with NULLs."""
        return self._with(joins=self.spec.joins + (ast.JoinSpec(ast.JoinKind.LEFT, _query(query)),))

    def each(self, name: str, items: Any) -> "Query":
        """Sees each row once per distinct item of a list computed from
        it, the item readable as the column ``name``: SQL's ``unnest``."""
        return self._with(unnest=ast.Select(name, node(items)))

    def filter(self, condition: Any) -> "Query":
        """Keeps rows for which the condition is true. Called again, both
        conditions must hold."""
        existing = self.spec.filter
        if existing is None:
            return self._with(filter=node(condition))
        return self._with(filter=Expr(existing).and_(condition).node)

    def group_by(self, keys: Iterable[Any]) -> "Query":
        return self._with(group_by=self.spec.group_by + _nodes(keys))

    def having(self, condition: Any) -> "Query":
        return self._with(having=node(condition))

    def order_by(self, order: Any) -> "Query":
        return self._with(order_by=self.spec.order_by + (_order(order),))

    def offset(self, n: int) -> "Query":
        return self._with(offset=n)

    def limit(self, n: int) -> "Query":
        return self._with(limit=n)

    def columns(self, names: Iterable[str]) -> "Query":
        """Returns these columns of the table instead of all of them."""
        return self._with(select=self.spec.select + _select_columns(names, True))

    def select(self, name: str, expr: Any) -> "Query":
        """Adds a named expression to what the query returns."""
        return self._with(select=self.spec.select + (ast.Select(name, node(expr)),))

    def distinct(self) -> "Query":
        return self._with(distinct=True)

    def with_total(self) -> "Query":
        """Also count the rows the query would give without offset and limit."""
        return self._with(with_total=True)


# ----------------------------------------------------------------- writes


class Insert:
    __slots__ = ("spec",)

    def __init__(self, spec: ast.InsertSpec) -> None:
        self.spec = spec

    def __repr__(self) -> str:
        return f"Insert({self.spec!r})"

    def _with(self, **changes: Any) -> "Insert":
        return Insert(replace(self.spec, **changes))

    @staticmethod
    def into(table: str) -> "Insert":
        return Insert(ast.InsertSpec(table))

    def columns(self, names: Iterable[str]) -> "Insert":
        """The columns the rows give values for. The rest take their defaults."""
        if isinstance(names, str):
            raise TypeError("expected a list of column names")
        return self._with(columns=tuple(names))

    def values(self, row: Iterable[Any]) -> "Insert":
        """Adds a row, its values in the order of ``columns``."""
        if self.spec.query is not None:
            return self._with(query=None, rows=(_nodes(row),))
        return self._with(rows=self.spec.rows + (_nodes(row),))

    def set(self, column: str, value: Any) -> "Insert":
        """Sets one column of a single-row insert."""
        columns = self.spec.columns + (column,)
        if self.spec.query is not None:
            return self._with(columns=columns, query=None, rows=((node(value),),))
        first = self.spec.rows[0] if self.spec.rows else ()
        return self._with(columns=columns, rows=(first + (node(value),),) + self.spec.rows[1:])

    def from_query(self, query: Any) -> "Insert":
        """One row per row of the query, its columns in order."""
        return self._with(query=_query(query), rows=())

    def on_conflict_do_nothing(self) -> "Insert":
        """Skips a row that breaks any unique index."""
        return self._with(on_conflict=ast.OnConflictSpec(1))

    def on_conflict_do_nothing_on(self, index: str) -> "Insert":
        """Skips a row that conflicts on the named unique index."""
        return self._with(on_conflict=ast.OnConflictSpec(1, index))

    def on_conflict_update(self, index: str) -> "Insert":
        """On a conflict with the named unique index (``PRIMARY`` for the
        primary key), update the existing row instead."""
        return self._with(on_conflict=ast.OnConflictSpec(2, index))

    def _update(self, **changes: Any) -> "Insert":
        if self.spec.on_conflict.kind != 2:
            return self
        return self._with(on_conflict=replace(self.spec.on_conflict, **changes))

    def update(self, column: str, value: Any) -> "Insert":
        """In the conflict update: assign a column."""
        return self._update(set=self.spec.on_conflict.set + (ast.Assign(column, node(value)),))

    def update_excluded(self, columns: Iterable[str]) -> "Insert":
        """In the conflict update: take the proposed values for these columns."""
        return self._update(set=self.spec.on_conflict.set + tuple(ast.Assign(c, ast.Excluded(c)) for c in columns))

    def update_where(self, condition: Any) -> "Insert":
        """In the conflict update: only update when this holds for the
        existing row."""
        return self._update(filter=node(condition))

    def returning(self, columns: Iterable[str]) -> "Insert":
        return self._with(returning=self.spec.returning + _select_columns(columns, False))

    def returning_expr(self, name: str, expr: Any) -> "Insert":
        return self._with(returning=self.spec.returning + (ast.Select(name, node(expr)),))

    def returning_all(self) -> "Insert":
        """Returns every column of each row, as SQL's ``RETURNING *``."""
        return self._with(returning=_ALL_COLUMNS)


class Update:
    """Changes the rows a query finds. The query's table is the one
    updated; its joins only help choose rows and supply values."""

    __slots__ = ("spec",)

    def __init__(self, spec: ast.UpdateSpec) -> None:
        self.spec = spec

    def __repr__(self) -> str:
        return f"Update({self.spec!r})"

    def _rows(self, rows: Query) -> "Update":
        return Update(replace(self.spec, rows=rows.spec))

    @staticmethod
    def rows(rows: Any) -> "Update":
        return Update(ast.UpdateSpec(_query(rows)))

    @staticmethod
    def table(name: str) -> "Update":
        """Every row of the table, until narrowed with ``key``, ``index``
        or ``filter``."""
        return Update(ast.UpdateSpec(ast.QuerySpec(name)))

    def key(self, key: Iterable[Any]) -> "Update":
        return self._rows(Query(self.spec.rows).key(key))

    def range(self, key_range: Any) -> "Update":
        return self._rows(Query(self.spec.rows).range(key_range))

    def index(self, index: str, key_range: Any) -> "Update":
        return self._rows(Query(self.spec.rows).index(index, key_range))

    def filter(self, condition: Any) -> "Update":
        return self._rows(Query(self.spec.rows).filter(condition))

    def set(self, column: str, value: Any) -> "Update":
        return Update(replace(self.spec, set=self.spec.set + (ast.Assign(column, node(value)),)))

    def returning(self, columns: Iterable[str]) -> "Update":
        return Update(replace(self.spec, returning=self.spec.returning + _select_columns(columns, True)))

    def returning_expr(self, name: str, expr: Any) -> "Update":
        return Update(replace(self.spec, returning=self.spec.returning + (ast.Select(name, node(expr)),)))

    def returning_all(self) -> "Update":
        return Update(replace(self.spec, returning=_ALL_COLUMNS))


class Delete:
    """Removes the rows a query finds from the query's table."""

    __slots__ = ("spec",)

    def __init__(self, spec: ast.DeleteSpec) -> None:
        self.spec = spec

    def __repr__(self) -> str:
        return f"Delete({self.spec!r})"

    def _rows(self, rows: Query) -> "Delete":
        return Delete(replace(self.spec, rows=rows.spec))

    @staticmethod
    def rows(rows: Any) -> "Delete":
        return Delete(ast.DeleteSpec(_query(rows)))

    @staticmethod
    def from_(table: str) -> "Delete":
        return Delete(ast.DeleteSpec(ast.QuerySpec(table)))

    def key(self, key: Iterable[Any]) -> "Delete":
        return self._rows(Query(self.spec.rows).key(key))

    def range(self, key_range: Any) -> "Delete":
        return self._rows(Query(self.spec.rows).range(key_range))

    def index(self, index: str, key_range: Any) -> "Delete":
        return self._rows(Query(self.spec.rows).index(index, key_range))

    def filter(self, condition: Any) -> "Delete":
        return self._rows(Query(self.spec.rows).filter(condition))

    def returning(self, columns: Iterable[str]) -> "Delete":
        return Delete(replace(self.spec, returning=self.spec.returning + _select_columns(columns, False)))

    def returning_expr(self, name: str, expr: Any) -> "Delete":
        return Delete(replace(self.spec, returning=self.spec.returning + (ast.Select(name, node(expr)),)))

    def returning_all(self) -> "Delete":
        return Delete(replace(self.spec, returning=_ALL_COLUMNS))


# ----------------------------------------------------------------- schema


class ColumnDef:
    """A column that cannot be NULL and has no default, until told otherwise."""

    __slots__ = ("spec",)

    def __init__(self, name: str, type: Type) -> None:
        self.spec = ast.ColumnSpec(name, type)

    def _with(self, **changes: Any) -> "ColumnDef":
        out = ColumnDef.__new__(ColumnDef)
        out.spec = replace(self.spec, **changes)
        return out

    def nullable(self) -> "ColumnDef":
        return self._with(nullable=True)

    def default(self, value: Any) -> "ColumnDef":
        """What an insert that leaves the column out stores: a constant,
        or an expression evaluated per row."""
        return self._with(default=node(value))

    def default_expr(self, expr: Any) -> "ColumnDef":
        return self._with(default=node(expr))

    def default_now(self) -> "ColumnDef":
        return self.default_expr(now())

    def default_random_uuid(self) -> "ColumnDef":
        return self.default_expr(call(Func.RANDOM_UUID))

    def auto_increment(self) -> "ColumnDef":
        """An integer column that numbers rows."""
        return self._with(auto_increment=True)

    def __repr__(self) -> str:
        return f"ColumnDef({self.spec!r})"


class IndexDef:
    __slots__ = ("spec",)

    def __init__(self, name: str) -> None:
        self.spec = ast.IndexSpec(name)

    def _with(self, **changes: Any) -> "IndexDef":
        out = IndexDef.__new__(IndexDef)
        out.spec = replace(self.spec, **changes)
        return out

    def _key(self, expr: ast.ExprNode, desc: bool = False, each: bool = False) -> "IndexDef":
        return self._with(keys=self.spec.keys + (ast.IndexKey(expr, desc, each),))

    def on(self, columns: Iterable[str]) -> "IndexDef":
        """Adds plain columns as keys, ascending."""
        if isinstance(columns, str):
            raise TypeError("expected a list of column names")
        out = self
        for column in columns:
            out = out._key(col(column).node)
        return out

    def on_expr(self, expr: Any) -> "IndexDef":
        """Adds an expression as a key."""
        return self._key(node(expr))

    def on_desc(self, column: str) -> "IndexDef":
        """Adds a column as a key, descending."""
        return self._key(col(column).node, desc=True)

    def on_each(self, expr: Any) -> "IndexDef":
        """Adds a list-valued expression as a key, indexing the row once
        under each distinct item."""
        return self._key(node(expr), each=True)

    def unique(self) -> "IndexDef":
        return self._with(unique=True)

    def filter(self, condition: Any) -> "IndexDef":
        """Makes the index partial: only rows for which this is true."""
        return self._with(filter=node(condition))

    def full_text(self, config: TextConfig, fields: Iterable[Tuple[Any, float]]) -> "IndexDef":
        """Makes this a full-text index over the given text expressions,
        each with the weight a match in it counts for."""
        return self._with(text_config=config, text_fields=tuple(ast.TextField(node(e), float(weight)) for e, weight in fields))

    def __repr__(self) -> str:
        return f"IndexDef({self.spec!r})"


def _index(value: Any) -> ast.IndexSpec:
    if isinstance(value, IndexDef):
        return value.spec
    if isinstance(value, ast.IndexSpec):
        return value
    raise TypeError(f"expected an IndexDef, got {value!r}")


def _column(value: Any) -> ast.ColumnSpec:
    if isinstance(value, ColumnDef):
        return value.spec
    if isinstance(value, ast.ColumnSpec):
        return value
    raise TypeError(f"expected a ColumnDef, got {value!r}")


class TableDef:
    __slots__ = ("spec",)

    def __init__(self, name: str) -> None:
        self.spec = ast.TableSpec(name)

    def _with(self, **changes: Any) -> "TableDef":
        out = TableDef.__new__(TableDef)
        out.spec = replace(self.spec, **changes)
        return out

    def column(self, column: Any) -> "TableDef":
        return self._with(columns=self.spec.columns + (_column(column),))

    def primary_key(self, columns: Sequence[str]) -> "TableDef":
        if isinstance(columns, str):
            raise TypeError("expected a list of column names")
        return self._with(primary_key=tuple(columns))

    def index(self, index: Any) -> "TableDef":
        return self._with(indexes=self.spec.indexes + (_index(index),))

    def unique(self, name: str, columns: Iterable[str]) -> "TableDef":
        """A unique index over plain columns."""
        return self.index(IndexDef(name).on(columns).unique())

    def foreign_key(self, columns: Sequence[str], ref_table: str, on_delete: OnDelete) -> "TableDef":
        """A foreign key to the parent's primary key, named
        ``<table>_<column>_fkey``."""
        return self.foreign_key_to(columns, ref_table, (), on_delete)

    def foreign_key_to(self, columns: Sequence[str], ref_table: str, ref_columns: Sequence[str], on_delete: OnDelete) -> "TableDef":
        """A foreign key to columns other than the primary key: they must
        be the columns of a unique index of ``ref_table``."""
        if isinstance(columns, str) or isinstance(ref_columns, str):
            raise TypeError("expected lists of column names")
        name = f"{self.spec.name}_{'_'.join(columns)}_fkey"
        key = ast.ForeignKey(name, tuple(columns), ref_table, tuple(ref_columns), on_delete)
        return self._with(foreign_keys=self.spec.foreign_keys + (key,))

    def check(self, name: str, expr: Any) -> "TableDef":
        """A row is refused when this is false. NULL passes, as in SQL."""
        return self._with(checks=self.spec.checks + (ast.Check(name, node(expr)),))

    def volatile(self) -> "TableDef":
        """A table whose rows need not survive a crash or a restart."""
        return self._with(volatile=True)

    def __repr__(self) -> str:
        return f"TableDef({self.spec!r})"


def _table(value: Any) -> ast.TableSpec:
    if isinstance(value, TableDef):
        return value.spec
    if isinstance(value, ast.TableSpec):
        return value
    raise TypeError(f"expected a TableDef, got {value!r}")
