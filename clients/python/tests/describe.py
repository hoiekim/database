"""Between the golden vectors' JSON descriptions and this client's objects.

``protocol/fixtures/vectors.json`` describes each message in JSON (the
form ``protocol/tests/vectors.rs`` writes). ``*_from`` builds this
client's object from a description, to be encoded and compared with the
vector's bytes; ``*_to`` describes an object this client decoded, to be
compared with the vector's description.
"""

from __future__ import annotations

import json
import struct
import uuid
from decimal import Decimal
from pathlib import Path
from typing import Any, Dict, List, Optional

from database_client import Date, DbError, Json, Timestamp, ast, codec
from database_client.values import decimal_from_parts, decimal_parts

FIXTURES = Path(__file__).resolve().parents[3] / "protocol" / "fixtures" / "vectors.json"


def load() -> Dict[str, Any]:
    with open(FIXTURES, encoding="utf-8") as f:
        fixtures: Dict[str, Any] = json.load(f)
    return fixtures


def float_from(bits: str) -> float:
    number: float = struct.unpack(">d", bytes.fromhex(bits))[0]
    return number


def float_to(number: float) -> str:
    return struct.pack(">d", number).hex()


# ----------------------------------------------------------------- values


def value_from(d: Dict[str, Any]) -> Any:
    kind = d["kind"]
    if kind == "null":
        return None
    if kind == "bool":
        return d["value"]
    if kind == "int":
        return int(d["value"])
    if kind == "float":
        return float_from(d["bits"])
    if kind == "decimal":
        return decimal_from_parts(int(d["mantissa"]), d["scale"])
    if kind == "text":
        return d["value"]
    if kind == "bytes":
        return bytes.fromhex(d["hex"])
    if kind == "uuid":
        return uuid.UUID(hex=d["hex"])
    if kind == "timestamp":
        return Timestamp(int(d["micros"]))
    if kind == "date":
        return Date(d["days"])
    if kind == "json":
        return Json(json.loads(d["text"]))
    if kind == "list":
        return [value_from(item) for item in d["items"]]
    raise ValueError(f"a value of kind {kind}")


def value_to(v: Any) -> Dict[str, Any]:
    """Describes a value decoded with ``exact=True``."""
    if v is None:
        return {"kind": "null"}
    if isinstance(v, bool):
        return {"kind": "bool", "value": v}
    if isinstance(v, int):
        return {"kind": "int", "value": str(v)}
    if isinstance(v, float):
        return {"kind": "float", "bits": float_to(v)}
    if isinstance(v, Decimal):
        mantissa, scale = decimal_parts(v)
        return {"kind": "decimal", "mantissa": str(mantissa), "scale": scale}
    if isinstance(v, str):
        return {"kind": "text", "value": v}
    if isinstance(v, bytes):
        return {"kind": "bytes", "hex": v.hex()}
    if isinstance(v, uuid.UUID):
        return {"kind": "uuid", "hex": v.hex}
    if isinstance(v, Timestamp):
        return {"kind": "timestamp", "micros": str(v.micros)}
    if isinstance(v, Date):
        return {"kind": "date", "days": v.days}
    if isinstance(v, Json):
        return {"kind": "json", "text": codec.json_text(v.value)}
    if isinstance(v, list):
        return {"kind": "list", "items": [value_to(item) for item in v]}
    raise TypeError(f"not a value read exactly: {v!r}")


_TYPE_TAGS = {"bool": 1, "int": 2, "float": 3, "text": 5, "bytes": 6, "uuid": 7, "timestamp": 8, "date": 9, "json": 10}
_TYPE_KINDS = {tag: kind for kind, tag in _TYPE_TAGS.items()}


def type_from(d: Dict[str, Any]) -> ast.Type:
    kind = d["kind"]
    if kind == "list":
        return ast.Type.list(type_from(d["of"]))
    if kind == "decimal":
        return ast.Type.DECIMAL if d["precision"] is None else ast.Type.decimal(d["precision"], d["scale"])
    return ast.Type(_TYPE_TAGS[kind])


