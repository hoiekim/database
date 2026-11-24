"""The builders say what the Rust client's builders say.

Each request here is written with this client's builders the way
``protocol/tests/vectors.rs`` writes it with the Rust ones, and must come
out as the same bytes: so ``Query.table(..).filter(..).filter(..)``,
``KeyRange.within``, ``returning`` and the rest mean the same in both.
"""

from __future__ import annotations

import unittest
import uuid
from typing import Any, Dict

from database_client import *
from database_client import ast, codec

from . import describe

VECTORS: Dict[str, Dict[str, Any]] = {v["name"]: v for v in describe.load()["vectors"]}


def sample_query() -> Query:
    return (
        Query.table("mails")
        .alias("m")
        .index("mails_user_date", KeyRange.within([uuid.UUID(bytes=bytes([1] * 16))], Bound.UNBOUNDED, Bound.excluded([5])))
        .reverse()
        .join(Query.table("map").alias("x").key([col("m.user_id"), "INBOX", col("m.mail_id")]))
        .left_join(Query.table("users").alias("u").key([col("m.user_id")]).limit(1))
        .each("address", col("to").json_pluck("address"))
        .filter(col("expunged").eq(False).and_(col("subject").ilike("%hi%")).and_(col("cc").is_null().not_()))
        .filter(any_([col("a").in_list([1, 2]), exists(Query.table("t").key([col("m.id")]))]))
        .group_by([col("user_id")])
        .having(count().gt(1))
        .order_by(col("date").desc().nulls_last())
        .order_by(max_(col("date")).filter(col("read").eq(False)))
        .offset(3)
        .limit(10)
        .select("n", count().distinct())
        .select("score", when(col("x").gt(1), col("x") * 2 + 1, null()).cast(Type.FLOAT))
        .select("j", col("doc").json_text(["a", 0]))
        .select("s", scalar(Query.table("t").select("m", max_(col("v")))))
        .distinct()
        .with_total()
    )


def sample_table() -> TableDef:
    return (
        TableDef("users")
        .column(ColumnDef("id", Type.UUID).default_random_uuid())
        .column(ColumnDef("email", Type.TEXT).nullable())
        .column(ColumnDef("n", Type.INT).auto_increment())
        .column(ColumnDef("amount", Type.decimal(15, 2)).default(0))
        .column(ColumnDef("ratio", Type.DECIMAL).nullable())
        .column(ColumnDef("tags", Type.list(Type.TEXT)).default([]))
        .column(ColumnDef("grid", Type.list(Type.list(Type.INT))).nullable())
        .column(ColumnDef("at", Type.TIMESTAMP).default_now())
        .column(ColumnDef("day", Type.DATE).nullable())
        .column(ColumnDef("flag", Type.BOOL).default(False))
        .column(ColumnDef("score", Type.FLOAT).default(1.5))
        .column(ColumnDef("blob", Type.BYTES).nullable())
        .column(ColumnDef("doc", Type.JSON).nullable())
        .column(ColumnDef("account", Type.UUID).nullable())
        .primary_key(["id"])
        .unique("users_n", ["n"])
        .index(IndexDef("users_email").on_expr(col("email").lower()).unique().filter(col("email").is_not_null()))
        .index(IndexDef("users_tags").on(["id"]).on_each(col("tags")).on_desc("at"))
        .index(IndexDef("users_text").on(["id"]).full_text(TextConfig.ENGLISH, [(col("email"), 1.0), (col("doc").json_text(["bio"]), 0.4)]))
        .index(IndexDef("users_words").full_text(TextConfig.SIMPLE, [(col("email"), 1.0)]))
        .foreign_key(["id"], "accounts", OnDelete.CASCADE)
        .foreign_key(["account"], "accounts", OnDelete.SET_NULL)
        .foreign_key_to(["email"], "addresses", ["address"], OnDelete.RESTRICT)
        .check("positive", col("n").ge(0))
        .volatile()
    )


