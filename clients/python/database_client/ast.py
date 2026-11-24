"""Requests as plain data: what the builders make and the codec writes.

Each class here is one structure of ``docs/PROTOCOL.md``, field for
field. The fluent builders (``Query``, ``Insert``, ``col`` and the rest)
produce these; ``describe()`` returns them. They hold no behaviour.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import IntEnum
from typing import Any, ClassVar, Optional, Tuple, Union

# ------------------------------------------------------------------ enums


class CmpOp(IntEnum):
    EQ = 0
    NE = 1
    LT = 2
    LE = 3
    GT = 4
    GE = 5
    #: Not equal, where NULL counts as a value: never NULL itself.
    IS_DISTINCT_FROM = 6
    IS_NOT_DISTINCT_FROM = 7


class ArithOp(IntEnum):
    ADD = 0
    SUB = 1
    MUL = 2
    DIV = 3
    MOD = 4


class AggFunc(IntEnum):
    #: Rows, or non-NULL values when given an argument.
    COUNT = 0
    SUM = 1
    MIN = 2
    MAX = 3
    AVG = 4
    #: The values as a list, in the order the rows were read.
    COLLECT = 5
    BOOL_OR = 6
    BOOL_AND = 7


class SubqueryKind(IntEnum):
    #: The first column of the first row, or NULL if there is none.
    SCALAR = 0
    #: Whether there is any row.
    EXISTS = 1
    #: The first column of every row, as a list.
    LIST = 2


class Func(IntEnum):
    """The functions an expression can call. ``docs/PROTOCOL.md`` says
    what each takes and gives."""

    LOWER = 1
    UPPER = 2
    LENGTH = 3
    OCTET_LENGTH = 4
    TRIM = 5
    LEFT = 6
    SUBSTR = 7
    SUBSTR_BYTES = 8
    STRPOS = 9
    CONCAT = 10
    REPLACE = 11
    HEX = 12
    SIMILARITY = 13
    ABS = 20
    CEIL = 21
    FLOOR = 22
    ROUND = 23
    POWER = 24
    LEAST = 25
    GREATEST = 26
    SIGN = 27
    COALESCE = 30
    NULLIF = 31
    NOW = 40
    PARSE_TIMESTAMP = 41
    RANDOM_UUID = 42
    DATE_PART = 43
    JSON_GET = 50
    JSON_GET_TEXT = 51
    JSON_TYPEOF = 52
    JSON_LEN = 53
    JSON_CONTAINS = 54
    JSON_HAS_KEY = 55
    JSON_HAS_ANY = 56
    JSON_HAS_ALL = 57
    JSON_MERGE = 58
    JSON_STRIP_NULLS = 59
    JSON_REMOVE = 60
    JSON_ELEMS = 61
    JSON_PLUCK = 62
    JSON_ARRAY = 63
    JSON_OBJECT = 64
    TO_JSON = 65
    JSON_TEXTS = 66
    LIST_LEN = 80
    LIST_OVERLAPS = 81
    LIST_CONCAT = 82
    LIST_DISTINCT = 83
    LIST_POSITION = 84
    MATCHES = 90
    MATCH_RANK = 91
    HEADLINE = 92


class Direction(IntEnum):
    #: From a row to its parent, and on up.
    UP = 0
    #: From a row to its children, and on down.
    DOWN = 1


class JoinKind(IntEnum):
    #: Rows without a match are dropped.
    INNER = 0
    #: Rows without a match are kept, with NULL for the joined columns.
    LEFT = 1


class OnDelete(IntEnum):
    #: Refuse to delete a parent that still has children.
    RESTRICT = 0
    #: Delete the children with it.
    CASCADE = 1
    #: Set the children's columns to NULL.
    SET_NULL = 2


class TextConfig(IntEnum):
    #: Runs of letters and digits, lower-cased.
    SIMPLE = 0
    #: The same, without common English words and endings.
    ENGLISH = 1


class TxnKind(IntEnum):
    #: It only reads: one moment of the database, and it holds up nobody.
    READ = 0
    #: It is the database's one writer from its beginning to its end.
    EXCLUSIVE = 1
    #: It may write, and runs beside other writers on a copy of its own.
    OPTIMISTIC = 2


# ------------------------------------------------------------------ types


@dataclass(frozen=True)
class Type:
    """A column's type. Use the constants and helpers: ``Type.INT``,
    ``Type.decimal(15, 2)``, ``Type.list(Type.TEXT)``."""

    #: The tag on the wire: 1 bool, 2 int, 3 float, 4 decimal, 5 text,
    #: 6 bytes, 7 uuid, 8 timestamp, 9 date, 10 json, 11 list,
    #: 12 decimal with precision and scale.
    tag: int
    #: For a list: the type of its items.
    of: Optional["Type"] = None
    precision: Optional[int] = None
    scale: Optional[int] = None

    @staticmethod
    def list(of: "Type") -> "Type":
        return Type(11, of=of)

    @staticmethod
    def decimal(precision: int, scale: int) -> "Type":
        """SQL's ``NUMERIC(precision, scale)``."""
        return Type(12, precision=precision, scale=scale)

    @property
    def name(self) -> str:
        if self.tag == 11:
            assert self.of is not None
            return f"list of {self.of.name}"
        if self.tag == 12:
            return f"decimal({self.precision}, {self.scale})"
        return _TYPE_NAMES[self.tag]

    BOOL: ClassVar["Type"]
    INT: ClassVar["Type"]
    FLOAT: ClassVar["Type"]
    DECIMAL: ClassVar["Type"]
    TEXT: ClassVar["Type"]
    BYTES: ClassVar["Type"]
    UUID: ClassVar["Type"]
    TIMESTAMP: ClassVar["Type"]
    DATE: ClassVar["Type"]
    JSON: ClassVar["Type"]