def type_to(t: ast.Type) -> Dict[str, Any]:
    if t.tag == 11:
        assert t.of is not None
        return {"kind": "list", "of": type_to(t.of)}
    if t.tag in (4, 12):
        return {"kind": "decimal", "precision": t.precision, "scale": t.scale}
    return {"kind": _TYPE_KINDS[t.tag]}


# ------------------------------------------------------------ expressions


def _name(member: Any) -> str:
    name: str = member.name.lower()
    return name


def opt_expr_from(d: Optional[Dict[str, Any]]) -> Optional[ast.ExprNode]:
    return None if d is None else expr_from(d)


def opt_expr_to(e: Optional[ast.ExprNode]) -> Optional[Dict[str, Any]]:
    return None if e is None else expr_to(e)


def exprs_from(items: List[Dict[str, Any]]) -> Any:
    return tuple(expr_from(item) for item in items)


def exprs_to(items: Any) -> List[Dict[str, Any]]:
    return [expr_to(item) for item in items]


def expr_from(d: Dict[str, Any]) -> ast.ExprNode:
    kind = d["kind"]
    if kind == "value":
        return ast.Lit(value_from(d["value"]))
    if kind == "column":
        return ast.Column(d["table"], d["name"])
    if kind == "excluded":
        return ast.Excluded(d["name"])
    if kind == "old":
        return ast.Old(d["name"])
    if kind == "depth":
        return ast.Depth()
    if kind == "rank":
        return ast.Rank()
    if kind == "not":
        return ast.Not(expr_from(d["expr"]))
    if kind == "and":
        return ast.And(exprs_from(d["parts"]))
    if kind == "or":
        return ast.Or(exprs_from(d["parts"]))
    if kind == "cmp":
        return ast.Cmp(ast.CmpOp[d["op"].upper()], expr_from(d["left"]), expr_from(d["right"]))
    if kind == "is_null":
        return ast.IsNull(expr_from(d["expr"]), d["negated"])
    if kind == "in":
        return ast.In(expr_from(d["expr"]), expr_from(d["list"]), d["negated"])
    if kind == "like":
        return ast.Like(expr_from(d["expr"]), expr_from(d["pattern"]), d["case_insensitive"], d["negated"])
    if kind == "arith":
        return ast.Arith(ast.ArithOp[d["op"].upper()], expr_from(d["left"]), expr_from(d["right"]))
    if kind == "neg":
        return ast.Neg(expr_from(d["expr"]))
    if kind == "call":
        return ast.Call(ast.Func[d["func"].upper()], exprs_from(d["args"]))
    if kind == "case":
        branches = tuple((expr_from(b["when"]), expr_from(b["then"])) for b in d["branches"])
        return ast.Case(branches, opt_expr_from(d["otherwise"]))
    if kind == "cast":
        return ast.Cast(expr_from(d["expr"]), type_from(d["to"]), d["lenient"])
    if kind == "list":
        return ast.ListOf(exprs_from(d["items"]))
    if kind == "agg":
        return ast.Agg(ast.AggFunc[d["func"].upper()], opt_expr_from(d["arg"]), opt_expr_from(d["filter"]), d["distinct"])
    if kind == "subquery":
        return ast.Subquery(query_from(d["query"]), ast.SubqueryKind[d["subquery"].upper()])
    raise ValueError(f"an expression of kind {kind}")