class Builders(unittest.TestCase):
    def same(self, name: str, kind: int, *args: Any) -> None:
        vector = VECTORS["request." + name]
        encoded = codec.encode_request(codec.request(kind, *args), int(vector["timeout_ms"]))
        self.assertEqual(encoded.hex(), vector["hex"], name)

    def query(self, name: str, query: Query) -> None:
        self.same("query." + name, codec.QUERY, query.spec)

    def test_queries(self) -> None:
        one = uuid.UUID(bytes=bytes([1] * 16))
        self.query("table", Query.table("mails"))
        self.query("key", Query.table("mails").key([7]))
        self.query("keys", Query.table("mails").keys([[1, "a"], [2, "b"]]))
        self.query("range", Query.table("mails").range(KeyRange.new(Bound.excluded([10]), Bound.UNBOUNDED)).limit(50))
        self.query("index", Query.table("mails").index("mails_user_date", KeyRange.eq([one])).limit(50))
        self.query(
            "index_ranges",
            Query.table("mails").index_ranges(
                "mails_user_date",
                [KeyRange.eq([1]), KeyRange.is_([None]), KeyRange.all(), KeyRange.new(Bound.included([1, 2]), Bound.excluded([3])).nulls()],
            ),
        )
        self.query("sample", sample_query())
        self.query("row", Query.row().select("taken", exists(Query.table("t").key([1]))).select("at", now()))
        self.query("columns", Query.table("mails").alias("m").columns(["m.subject", "date"]))
        self.query("ancestors", Query.table("pages").ancestors([[9]], "parent_id"))
        self.query(
            "descendants",
            Query.table("pages")
            .descendants([[1], [2]], "parent_id", "pages_parent")
            .prune(col("deleted_at").is_null())
            .stop_at(col("id").eq(9))
            .max_depth(5)
            .select("d", depth()),
        )
        self.query("search", Query.table("mails").search("mails_search", [1], "hello world").match_prefix().order_by(rank().desc()))
        self.query(
            "order",
            Query.table("t").order_by(col("a")).order_by(col("b").desc()).order_by(col("c").asc().nulls_first()).order_by(col("d").desc().nulls_last()),
        )

    def test_every_expression(self) -> None:
        self.query(
            "every_expression",
            Query.table("t")
            .select("value", val("x"))
            .select("column", col("a"))
            .select("qualified", col("t.a"))
            .select("excluded", excluded("a"))
            .select("old", old("a"))
            .select("depth", depth())
            .select("rank", rank())
            .select("not", col("a").not_())
            .select("and", all_([]))
            .select("or", any_([col("a"), col("b"), col("c")]))
            .select("is_null", col("a").is_null())
            .select("is_not_null", col("a").is_not_null())
            .select("in", col("a").in_list(["x", "y"]))
            .select("not_in", col("a").not_in_list(list_of(Query.table("u").columns(["a"]))))
            .select("like", col("a").like("a\\%%"))
            .select("not_like", col("a").not_like("_"))
            .select("ilike", col("a").ilike("%A%"))
            .select("neg", -col("a"))
            .select("case", case([(col("a").gt(1), "many"), (col("a").eq(1), "one")]))
            .select("when", when(col("a").is_null(), 0, col("a")))
            .select("cast", col("a").cast(Type.decimal(15, 2)))
            .select("try_cast", col("a").try_cast(Type.list(Type.UUID)))
            .select("list", list_([col("a"), 1]).lt(list_([2, 3])))
            .select("count", count())
            .select("count_distinct", count().distinct().filter(col("a").gt(0)))
            .select("scalar", scalar(Query.table("u").key([col("t.a")]).columns(["b"])))
            .select("exists", exists(Query.table("u").key([col("t.a")])))
            .select("between", col("a").between(1, 9))
            .select("span", now() - minutes(5)),
        )

    def test_every_function_and_operator(self) -> None:
        every = Query.row()
        for name, number in describe.load()["tables"]["functions"].items():
            every = every.select(name, call(Func(number), [col("a")]))
        self.query("every_function", every)
        every = Query.table("t")
        for op in CmpOp:
            every = every.select(op.name.lower(), Expr(ast.Cmp(op, col("a").node, val(1).node)))
        for arith in ArithOp:
            every = every.select(arith.name.lower(), Expr(ast.Arith(arith, col("a").node, val(2).node)))
        for func in AggFunc:
            every = every.select(func.name.lower(), Expr(ast.Agg(func, col("a").node)))
        self.query("every_operator", every)

    def test_operators_are_the_methods(self) -> None:
        a = col("a")
        self.assertEqual((a + 1).node, a.add(1).node)
        self.assertEqual((a - 1).node, a.sub(1).node)
        self.assertEqual((a * 2).node, a.mul(2).node)
        self.assertEqual((a / 2).node, a.div(2).node)
        self.assertEqual((a % 2).node, a.mod(2).node)
        self.assertEqual((~a).node, a.not_().node)
        self.assertEqual((1 - a).node, val(1).sub(a).node)
        self.assertEqual(seconds(1.5).node, ast.Lit(1_500_000))
        self.assertEqual(seconds(-0.0000005).node, ast.Lit(-1))
        self.assertEqual(days(1).node, ast.Lit(86_400_000_000))
        self.assertEqual(like_escape("50%_\\"), "50\\%\\_\\\\")
        with self.assertRaises(TypeError):
            bool(a.eq(1))
        with self.assertRaises(TypeError):
            Query.table("t").key(7)  # type: ignore[arg-type]
        with self.assertRaises(TypeError):
            Query.table("t").key("abc")
        # A path is a list: a string would be spread into its letters.
        self.assertEqual(col("d").json(["device"]).node, ast.Call(Func.JSON_GET, (col("d").node, ast.Lit("device"))))
        for path in ["device", b"device", 7]:
            with self.assertRaises(TypeError):
                col("d").json(path)  # type: ignore[arg-type]
            with self.assertRaises(TypeError):
                col("d").json_text(path)  # type: ignore[arg-type]

    def test_a_number_past_its_fields_width_is_refused(self) -> None:
        def encode(q: Query) -> bytes:
            return codec.encode_request(codec.request(codec.QUERY, q.spec), 0)

        walk = Query.table("t").ancestors([[1]], "parent")
        for depth in [-1, 2**32]:
            with self.assertRaises(ValueError):
                encode(walk.max_depth(depth))
        encode(walk.max_depth(2**32 - 1))
        for bad in [Query.table("t").select("x", col("a").cast(Type.decimal(300, 2))), Query.table("t").select("x", col("a") + seconds(1e13)), Query.table("t").select("x", col("a") + days(110_000_000))]:
            with self.assertRaises(ValueError):
                encode(bad)

    def test_writes(self) -> None:
        self.same(
            "insert.upsert",
            codec.INSERT,
            Insert.into("t")
            .columns(["a", "b"])
            .values([1, now() + seconds(5)])
            .values([2, None])
            .on_conflict_update(PRIMARY)
            .update("b", excluded("b"))
            .update_excluded(["a"])
            .update_where(col("b").lt(excluded("b")))
            .returning(["a"])
            .returning_expr("old_b", old("b"))
            .spec,
        )
        self.same("insert.set", codec.INSERT, Insert.into("users").set("username", "ann").set("age", 30).returning(["id", "created"]).spec)
        self.same("insert.from_query", codec.INSERT, Insert.into("t").columns(["a"]).from_query(Query.table("s").columns(["a"])).on_conflict_do_nothing().spec)
        self.same("insert.do_nothing_on", codec.INSERT, Insert.into("t").set("a", 1).on_conflict_do_nothing_on("t_a").returning_all().spec)
        self.same("insert.empty", codec.INSERT, Insert.into("t").spec)
        self.same("update.rows", codec.UPDATE, Update.rows(sample_query()).set("hits", col("hits") + 1).returning(["hits"]).returning_expr("was", old("hits")).spec)
        self.same(
            "update.key",
            codec.UPDATE,
            Update.table("auth_codes").key([b"\x01\x02\x03"]).filter(col("used_at").is_null()).set("used_at", now()).returning(["m.client_id"]).spec,
        )
        self.same("delete.index", codec.DELETE, Delete.from_("t").index("t_a", KeyRange.eq([1])).filter(col("b").lt(now())).returning(["a"]).spec)
        self.same("delete.key", codec.DELETE, Delete.from_("access").key(["u", "svc"]).returning_all().spec)

    def test_a_builder_is_not_changed_by_what_is_built_from_it(self) -> None:
        base = Query.table("t").filter(col("a").eq(1))
        narrowed = base.filter(col("b").eq(2)).limit(1)
        self.assertIsNone(base.spec.limit)
        self.assertIsInstance(base.spec.filter, ast.Cmp)
        self.assertIsInstance(narrowed.spec.filter, ast.And)
        insert = Insert.into("t").set("a", 1)
        insert.set("b", 2)
        self.assertEqual(insert.spec.columns, ("a",))

    def test_schema(self) -> None:
        self.same("ddl.create_table", codec.DDL, codec.CREATE_TABLE, sample_table().spec, True)
        self.same("ddl.create_table.small", codec.DDL, codec.CREATE_TABLE, TableDef("t").column(ColumnDef("a", Type.INT)).primary_key(["a"]).spec, False)
        self.same("ddl.add_column", codec.DDL, codec.ADD_COLUMN, "t", ColumnDef("c", Type.JSON).nullable().spec, True)
        self.same("ddl.create_index", codec.DDL, codec.CREATE_INDEX, "t", IndexDef("i").on(["a"]).spec, False)


if __name__ == "__main__":
    unittest.main()