_TYPE_NAMES = {1: "bool", 2: "int", 3: "float", 4: "decimal", 5: "text", 6: "bytes", 7: "uuid", 8: "timestamp", 9: "date", 10: "json"}
Type.BOOL = Type(1)
Type.INT = Type(2)
Type.FLOAT = Type(3)
#: A decimal with no fixed precision or scale.
Type.DECIMAL = Type(4)
Type.TEXT = Type(5)
Type.BYTES = Type(6)
Type.UUID = Type(7)
Type.TIMESTAMP = Type(8)
Type.DATE = Type(9)
Type.JSON = Type(10)


# ------------------------------------------------------------ expressions


@dataclass(frozen=True)
class Lit:
    """A constant. ``value`` is any value ``values.Value`` allows."""

    value: Any


@dataclass(frozen=True)
class Column:
    """A column, optionally of one named table of the query."""

    table: Optional[str]
    name: str


@dataclass(frozen=True)
class Excluded:
    """In an upsert's update: the row that could not be inserted."""

    name: str


@dataclass(frozen=True)
class Old:
    """In an update's ``returning``: the row as it was before."""

    name: str


@dataclass(frozen=True)
class Depth:
    """In a tree walk: how many steps from a start row."""


@dataclass(frozen=True)
class Rank:
    """In a full-text search: how well the row matches."""


@dataclass(frozen=True)
class Not:
    expr: "ExprNode"


@dataclass(frozen=True)
class And:
    parts: Tuple["ExprNode", ...]


@dataclass(frozen=True)
class Or:
    parts: Tuple["ExprNode", ...]


@dataclass(frozen=True)
class Cmp:
    op: CmpOp
    left: "ExprNode"
    right: "ExprNode"


@dataclass(frozen=True)
class IsNull:
    expr: "ExprNode"
    #: ``IS NOT NULL``.
    negated: bool = False


@dataclass(frozen=True)
class In:
    expr: "ExprNode"
    list: "ExprNode"
    negated: bool = False


@dataclass(frozen=True)
class Like:
    expr: "ExprNode"
    pattern: "ExprNode"
    case_insensitive: bool = False
    negated: bool = False


@dataclass(frozen=True)
class Arith:
    op: ArithOp
    left: "ExprNode"
    right: "ExprNode"