def expr_to(e: ast.ExprNode) -> Dict[str, Any]:
    if isinstance(e, ast.Lit):
        return {"kind": "value", "value": value_to(e.value)}
    if isinstance(e, ast.Column):
        return {"kind": "column", "table": e.table, "name": e.name}
    if isinstance(e, ast.Excluded):
        return {"kind": "excluded", "name": e.name}
    if isinstance(e, ast.Old):
        return {"kind": "old", "name": e.name}
    if isinstance(e, ast.Depth):
        return {"kind": "depth"}
    if isinstance(e, ast.Rank):
        return {"kind": "rank"}
    if isinstance(e, ast.Not):
        return {"kind": "not", "expr": expr_to(e.expr)}
    if isinstance(e, ast.And):
        return {"kind": "and", "parts": exprs_to(e.parts)}
    if isinstance(e, ast.Or):
        return {"kind": "or", "parts": exprs_to(e.parts)}
    if isinstance(e, ast.Cmp):
        return {"kind": "cmp", "op": _name(e.op), "left": expr_to(e.left), "right": expr_to(e.right)}
    if isinstance(e, ast.IsNull):
        return {"kind": "is_null", "expr": expr_to(e.expr), "negated": e.negated}
    if isinstance(e, ast.In):
        return {"kind": "in", "expr": expr_to(e.expr), "list": expr_to(e.list), "negated": e.negated}
    if isinstance(e, ast.Like):
        return {"kind": "like", "expr": expr_to(e.expr), "pattern": expr_to(e.pattern), "case_insensitive": e.case_insensitive, "negated": e.negated}
    if isinstance(e, ast.Arith):
        return {"kind": "arith", "op": _name(e.op), "left": expr_to(e.left), "right": expr_to(e.right)}
    if isinstance(e, ast.Neg):
        return {"kind": "neg", "expr": expr_to(e.expr)}
    if isinstance(e, ast.Call):
        return {"kind": "call", "func": _name(e.func), "args": exprs_to(e.args)}
    if isinstance(e, ast.Case):
        return {"kind": "case", "branches": [{"when": expr_to(c), "then": expr_to(v)} for c, v in e.branches], "otherwise": opt_expr_to(e.otherwise)}
    if isinstance(e, ast.Cast):
        return {"kind": "cast", "expr": expr_to(e.expr), "to": type_to(e.to), "lenient": e.lenient}
    if isinstance(e, ast.ListOf):
        return {"kind": "list", "items": exprs_to(e.items)}
    if isinstance(e, ast.Agg):
        return {"kind": "agg", "func": _name(e.func), "arg": opt_expr_to(e.arg), "filter": opt_expr_to(e.filter), "distinct": e.distinct}
    if isinstance(e, ast.Subquery):
        return {"kind": "subquery", "subquery": _name(e.kind), "query": query_to(e.query)}
    raise TypeError(f"not an expression: {e!r}")


# ---------------------------------------------------------------- queries

_BOUNDS = ["unbounded", "included", "excluded"]


def select_from(d: Dict[str, Any]) -> ast.Select:
    return ast.Select(d["name"], expr_from(d["expr"]))


def select_to(s: ast.Select) -> Dict[str, Any]:
    return {"name": s.name, "expr": expr_to(s.expr)}


def bound_from(d: Dict[str, Any]) -> ast.BoundSpec:
    return ast.BoundSpec(_BOUNDS.index(d["kind"]), exprs_from(d.get("values", [])))


def bound_to(b: ast.BoundSpec) -> Dict[str, Any]:
    if b.kind == 0:
        return {"kind": "unbounded"}
    return {"kind": _BOUNDS[b.kind], "values": exprs_to(b.values)}


def ranges_from(items: List[Dict[str, Any]]) -> Any:
    return tuple(ast.KeyRangeSpec(bound_from(r["lower"]), bound_from(r["upper"]), r["nulls"]) for r in items)


def ranges_to(items: Any) -> List[Dict[str, Any]]:
    return [{"lower": bound_to(r.lower), "upper": bound_to(r.upper), "nulls": r.nulls} for r in items]


def access_from(d: Dict[str, Any]) -> ast.Access:
    kind = d["kind"]
    if kind == "primary":
        return ast.Primary(ranges_from(d["ranges"]), d["reverse"])
    if kind == "index":
        return ast.Index(d["index"], ranges_from(d["ranges"]), d["reverse"])
    if kind == "walk":
        return ast.Walk(
            tuple(exprs_from(key) for key in d["start"]),
            d["parent"],
            ast.Direction[d["direction"].upper()],
            d["index"],
            opt_expr_from(d["prune"]),
            opt_expr_from(d["stop"]),
            d["max_depth"],
        )
    if kind == "search":
        return ast.Search(d["index"], exprs_from(d["prefix"]), expr_from(d["query"]), d["match_prefix"])
    raise ValueError(f"access of kind {kind}")


