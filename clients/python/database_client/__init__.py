"""The Python client for the database server.

    from database_client import *

    db = Client.connect("database://shop:secret@database:5454")
    db.create_table_if_not_exists(
        TableDef("users")
        .column(ColumnDef("id", Type.UUID).default_random_uuid())
        .column(ColumnDef("username", Type.TEXT))
        .column(ColumnDef("created", Type.TIMESTAMP).default_now())
        .primary_key(["id"])
        .unique("users_username_key", ["username"])
    )
    user_id = db.insert(Insert.into("users").set("username", "ann").returning(["id"])).one()["id"]
    ann = db.query_opt(Query.table("users").index("users_username_key", KeyRange.eq(["ann"])))

There is no SQL and no planner: a query says which key or index to read
and in what range, and the server does exactly that. ``docs/CLIENT.md`` in
the repository describes the vocabulary (for the Rust client, which this
one follows name for name); ``docs/PROTOCOL.md`` is the wire protocol.
"""

from . import ast
from .ast import AggFunc, ArithOp, CmpOp, Direction, Func, JoinKind, OnDelete, SubqueryKind, TextConfig, TxnKind, Type
from .builders import (
    ALL,
    PRIMARY,
    Bound,
    ColumnDef,
    Delete,
    Expr,
    IndexDef,
    Insert,
    KeyRange,
    Order,
    Query,
    TableDef,
    Update,
    all_,
    any_,
    avg,
    call,
    case,
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
    like_escape,
    list_,
    list_of,
    max_,
    min_,
    minutes,
    now,
    null,
    old,
    rank,
    scalar,
    seconds,
    sum_,
    val,
    when,
)
from .client import Admin, Client, Db, LockGuard, Row, Rows, Transaction
from .codec import MAX_FRAME, PROTOCOL_VERSION
from .conn import DEFAULT_PORT, Config
from .errors import DbError, ErrorCode
from .values import Date, Json, Timestamp

__version__ = "0.1.0"

__all__ = [
    "ALL",
    "Admin",
    "AggFunc",
    "ArithOp",
    "Bound",
    "Client",
    "CmpOp",
    "ColumnDef",
    "Config",
    "DEFAULT_PORT",
    "Date",
    "Db",
    "DbError",
    "Delete",
    "Direction",
    "ErrorCode",
    "Expr",
    "Func",
    "IndexDef",
    "Insert",
    "JoinKind",
    "Json",
    "KeyRange",
    "LockGuard",
    "MAX_FRAME",
    "OnDelete",
    "Order",
    "PRIMARY",
    "PROTOCOL_VERSION",
    "Query",
    "Row",
    "Rows",
    "SubqueryKind",
    "TableDef",
    "TextConfig",
    "Timestamp",
    "Transaction",
    "TxnKind",
    "Type",
    "Update",
    "all_",
    "any_",
    "ast",
    "avg",
    "call",
    "case",
    "coalesce",
    "col",
    "collect",
    "concat",
    "count",
    "days",
    "depth",
    "excluded",
    "exists",
    "greatest",
    "hours",
    "least",
    "like_escape",
    "list_",
    "list_of",
    "max_",
    "min_",
    "minutes",
    "now",
    "null",
    "old",
    "rank",
    "scalar",
    "seconds",
    "sum_",
    "val",
    "when",
]