@dataclass(frozen=True)
class Neg:
    expr: "ExprNode"


@dataclass(frozen=True)
class Call:
    func: Func
    args: Tuple["ExprNode", ...]


@dataclass(frozen=True)
class Case:
    """The first branch whose condition is true, else ``otherwise``, else NULL."""

    branches: Tuple[Tuple["ExprNode", "ExprNode"], ...]
    otherwise: Optional["ExprNode"] = None


@dataclass(frozen=True)
class Cast:
    expr: "ExprNode"
    to: Type
    #: A value that cannot be converted gives NULL instead of an error.
    lenient: bool = False


@dataclass(frozen=True)
class ListOf:
    """A list of the items' values."""

    items: Tuple["ExprNode", ...]


@dataclass(frozen=True)
class Agg:
    func: AggFunc
    arg: Optional["ExprNode"] = None
    filter: Optional["ExprNode"] = None
    distinct: bool = False


@dataclass(frozen=True)
class Subquery:
    query: "QuerySpec"
    kind: SubqueryKind


ExprNode = Union[Lit, Column, Excluded, Old, Depth, Rank, Not, And, Or, Cmp, IsNull, In, Like, Arith, Neg, Call, Case, Cast, ListOf, Agg, Subquery]


# ---------------------------------------------------------------- queries


@dataclass(frozen=True)
class Select:
    name: str
    expr: ExprNode


@dataclass(frozen=True)
class OrderSpec:
    expr: ExprNode
    desc: bool = False
    #: Where NULLs go. ``None`` puts them last ascending and first
    #: descending, as Postgres does.
    nulls_last: Optional[bool] = None


@dataclass(frozen=True)
class BoundSpec:
    """One end of a key range. ``kind`` is 0 for no bound, 1 for a bound
    that is part of the range, 2 for one that is not."""

    kind: int
    values: Tuple[ExprNode, ...] = ()


@dataclass(frozen=True)
class KeyRangeSpec:
    lower: BoundSpec
    upper: BoundSpec
    #: Whether a NULL in a bound stands for the keys that are NULL.
    nulls: bool = False


@dataclass(frozen=True)
class Primary:
    """Rows by primary key, in key order."""

    ranges: Tuple[KeyRangeSpec, ...]
    reverse: bool = False


@dataclass(frozen=True)
class Index:
    """Rows through an index, in the index's order."""

    index: str
    ranges: Tuple[KeyRangeSpec, ...]
    reverse: bool = False


@dataclass(frozen=True)
class Walk:
    """Rows of a tree held in one table."""

    start: Tuple[Tuple[ExprNode, ...], ...]
    parent: str
    direction: Direction
    index: Optional[str] = None
    prune: Optional[ExprNode] = None
    stop: Optional[ExprNode] = None
    max_depth: int = 1000


@dataclass(frozen=True)
class Search:
    """Rows matching a full-text search, through a full-text index."""

    index: str
    prefix: Tuple[ExprNode, ...]
    query: ExprNode
    match_prefix: bool = False


Access = Union[Primary, Index, Walk, Search]


@dataclass(frozen=True)
class JoinSpec:
    kind: JoinKind
    query: "QuerySpec"


_EVERY_ROW = (KeyRangeSpec(BoundSpec(0), BoundSpec(0)),)


@dataclass(frozen=True)
class QuerySpec:
    table: str
    alias: Optional[str] = None
    access: Access = Primary(_EVERY_ROW)
    joins: Tuple[JoinSpec, ...] = ()
    unnest: Optional[Select] = None
    filter: Optional[ExprNode] = None
    group_by: Tuple[ExprNode, ...] = ()
    having: Optional[ExprNode] = None
    order_by: Tuple[OrderSpec, ...] = ()
    offset: int = 0
    limit: Optional[int] = None
    #: Empty for every column of ``table``.
    select: Tuple[Select, ...] = ()
    distinct: bool = False
    with_total: bool = False


@dataclass(frozen=True)
class Assign:
    column: str
    value: ExprNode