def access_to(a: ast.Access) -> Dict[str, Any]:
    if isinstance(a, ast.Primary):
        return {"kind": "primary", "ranges": ranges_to(a.ranges), "reverse": a.reverse}
    if isinstance(a, ast.Index):
        return {"kind": "index", "index": a.index, "ranges": ranges_to(a.ranges), "reverse": a.reverse}
    if isinstance(a, ast.Walk):
        return {
            "kind": "walk",
            "start": [exprs_to(key) for key in a.start],
            "parent": a.parent,
            "direction": _name(a.direction),
            "index": a.index,
            "prune": opt_expr_to(a.prune),
            "stop": opt_expr_to(a.stop),
            "max_depth": a.max_depth,
        }
    return {"kind": "search", "index": a.index, "prefix": exprs_to(a.prefix), "query": expr_to(a.query), "match_prefix": a.match_prefix}


def query_from(d: Dict[str, Any]) -> ast.QuerySpec:
    return ast.QuerySpec(
        table=d["table"],
        alias=d["alias"],
        access=access_from(d["access"]),
        joins=tuple(ast.JoinSpec(ast.JoinKind[j["kind"].upper()], query_from(j["query"])) for j in d["joins"]),
        unnest=None if d["unnest"] is None else select_from(d["unnest"]),
        filter=opt_expr_from(d["filter"]),
        group_by=exprs_from(d["group_by"]),
        having=opt_expr_from(d["having"]),
        order_by=tuple(ast.OrderSpec(expr_from(o["expr"]), o["desc"], o["nulls_last"]) for o in d["order_by"]),
        offset=int(d["offset"]),
        limit=None if d["limit"] is None else int(d["limit"]),
        select=tuple(select_from(s) for s in d["select"]),
        distinct=d["distinct"],
        with_total=d["with_total"],
    )


def query_to(q: ast.QuerySpec) -> Dict[str, Any]:
    return {
        "table": q.table,
        "alias": q.alias,
        "access": access_to(q.access),
        "joins": [{"kind": _name(j.kind), "query": query_to(j.query)} for j in q.joins],
        "unnest": None if q.unnest is None else select_to(q.unnest),
        "filter": opt_expr_to(q.filter),
        "group_by": exprs_to(q.group_by),
        "having": opt_expr_to(q.having),
        "order_by": [{"expr": expr_to(o.expr), "desc": o.desc, "nulls_last": o.nulls_last} for o in q.order_by],
        "offset": str(q.offset),
        "limit": None if q.limit is None else str(q.limit),
        "select": [select_to(s) for s in q.select],
        "distinct": q.distinct,
        "with_total": q.with_total,
    }


def assigns_from(items: List[Dict[str, Any]]) -> Any:
    return tuple(ast.Assign(a["column"], expr_from(a["value"])) for a in items)


_CONFLICTS = ["error", "do_nothing", "do_update"]


def insert_from(d: Dict[str, Any]) -> ast.InsertSpec:
    source, conflict = d["source"], d["on_conflict"]
    kind = _CONFLICTS.index(conflict["kind"])
    return ast.InsertSpec(
        table=d["table"],
        columns=tuple(d["columns"]),
        rows=tuple(exprs_from(row) for row in source["rows"]) if source["kind"] == "values" else (),
        query=query_from(source["query"]) if source["kind"] == "query" else None,
        on_conflict=ast.OnConflictSpec(kind, conflict.get("target"), assigns_from(conflict.get("set", [])), opt_expr_from(conflict.get("filter"))),
        returning=tuple(select_from(s) for s in d["returning"]),
    )


def update_from(d: Dict[str, Any]) -> ast.UpdateSpec:
    return ast.UpdateSpec(query_from(d["rows"]), assigns_from(d["set"]), tuple(select_from(s) for s in d["returning"]))


def delete_from(d: Dict[str, Any]) -> ast.DeleteSpec:
    return ast.DeleteSpec(query_from(d["rows"]), tuple(select_from(s) for s in d["returning"]))


# ----------------------------------------------------------------- schema


def column_from(d: Dict[str, Any]) -> ast.ColumnSpec:
    return ast.ColumnSpec(d["name"], type_from(d["type"]), d["nullable"], opt_expr_from(d["default"]), d["auto_increment"])


def column_to(c: ast.ColumnSpec) -> Dict[str, Any]:
    return {"name": c.name, "type": type_to(c.type), "nullable": c.nullable, "default": opt_expr_to(c.default), "auto_increment": c.auto_increment}


def index_from(d: Dict[str, Any]) -> ast.IndexSpec:
    kind = d["index"]
    full_text = kind["kind"] == "full_text"
    return ast.IndexSpec(
        name=d["name"],
        keys=tuple(ast.IndexKey(expr_from(k["expr"]), k["desc"], k["each"]) for k in d["keys"]),
        unique=d["unique"],
        filter=opt_expr_from(d["filter"]),
        text_config=ast.TextConfig[kind["config"].upper()] if full_text else None,
        text_fields=tuple(ast.TextField(expr_from(f["expr"]), float_from(f["weight_bits"])) for f in kind["fields"]) if full_text else (),
    )


def index_to(i: ast.IndexSpec) -> Dict[str, Any]:
    kind: Dict[str, Any] = {"kind": "ordered"}
    if i.text_config is not None:
        kind = {
            "kind": "full_text",
            "config": _name(i.text_config),
            "fields": [{"expr": expr_to(f.expr), "weight_bits": float_to(f.weight)} for f in i.text_fields],
        }
    return {
        "name": i.name,
        "keys": [{"expr": expr_to(k.expr), "desc": k.desc, "each": k.each} for k in i.keys],
        "unique": i.unique,
        "filter": opt_expr_to(i.filter),
        "index": kind,
    }


def table_from(d: Dict[str, Any]) -> ast.TableSpec:
    return ast.TableSpec(
        name=d["name"],
        columns=tuple(column_from(c) for c in d["columns"]),
        primary_key=tuple(d["primary_key"]),
        indexes=tuple(index_from(i) for i in d["indexes"]),
        foreign_keys=tuple(
            ast.ForeignKey(fk["name"], tuple(fk["columns"]), fk["ref_table"], tuple(fk["ref_columns"]), ast.OnDelete[fk["on_delete"].upper()]) for fk in d["foreign_keys"]
        ),
        checks=tuple(ast.Check(c["name"], expr_from(c["expr"])) for c in d["checks"]),
        volatile=d["volatile"],
    )


def table_to(t: ast.TableSpec) -> Dict[str, Any]:
    return {
        "name": t.name,
        "columns": [column_to(c) for c in t.columns],
        "primary_key": list(t.primary_key),
        "indexes": [index_to(i) for i in t.indexes],
        "foreign_keys": [
            {"name": fk.name, "columns": list(fk.columns), "ref_table": fk.ref_table, "ref_columns": list(fk.ref_columns), "on_delete": _name(fk.on_delete)}
            for fk in t.foreign_keys
        ],
        "checks": [{"name": c.name, "expr": expr_to(c.expr)} for c in t.checks],
        "volatile": t.volatile,
    }


# --------------------------------------------------------------- messages


def request_from(d: Dict[str, Any]) -> ast.Request:
    kind = d["kind"]
    if kind == "hello":
        return codec.request(codec.HELLO, d["version"], d["database"], d["password"], d["client"])
    if kind == "ping":
        return codec.request(codec.PING)
    if kind == "begin":
        return codec.request(codec.BEGIN, ast.TxnKind[d["txn"].upper()], int(d["lock_wait_ms"]))
    if kind == "commit":
        return codec.request(codec.COMMIT)
    if kind == "rollback":
        return codec.request(codec.ROLLBACK)
    if kind == "savepoint":
        return codec.request(codec.SAVEPOINT, d["name"])
    if kind == "rollback_to":
        return codec.request(codec.ROLLBACK_TO, d["name"])
    if kind == "release":
        return codec.request(codec.RELEASE, d["name"])
    if kind == "query":
        return codec.request(codec.QUERY, query_from(d["query"]))
    if kind == "insert":
        return codec.request(codec.INSERT, insert_from(d["insert"]))
    if kind == "update":
        return codec.request(codec.UPDATE, update_from(d["update"]))
    if kind == "delete":
        return codec.request(codec.DELETE, delete_from(d["delete"]))
    if kind == "describe":
        return codec.request(codec.DESCRIBE)
    if kind == "lock":
        return codec.request(codec.LOCK, d["name"], int(d["wait_ms"]))
    if kind == "unlock":
        return codec.request(codec.UNLOCK, d["name"])
    if kind == "now":
        return codec.request(codec.NOW)
    if kind == "ddl":
        return codec.request(codec.DDL, *_ddl_from(d["ddl"]))
    if kind == "admin":
        return codec.request(codec.ADMIN, *_admin_from(d["admin"]))
    raise ValueError(f"a request of kind {kind}")