@dataclass(frozen=True)
class OnConflictSpec:
    """``kind`` is 0 to fail, 1 to skip the row, 2 to update the existing one."""

    kind: int = 0
    target: Optional[str] = None
    set: Tuple[Assign, ...] = ()
    filter: Optional[ExprNode] = None


@dataclass(frozen=True)
class InsertSpec:
    table: str
    columns: Tuple[str, ...] = ()
    #: The rows to insert, each a tuple of expressions; unused when
    #: ``query`` is given.
    rows: Tuple[Tuple[ExprNode, ...], ...] = ()
    #: Insert this query's rows instead.
    query: Optional[QuerySpec] = None
    on_conflict: OnConflictSpec = OnConflictSpec()
    returning: Tuple[Select, ...] = ()


@dataclass(frozen=True)
class UpdateSpec:
    rows: QuerySpec
    set: Tuple[Assign, ...] = ()
    returning: Tuple[Select, ...] = ()


@dataclass(frozen=True)
class DeleteSpec:
    rows: QuerySpec
    returning: Tuple[Select, ...] = ()


# ----------------------------------------------------------------- schema


@dataclass(frozen=True)
class ColumnSpec:
    name: str
    type: Type
    nullable: bool = False
    default: Optional[ExprNode] = None
    auto_increment: bool = False


@dataclass(frozen=True)
class IndexKey:
    expr: ExprNode
    desc: bool = False
    #: The expression gives a list, and the row is indexed once under
    #: each distinct item.
    each: bool = False


@dataclass(frozen=True)
class TextField:
    expr: ExprNode
    #: How much a match in this field counts toward the rank.
    weight: float = 1.0


@dataclass(frozen=True)
class IndexSpec:
    name: str
    keys: Tuple[IndexKey, ...] = ()
    unique: bool = False
    filter: Optional[ExprNode] = None
    #: For a full-text index: how its text is cut into words. ``None``
    #: for an ordinary, ordered index.
    text_config: Optional[TextConfig] = None
    text_fields: Tuple[TextField, ...] = ()


@dataclass(frozen=True)
class ForeignKey:
    name: str
    columns: Tuple[str, ...]
    ref_table: str
    #: Empty for the parent's primary key.
    ref_columns: Tuple[str, ...] = ()
    on_delete: OnDelete = OnDelete.RESTRICT


@dataclass(frozen=True)
class Check:
    name: str
    expr: ExprNode


@dataclass(frozen=True)
class TableSpec:
    name: str
    columns: Tuple[ColumnSpec, ...] = ()
    primary_key: Tuple[str, ...] = ()
    indexes: Tuple[IndexSpec, ...] = ()
    foreign_keys: Tuple[ForeignKey, ...] = ()
    checks: Tuple[Check, ...] = ()
    volatile: bool = False

    def column(self, name: str) -> Optional[ColumnSpec]:
        return next((c for c in self.columns if c.name == name), None)

    def index(self, name: str) -> Optional[IndexSpec]:
        return next((i for i in self.indexes if i.name == name), None)


# --------------------------------------------------------------- messages


@dataclass(frozen=True)
class Request:
    """One request. ``kind`` is its tag on the wire; ``args`` its fields
    in the order the wire carries them (see ``codec.encode_request``)."""

    kind: int
    args: Tuple[Any, ...] = ()


@dataclass
class RowSet:
    columns: "list[str]" = field(default_factory=list)
    rows: "list[list[Any]]" = field(default_factory=list)
    #: Rows inserted, updated or deleted by a write; rows returned by a read.
    affected: int = 0
    #: For a query ``with_total``: rows before offset and limit.
    total: Optional[int] = None


@dataclass(frozen=True)
class Response:
    """One response frame. ``kind`` is its tag on the wire: 1 ok, 2 hello,
    3 rows, 4 tables, 5 flag, 6 time, 7 names, 8 text, 9 chunk, 10 error.
    ``value`` is what it carries; for rows, ``more`` says whether another
    frame of the same result follows."""

    kind: int
    value: Any = None
    more: bool = False