def _ddl_from(d: Dict[str, Any]) -> Any:
    kind = d["kind"]
    if kind == "create_table":
        return codec.CREATE_TABLE, table_from(d["def"]), d["if_not_exists"]
    if kind == "drop_table":
        return codec.DROP_TABLE, d["name"], d["if_exists"]
    if kind == "add_column":
        return codec.ADD_COLUMN, d["table"], column_from(d["column"]), d["if_not_exists"]
    if kind == "drop_column":
        return codec.DROP_COLUMN, d["table"], d["column"], d["if_exists"]
    if kind == "create_index":
        return codec.CREATE_INDEX, d["table"], index_from(d["index"]), d["if_not_exists"]
    if kind == "drop_index":
        return codec.DROP_INDEX, d["table"], d["index"], d["if_exists"]
    raise ValueError(f"a schema change of kind {kind}")


def _admin_from(d: Dict[str, Any]) -> Any:
    kind = d["kind"]
    if kind == "create_database":
        return codec.CREATE_DATABASE, d["name"], d["password"], d["if_not_exists"]
    if kind == "drop_database":
        return codec.DROP_DATABASE, d["name"], d["if_exists"]
    if kind == "set_password":
        return codec.SET_PASSWORD, d["name"], d["password"]
    if kind == "dump":
        return codec.DUMP, d["databases"]
    if kind == "restore_begin":
        return codec.RESTORE_BEGIN, d["replace"]
    if kind == "restore_chunk":
        return codec.RESTORE_CHUNK, bytes.fromhex(d["hex"])
    plain = {"list_databases": codec.LIST_DATABASES, "checkpoint": codec.CHECKPOINT, "check": codec.CHECK, "stats": codec.STATS, "restore_end": codec.RESTORE_END}
    return (plain[kind],)


def response_to(r: ast.Response, error_names: Dict[int, str]) -> Dict[str, Any]:
    """Describes a response decoded with ``exact=True``."""
    kind, v = r.kind, r.value
    if kind == codec.R_OK:
        return {"kind": "ok"}
    if kind == codec.R_HELLO:
        return {"kind": "hello", "version": v[0], "server": v[1]}
    if kind == codec.R_ROWS:
        return {
            "kind": "rows",
            "more": r.more,
            "columns": v.columns,
            "affected": str(v.affected),
            "total": None if v.total is None else str(v.total),
            "rows": [[value_to(value) for value in row] for row in v.rows],
        }
    if kind == codec.R_TABLES:
        return {"kind": "tables", "tables": [table_to(t) for t in v]}
    if kind == codec.R_FLAG:
        return {"kind": "flag", "value": v}
    if kind == codec.R_TIME:
        return {"kind": "time", "micros": str(v)}
    if kind == codec.R_NAMES:
        return {"kind": "names", "names": v}
    if kind == codec.R_TEXT:
        return {"kind": "text", "text": v}
    if kind == codec.R_CHUNK:
        return {"kind": "chunk", "hex": v.hex()}
    if kind == codec.R_ERROR:
        assert isinstance(v, DbError)
        return {"kind": "error", "code": int(v.code), "code_name": error_names[int(v.code)], "message": v.message, "constraint": v.constraint, "table": v.table}
    raise ValueError(f"a response of kind {kind}")
