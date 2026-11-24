"""This client against a real server: every kind of request, over TCP."""

from __future__ import annotations

import dataclasses
import datetime as dt
import io
import threading
import time
import unittest
import uuid
from decimal import Decimal
from typing import Any, List, Optional

from database_client import *
from database_client import codec
from database_client.conn import Conn

from .server import PASSWORD, Server

server: Optional[Server] = None


def setUpModule() -> None:
    global server
    server = Server()


def tearDownModule() -> None:
    if server is not None:
        server.stop()


class Case(unittest.TestCase):
    """Each test gets a database of its own on the shared server."""

    db: Client

    def setUp(self) -> None:
        assert server is not None
        self.server = server
        self.name = f"t_{uuid.uuid4().hex[:12]}"
        self.db = server.client(self.name)
        self.addCleanup(self.db.close)

    def code(self, expected: ErrorCode, call: Any, *args: Any) -> DbError:
        with self.assertRaises(DbError) as caught:
            call(*args)
        self.assertEqual(caught.exception.code, expected, caught.exception)
        return caught.exception


def users() -> TableDef:
    return (
        TableDef("users")
        .column(ColumnDef("id", Type.INT).auto_increment())
        .column(ColumnDef("name", Type.TEXT))
        .column(ColumnDef("email", Type.TEXT).nullable())
        .column(ColumnDef("age", Type.INT).default(0))
        .column(ColumnDef("created", Type.TIMESTAMP).default_now())
        .primary_key(["id"])
        .unique("users_name_key", ["name"])
        .index(IndexDef("users_email").on_expr(col("email").lower()).unique().filter(col("email").is_not_null()))
        .check("age_not_negative", col("age").ge(0))
    )


def orders() -> TableDef:
    return (
        TableDef("orders")
        .column(ColumnDef("id", Type.UUID).default_random_uuid())
        .column(ColumnDef("user_id", Type.INT))
        .column(ColumnDef("day", Type.DATE))
        .column(ColumnDef("amount", Type.decimal(15, 2)))
        .column(ColumnDef("paid", Type.BOOL).default(False))
        .primary_key(["id"])
        .index(IndexDef("orders_user_day").on(["user_id"]).on_desc("day").on(["id"]))
        .foreign_key(["user_id"], "users", OnDelete.CASCADE)
    )


class Schema(Case):
    def test_tables_columns_and_indexes(self) -> None:
        db = self.db
        db.create_table(users())
        db.create_table(orders())
        self.code(ErrorCode.ALREADY_EXISTS, db.create_table, users())
        db.create_table_if_not_exists(users())

        # What the server holds is what was sent, read back through the
        # decoders for definitions, expressions and types.
        described = {t.name: t for t in db.describe()}
        self.assertEqual(set(described), {"users", "orders"})
        self.assertEqual(described["users"], users().spec)
        # But for a foreign key to a primary key, whose columns the server
        # has filled in.
        key = orders().spec.foreign_keys[0]
        stored = dataclasses.replace(orders().spec, foreign_keys=(dataclasses.replace(key, ref_columns=("id",)),))
        self.assertEqual(key.ref_columns, ())
        self.assertEqual(described["orders"], stored)
        self.assertEqual(db.describe_table("orders"), stored)
        self.assertIsNone(db.describe_table("nothing"))

        db.add_column("users", ColumnDef("tags", Type.list(Type.TEXT)).default(["new"]))
        db.add_column_if_not_exists("users", ColumnDef("tags", Type.list(Type.TEXT)))
        db.create_index("users", IndexDef("users_tags").on_each(col("tags")))
        db.create_index_if_not_exists("users", IndexDef("users_tags").on_each(col("tags")))
        db.insert(Insert.into("users").set("name", "ann"))
        tagged = db.query(Query.table("users").index("users_tags", KeyRange.eq(["new"])))
        self.assertEqual(tagged.column("name"), ["ann"])
        table = db.describe_table("users")
        assert table is not None
        self.assertEqual(table.column("tags").type, Type.list(Type.TEXT))  # type: ignore[union-attr]
        self.assertIsNotNone(table.index("users_tags"))

        db.drop_index("users", "users_tags")
        db.drop_index_if_exists("users", "users_tags")
        self.code(ErrorCode.NOT_FOUND, db.drop_index, "users", "users_tags")
        db.drop_column("users", "tags")
        db.drop_column_if_exists("users", "tags")
        db.drop_table("orders")
        db.drop_table_if_exists("orders")
        self.code(ErrorCode.NOT_FOUND, db.drop_table, "orders")
        self.code(ErrorCode.NOT_FOUND, db.query, Query.table("orders"))


class Values(Case):
    def test_every_kind_of_value_comes_back_as_it_went(self) -> None:
        db = self.db
        db.create_table(
            TableDef("things")
            .column(ColumnDef("id", Type.INT))
            .column(ColumnDef("flag", Type.BOOL).nullable())
            .column(ColumnDef("n", Type.INT).nullable())
            .column(ColumnDef("x", Type.FLOAT).nullable())
            .column(ColumnDef("money", Type.decimal(15, 2)).nullable())
            .column(ColumnDef("exact", Type.DECIMAL).nullable())
            .column(ColumnDef("text", Type.TEXT).nullable())
            .column(ColumnDef("blob", Type.BYTES).nullable())
            .column(ColumnDef("uid", Type.UUID).nullable())
            .column(ColumnDef("at", Type.TIMESTAMP).nullable())
            .column(ColumnDef("day", Type.DATE).nullable())
            .column(ColumnDef("doc", Type.JSON).nullable())
            .column(ColumnDef("tags", Type.list(Type.TEXT)).nullable())
            .column(ColumnDef("grid", Type.list(Type.list(Type.INT))).nullable())
            .primary_key(["id"])
        )
        columns = ["id", "flag", "n", "x", "money", "exact", "text", "blob", "uid", "at", "day", "doc", "tags", "grid"]
        uid = uuid.UUID("6f9619ff-8b86-d011-b42d-00c04fc964ff")
        paris = dt.timezone(dt.timedelta(hours=2))
        at = dt.datetime(2026, 3, 5, 12, 20, 30, 123456, tzinfo=paris)
        first = [1, True, -(2**63), 1.5, Decimal("12.34"), Decimal("-0.000001"), "héllo, wörld: 日本語 🚀", b"\x00\xff\x07", uid, at, dt.date(2026, 3, 5), {"b": 1, "a": [1, 2.5, None, {"k": "v"}]}, ["a", "b"], [[1, 2], [], [3]]]
        second = [2, False, 2**63 - 1, float("-inf"), Decimal("5"), Decimal("123456789012345678.90"), "", b"", uuid.UUID(int=0), dt.datetime(1969, 12, 31, 23, 59, 59, 999999), dt.date(1, 1, 1), Json("just text"), [], None]
        third = [3] + [None] * 13
        db.insert(Insert.into("things").columns(columns).values(first).values(second).values(third))

        rows = db.query(Query.table("things"))
        self.assertEqual(rows.columns, tuple(columns))
        self.assertEqual(rows.affected, 3)
        one, two, three = rows
        self.assertEqual(one.values[:9], first[:9])
        self.assertIs(type(one["flag"]), bool)
        # A timestamp is one instant, read back in UTC.
        self.assertEqual(one["at"], at)
        self.assertEqual(one["at"].tzinfo, dt.timezone.utc)
        self.assertEqual(one["at"].hour, 10)
        self.assertEqual(one["day"], dt.date(2026, 3, 5))
        # JSON comes back parsed, its keys in the stored order.
        self.assertEqual(one["doc"], {"a": [1, 2.5, None, {"k": "v"}], "b": 1})
        self.assertEqual(list(one["doc"]), ["b", "a"][::-1])
        self.assertEqual(one["tags"], ["a", "b"])
        self.assertEqual(one["grid"], [[1, 2], [], [3]])

        self.assertEqual(two["n"], 2**63 - 1)
        self.assertEqual(two["x"], float("-inf"))
        # A decimal column keeps its scale: 5 in a (15, 2) column is 5.00.
        self.assertEqual(str(two["money"]), "5.00")
        self.assertEqual(two["exact"], Decimal("123456789012345678.90"))
        self.assertEqual(str(two["exact"]), "123456789012345678.90")
        self.assertEqual((two["text"], two["blob"], two["uid"]), ("", b"", uuid.UUID(int=0)))
        # A datetime without a zone was taken as UTC.
        self.assertEqual(two["at"], dt.datetime(1969, 12, 31, 23, 59, 59, 999999, tzinfo=dt.timezone.utc))
        self.assertEqual(two["day"], dt.date(1, 1, 1))
        self.assertEqual(two["doc"], "just text")
        self.assertEqual(two["tags"], [])
        self.assertEqual(three.values, third)

        # Values as keys and in expressions.
        self.assertEqual(db.get("things", [1])["uid"], uid)  # type: ignore[index]
        found = db.query(Query.table("things").filter(col("at").eq(at)).select("id", col("id")))
        self.assertEqual(found.column(), [1])
        found = db.query(Query.table("things").filter(col("money").gt(Decimal("5.001"))).select("id", col("id")))
        self.assertEqual(found.column(), [1])
        found = db.query(Query.table("things").filter(col("doc").json_text(["a", 3, "k"]).eq("v")).select("id", col("id")))
        self.assertEqual(found.column(), [1])

        # Times beyond what datetime can express are read as they are held.
        far = Timestamp(-(2**62))
        db.update(Update.table("things").key([3]).set("at", far).set("day", Date(-1_000_000)))
        row = db.get("things", [3])
        assert row is not None
        self.assertEqual((row["at"], row["day"]), (far, Date(-1_000_000)))

    def test_what_python_cannot_send_is_refused_before_it_is_sent(self) -> None:
        db = self.db
        for bad in [2**63, Decimal("NaN"), Decimal("1e-31"), object(), {1, 2}]:
            with self.assertRaises((TypeError, ValueError)):
                db.query(Query.row().select("v", val(bad)))
        # Nothing was sent, so nothing was broken.
        self.assertEqual(db.query_one(Query.row().select("v", val(1)))["v"], 1)


class Statements(Case):
    def setUp(self) -> None:
        super().setUp()
        self.db.create_table(users())
        self.db.create_table(orders())

    def test_insert_upsert_update_delete_with_returning(self) -> None:
        db = self.db
        inserted = db.insert(Insert.into("users").set("name", "ann").set("email", "Ann@Example.com").returning(["id", "created"]))
        self.assertEqual(inserted.affected, 1)
        ann = inserted.one()
        self.assertEqual(ann["id"], 1)
        self.assertIsInstance(ann["created"], dt.datetime)
        self.assertLess(abs((ann["created"] - db.now()).total_seconds()), 60)

        several = db.insert(Insert.into("users").columns(["name", "age"]).values(["bob", 30]).values(["cy", 40]).returning_all())
        self.assertEqual(several.affected, 2)
        self.assertEqual(several.columns, ("id", "name", "email", "age", "created"))
        self.assertEqual(several.column("name"), ["bob", "cy"])

        # An upsert on the name: the existing row is updated, the proposed
        # values read through excluded().
        upsert = (
            Insert.into("users")
            .set("name", "bob")
            .set("age", 31)
            .on_conflict_update("users_name_key")
            .update("age", excluded("age") + col("age"))
            .returning(["id", "age"])
            .returning_expr("was", old("age"))
        )
        self.assertEqual(db.insert(upsert).one().as_dict(), {"id": 2, "age": 61, "was": 30})
        skipped = db.insert(Insert.into("users").set("name", "bob").on_conflict_do_nothing())
        self.assertEqual((skipped.affected, len(skipped)), (0, 0))
        copied = db.insert(
            Insert.into("users").columns(["name", "age"]).from_query(Query.table("users").select("name", concat([col("name"), "2"])).select("age", col("age")))
        )
        self.assertEqual(copied.affected, 3)

        # Compare-and-swap: the count is the answer.
        swap = Update.table("users").key([1]).filter(col("age").eq(0)).set("age", col("age") + 1).returning(["age"]).returning_expr("was", old("age"))
        self.assertEqual(db.update(swap).one().as_dict(), {"age": 1, "was": 0})
        self.assertEqual(db.update(swap).affected, 0)
        raised = db.update(Update.table("users").filter(col("age").ge(30)).set("age", col("age") + 100))
        self.assertEqual(raised.affected, 4)

        gone = db.delete(Delete.from_("users").key([3]).returning(["name"]))
        self.assertEqual(gone.one()["name"], "cy")
        self.assertIsNone(db.get("users", [3]))
        self.assertEqual(db.delete(Delete.from_("users").filter(col("name").like("%2"))).affected, 3)
        self.assertEqual(db.query(Query.table("users")).column("name"), ["ann", "bob"])

    def test_a_query_with_a_range_a_join_a_filter_and_aggregates(self) -> None:
        db = self.db
        db.insert(Insert.into("users").columns(["name", "age"]).values(["ann", 30]).values(["bob", 40]).values(["cy", 50]))
        insert = Insert.into("orders").columns(["user_id", "day", "amount", "paid"])
        for user in (1, 2, 3):
            for n in range(1, 11):
                insert = insert.values([user, dt.date(2026, 1, n), Decimal(n * user) / 4, n % 2 == 0])
        self.assertEqual(db.insert(insert).affected, 30)

        # The newest five of one user, through the index, in its order.
        page = db.query(Query.table("orders").index("orders_user_day", KeyRange.eq([2])).limit(5).columns(["day", "amount"]))
        self.assertEqual(page.column("day"), [dt.date(2026, 1, n) for n in (10, 9, 8, 7, 6)])
        self.assertEqual(page[0]["amount"], Decimal("5.00"))
        # The next page: the entries after a cursor (on a descending key, earlier days).
        cursor = KeyRange.within([2], Bound.excluded([dt.date(2026, 1, 6)]), Bound.UNBOUNDED)
        self.assertEqual(db.query(Query.table("orders").index("orders_user_day", cursor).limit(2)).column("day"), [dt.date(2026, 1, 5), dt.date(2026, 1, 4)])
        backwards = db.query(Query.table("orders").index("orders_user_day", KeyRange.eq([2])).reverse().limit(1))
        self.assertEqual(backwards.column("day"), [dt.date(2026, 1, 1)])

        # Two users' orders joined to their users, filtered, grouped, counted.
        report = db.query(
            Query.table("orders")
            .alias("o")
            .index_ranges("orders_user_day", [KeyRange.eq([1]), KeyRange.eq([3])])
            .join(Query.table("users").alias("u").key([col("o.user_id")]).filter(col("u.age").ge(30)))
            .filter(col("o.day").between(dt.date(2026, 1, 3), dt.date(2026, 1, 8)))
            .group_by([col("u.name")])
            .select("name", col("u.name"))
            .select("orders", count())
            .select("paid", count().filter(col("o.paid")))
            .select("total", sum_(col("o.amount")))
            .select("largest", max_(col("o.amount")))
            .select("days", collect(col("o.day")).distinct())
            .order_by(col("name").desc())
        )
        self.assertEqual(report.columns, ("name", "orders", "paid", "total", "largest", "days"))
        cy, ann = report
        self.assertEqual(cy.values[:5], ["cy", 6, 3, Decimal("24.75"), Decimal("6.00")])
        self.assertEqual(ann.values[:5], ["ann", 6, 3, Decimal("8.25"), Decimal("2.00")])
        self.assertEqual(sorted(ann["days"]), [dt.date(2026, 1, n) for n in range(3, 9)])

        # A left join keeps the rows without a match; a subquery reads the enclosing row.
        db.insert(Insert.into("users").set("name", "dee"))
        counted = db.query(
            Query.table("users")
            .alias("u")
            .left_join(Query.table("orders").alias("o").index("orders_user_day", KeyRange.eq([col("u.id")])).limit(1))
            .select("name", col("u.name"))
            .select("latest", col("o.day"))
            .select("orders", scalar(Query.table("orders").index("orders_user_day", KeyRange.eq([col("u.id")])).select("n", count())))
            .select("any", exists(Query.table("orders").index("orders_user_day", KeyRange.eq([col("u.id")]))))
        )
        self.assertEqual(counted.dicts()[0], {"name": "ann", "latest": dt.date(2026, 1, 10), "orders": 10, "any": True})
        self.assertEqual(counted.dicts()[3], {"name": "dee", "latest": None, "orders": 0, "any": False})

        paged = db.query(Query.table("orders").filter(col("paid")).order_by(col("amount").desc()).order_by(col("day")).offset(2).limit(3).with_total().columns(["amount"]))
        self.assertEqual((len(paged), paged.affected, paged.total), (3, 3, 15))
        self.assertEqual(paged.column(), [Decimal("5.00"), Decimal("4.50"), Decimal("4.00")])
        row = db.query_one(Query.row().select("sum", val(1) + 2).select("text", val("a").upper()).select("list", list_([1, "b"])))
        self.assertEqual(row.as_dict(), {"sum": 3, "text": "A", "list": [1, "b"]})
        self.assertIsNone(db.query_opt(Query.table("users").key([99])))
        self.code(ErrorCode.NOT_FOUND, db.query_one, Query.table("users").key([99]))
        self.code(ErrorCode.INVALID_REQUEST, db.query_one, Query.table("users"))

    def test_trees_and_full_text_search(self) -> None:
        db = self.db
        db.create_table(
            TableDef("pages")
            .column(ColumnDef("id", Type.INT))
            .column(ColumnDef("parent_id", Type.INT).nullable())
            .column(ColumnDef("title", Type.TEXT))
            .primary_key(["id"])
            .index(IndexDef("pages_parent").on(["parent_id", "id"]))
            .index(IndexDef("pages_search").full_text(TextConfig.ENGLISH, [(col("title"), 1.0)]))
        )
        insert = Insert.into("pages").columns(["id", "parent_id", "title"])
        for page, parent, title in [(1, None, "Invoices"), (2, 1, "Overdue invoices"), (3, 1, "Paid"), (4, 2, "An overdue invoice from March"), (5, 4, "Notes")]:
            insert = insert.values([page, parent, title])
        db.insert(insert)
        up = db.query(Query.table("pages").ancestors([[5]], "parent_id").select("id", col("id")).select("depth", depth()))
        self.assertEqual([tuple(r) for r in up], [(5, 0), (4, 1), (2, 2), (1, 3)])
        down = db.query(Query.table("pages").descendants([[1]], "parent_id", "pages_parent").stop_at(col("id").eq(4)).max_depth(3).select("id", col("id")))
        self.assertEqual(down.column(), [1, 2, 3, 4])
        roots = db.query(Query.table("pages").index("pages_parent", KeyRange.is_([None])))
        self.assertEqual(roots.column("id"), [1])
        self.assertEqual(len(db.query(Query.table("pages").index("pages_parent", KeyRange.eq([None])))), 0)
        hits = db.query(Query.table("pages").search("pages_search", [], "invoice overdue").select("id", col("id")).select("rank", rank()).order_by(rank().desc()))
        self.assertEqual(sorted(hits.column("id")), [2, 4])
        self.assertTrue(all(isinstance(r["rank"], float) and r["rank"] > 0 for r in hits))
        prefixed = db.query(Query.table("pages").search("pages_search", [], "inv").match_prefix().select("id", col("id")))
        self.assertEqual(sorted(prefixed.column()), [1, 2, 4])

    def test_a_result_larger_than_a_frame_arrives_whole(self) -> None:
        db = self.db
        db.create_table(TableDef("big").column(ColumnDef("id", Type.INT)).column(ColumnDef("body", Type.TEXT)).primary_key(["id"]))
        body = "0123456789abcdef" * 8192  # 128 KiB
        for start in range(0, 48, 8):
            insert = Insert.into("big").columns(["id", "body"])
            for n in range(start, start + 8):
                insert = insert.values([n, f"{n}:{body}"])
            db.insert(insert)
        rows = db.query(Query.table("big"))
        self.assertEqual((len(rows), rows.affected), (48, 48))
        self.assertTrue(all(row["body"] == f"{row['id']}:{body}" for row in rows))

        # On the wire it was several frames, all but the last marked so.
        conn = Conn(self.server.config(self.name))
        self.addCleanup(conn.close)
        conn.send(codec.request(codec.QUERY, Query.table("big").spec))
        frames: List[Any] = []
        while not frames or frames[-1].more:
            frames.append(conn.receive())
        conn.settle()
        self.assertGreater(len(frames), 1)
        self.assertTrue(all(f.kind == codec.R_ROWS and f.value.affected == 48 for f in frames))
        self.assertEqual(sum(len(f.value.rows) for f in frames), 48)

    def test_errors_carry_their_code_and_what_was_broken(self) -> None:
        db = self.db
        db.insert(Insert.into("users").set("name", "ann").set("email", "ann@example.com"))
        duplicate = self.code(ErrorCode.UNIQUE_VIOLATION, db.insert, Insert.into("users").set("name", "ann"))
        self.assertEqual((duplicate.constraint, duplicate.table), ("users_name_key", "users"))
        self.assertTrue(duplicate.violates("users_name_key") and duplicate.is_unique_violation)
        self.assertIn("users_name_key", str(duplicate))
        primary = self.code(ErrorCode.UNIQUE_VIOLATION, db.insert, Insert.into("users").set("id", 1).set("name", "other"))
        self.assertEqual(primary.constraint, PRIMARY)
        expression = self.code(ErrorCode.UNIQUE_VIOLATION, db.insert, Insert.into("users").set("name", "ann2").set("email", "ANN@example.com"))
        self.assertEqual(expression.constraint, "users_email")
        missing = self.code(ErrorCode.NOT_NULL_VIOLATION, db.insert, Insert.into("users").set("email", "x@example.com"))
        self.assertEqual(missing.constraint, "name")
        check = self.code(ErrorCode.CHECK_VIOLATION, db.insert, Insert.into("users").set("name", "neg").set("age", -1))
        self.assertEqual(check.constraint, "age_not_negative")
        orphan = Insert.into("orders").set("user_id", 99).set("day", dt.date(2026, 1, 1)).set("amount", 1)
        self.assertEqual(self.code(ErrorCode.FOREIGN_KEY_VIOLATION, db.insert, orphan).constraint, "orders_user_id_fkey")
        self.code(ErrorCode.TYPE_MISMATCH, db.insert, Insert.into("users").set("name", 5))
        self.code(ErrorCode.TYPE_MISMATCH, db.query, Query.table("users").filter(col("name").eq(5)))
        self.code(ErrorCode.NOT_FOUND, db.query, Query.table("nothing"))
        self.code(ErrorCode.NOT_FOUND, db.query, Query.table("users").index("no_such_index", KeyRange.all()))
        self.code(ErrorCode.ARITHMETIC, db.query, Query.row().select("x", val(1) / 0))
        self.code(ErrorCode.OUT_OF_RANGE, db.insert, Insert.into("orders").set("user_id", 1).set("day", dt.date(2026, 1, 1)).set("amount", Decimal("1e14")))
        # An error is an answer: the connection carries on, and nothing was written.
        self.assertEqual(db.query(Query.table("users")).column("name"), ["ann"])
        # A delete takes its children with it, as the foreign key says.
        db.insert(Insert.into("orders").set("user_id", 1).set("day", dt.date(2026, 1, 1)).set("amount", 1))
        self.assertEqual(db.delete(Delete.from_("users").key([1])).affected, 1)
        self.assertEqual(len(db.query(Query.table("orders"))), 0)


class Transactions(Case):
    def setUp(self) -> None:
        super().setUp()
        self.db.create_table(TableDef("accounts").column(ColumnDef("id", Type.INT)).column(ColumnDef("balance", Type.INT)).primary_key(["id"]))
        self.db.insert(Insert.into("accounts").columns(["id", "balance"]).values([1, 100]).values([2, 100]))

    def balance(self, account: int, db: Optional[Db] = None) -> int:
        row = (db or self.db).get("accounts", [account])
        assert row is not None
        balance: int = row["balance"]
        return balance

    def test_commit_and_rollback(self) -> None:
        db = self.db
        with db.begin() as tx:
            tx.update(Update.table("accounts").key([1]).set("balance", 50))
            self.assertEqual(self.balance(1, tx), 50)
            # Nobody else sees it until the commit.
            self.assertEqual(self.balance(1), 100)
        self.assertEqual(self.balance(1), 50)
        self.assertTrue(tx.ended)
        self.code(ErrorCode.TRANSACTION_ABORTED, tx.query, Query.table("accounts"))
        self.code(ErrorCode.TRANSACTION_ABORTED, tx.commit)

        with self.assertRaises(RuntimeError):
            with db.begin_optimistic() as tx:
                tx.update(Update.table("accounts").key([1]).set("balance", 0))
                raise RuntimeError("changed my mind")
        self.assertEqual(self.balance(1), 50)

        tx = db.begin()
        tx.update(Update.table("accounts").key([1]).set("balance", 1))
        tx.rollback()
        self.assertEqual(self.balance(1), 50)
        self.assertEqual(db.exclusive(lambda tx: tx.update(Update.table("accounts").key([1]).set("balance", 60)).affected), 1)
        self.assertEqual(self.balance(1), 60)

    def test_a_failed_statement_ends_the_transaction_unless_a_savepoint_was_taken(self) -> None:
        db = self.db
        tx = db.begin()
        tx.update(Update.table("accounts").key([1]).set("balance", 10))
        tx.savepoint("before")
        self.code(ErrorCode.UNIQUE_VIOLATION, tx.insert, Insert.into("accounts").set("id", 1).set("balance", 0))
        self.code(ErrorCode.TRANSACTION_ABORTED, tx.query, Query.table("accounts"))
        tx.rollback_to("before")
        tx.update(Update.table("accounts").key([2]).set("balance", 20))
        tx.savepoint("again")
        tx.update(Update.table("accounts").key([2]).set("balance", 0))
        tx.rollback_to("again")
        tx.release("again")
        tx.commit()
        self.assertEqual((self.balance(1), self.balance(2)), (10, 20))

        tx = db.begin()
        self.code(ErrorCode.NOT_FOUND, tx.query, Query.table("nothing"))
        self.code(ErrorCode.TRANSACTION_ABORTED, tx.commit)
        self.assertTrue(tx.ended)

    def test_a_transaction_that_conflicts_is_run_again(self) -> None:
        db = self.db
        other = self.server.client(self.name)
        self.addCleanup(other.close)

        # By hand: what this one read was changed by a commit made beside it.
        tx = db.begin_optimistic()
        seen = self.balance(1, tx)
        other.update(Update.table("accounts").key([1]).set("balance", col("balance") + 1))
        tx.update(Update.table("accounts").key([1]).set("balance", seen - 30))
        self.assertFalse(tx.conflicted)
        self.code(ErrorCode.CONFLICT, tx.commit)
        self.assertTrue(tx.conflicted)
        self.assertEqual(self.balance(1), 101)

        # `transaction` does the running again by itself.
        runs: List[int] = []

        def transfer(tx: Transaction) -> int:
            runs.append(len(runs))
            available = self.balance(1, tx)
            if len(runs) == 1:
                # Another writer gets in between the read and the write.
                other.update(Update.table("accounts").key([1]).set("balance", col("balance") + 1))
            tx.update(Update.table("accounts").key([1]).set("balance", available - 30))
            tx.update(Update.table("accounts").key([2]).set("balance", col("balance") + 30))
            return available

        self.assertEqual(db.transaction(transfer), 102)
        self.assertEqual(runs, [0, 1])
        self.assertEqual((self.balance(1), self.balance(2)), (72, 130))

        # Beaten twice, the third run is the database's only writer: it
        # cannot conflict, and the other writer waits for it.
        runs.clear()

        def contended(tx: Transaction) -> None:
            runs.append(len(runs))
            available = self.balance(1, tx)
            if len(runs) <= 2:
                other.update(Update.table("accounts").key([1]).set("balance", col("balance") + 1))
            tx.update(Update.table("accounts").key([1]).set("balance", available - 1))

        db.transaction(contended)
        self.assertEqual(runs, [0, 1, 2])
        self.assertEqual(self.balance(1), 73)

        # An error of the work's own is not a conflict: it is raised, once.
        runs.clear()

        def failing(tx: Transaction) -> None:
            runs.append(1)
            tx.update(Update.table("accounts").key([2]).set("balance", 0))
            raise KeyError("no")

        with self.assertRaises(KeyError):
            db.transaction(failing)
        self.assertEqual((runs, self.balance(2)), ([1], 130))

    def test_a_transaction_that_conflicts_before_its_commit_is_run_again(self) -> None:
        db = self.db
        other = self.server.client(self.name)
        self.addCleanup(other.close)
        bump = Update.table("accounts").key([1]).set("balance", col("balance") + 1)

        # More than eight rows in one insert is more than the server does
        # twice: a transaction asked for it becomes the database's writer
        # there and then, running what it did again on the newest version.
        # If another commit has changed what it read, that fails with
        # CONFLICT at the insert, before any commit.
        def many(start: int) -> Insert:
            insert = Insert.into("accounts").columns(["id", "balance"])
            for id in range(start, start + 9):
                insert = insert.values([id, 0])
            return insert

        tx = db.begin_optimistic()
        self.balance(1, tx)
        other.update(bump)
        self.code(ErrorCode.CONFLICT, tx.insert, many(10))
        self.assertTrue(tx.conflicted)
        self.code(ErrorCode.CONFLICT, tx.commit)

        # `transaction` runs the work again when that happens.
        runs: List[int] = []

        def work(tx: Transaction) -> None:
            runs.append(len(runs))
            available = self.balance(1, tx)
            if len(runs) == 1:
                other.update(bump)
            tx.insert(many(10 * len(runs)))
            tx.update(Update.table("accounts").key([1]).set("balance", available - 1))

        db.transaction(work)
        self.assertEqual(runs, [0, 1])
        self.assertEqual(self.balance(1), 101)
        self.assertEqual(db.query(Query.table("accounts").filter(col("id").ge(10))).column("id"), list(range(20, 29)))

    def test_a_reading_transaction_sees_one_moment(self) -> None:
        db = self.db
        with db.begin_read() as tx:
            self.assertEqual(self.balance(1, tx), 100)
            db.update(Update.table("accounts").key([1]).set("balance", 7))
            db.insert(Insert.into("accounts").set("id", 3).set("balance", 1))
            self.assertEqual(self.balance(1, tx), 100)
            self.assertEqual(len(tx.query(Query.table("accounts"))), 2)
            self.code(ErrorCode.INVALID_REQUEST, tx.update, Update.table("accounts").key([1]).set("balance", 0))
            moment = tx.now_micros()
            time.sleep(0.01)
            self.assertEqual(tx.now_micros(), moment)
        self.assertEqual(self.balance(1), 7)
        total = db.read(lambda tx: sum(tx.query(Query.table("accounts")).column("balance")))
        self.assertEqual(total, 108)

    def test_many_threads_share_the_pool(self) -> None:
        db = self.server.client(self.name, pool_size=3)
        self.addCleanup(db.close)
        errors: List[BaseException] = []

        def work() -> None:
            try:
                for _ in range(10):
                    db.transaction(lambda tx: tx.update(Update.table("accounts").key([2]).set("balance", self.balance(2, tx) + 1)))
            except BaseException as e:  # noqa: BLE001
                errors.append(e)

        threads = [threading.Thread(target=work) for _ in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(errors, [])
        self.assertEqual(self.balance(2), 180)


class LocksAndTimeouts(Case):
    def test_a_named_lock_is_held_by_one_at_a_time(self) -> None:
        db = self.db
        other = self.server.client(self.name)
        self.addCleanup(other.close)
        guard = db.try_lock("migrate")
        assert guard is not None
        self.assertEqual(guard.name, "migrate")
        self.assertIsNone(other.try_lock("migrate"))
        elsewhere = other.try_lock("something else")
        assert elsewhere is not None
        elsewhere.unlock()

        waited: List[float] = []

        def wait() -> None:
            began = time.monotonic()
            with other.lock("migrate"):
                waited.append(time.monotonic() - began)

        thread = threading.Thread(target=wait)
        thread.start()
        time.sleep(0.2)
        self.assertEqual(waited, [])
        guard.unlock()
        thread.join(timeout=30)
        self.assertEqual(len(waited), 1)
        self.assertGreater(waited[0], 0.15)
        with db.lock("migrate") as again:
            self.assertIsNone(other.try_lock("migrate"))
            self.assertIsNotNone(again)

    def test_a_statement_past_its_deadline_is_stopped(self) -> None:
        db = self.db
        db.create_table(TableDef("big").column(ColumnDef("id", Type.INT)).column(ColumnDef("text", Type.TEXT)).primary_key(["id"]))
        for batch in range(8):
            insert = Insert.into("big").columns(["id", "text"])
            for n in range(500):
                insert = insert.values([batch * 500 + n, "lorem ipsum dolor sit amet " * 40])
            db.insert(insert)
        # Every row is read and matched against a pattern that never matches.
        slow = Query.table("big").filter(col("text").ilike("%a%b%c%d%e%f%g%h%zzz%")).select("n", count())
        quickest = float("inf")
        for _ in range(3):
            began = time.monotonic()
            self.assertEqual(db.query_one(slow)["n"], 0)
            quickest = min(quickest, time.monotonic() - began)
        # The deadline must fall while the statement runs, however fast
        # the machine is.
        hurried = self.server.client(self.name, statement_timeout=max(quickest / 8, 0.001))
        self.addCleanup(hurried.close)
        self.code(ErrorCode.TIMEOUT, hurried.query, slow)
        self.code(ErrorCode.TIMEOUT, hurried.update, Update.table("big").filter(col("text").ilike("%a%b%c%d%e%f%g%h%zzz%").not_()).set("text", "x"))
        # The connection is still good, and nothing was changed.
        self.assertEqual(len(hurried.get("big", [1])["text"]), 27 * 40)  # type: ignore[index]

    def test_a_connection_left_mid_answer_is_not_used_again(self) -> None:
        conn = Conn(self.server.config(self.name))
        self.addCleanup(conn.close)
        conn.send(codec.request(codec.PING))
        with self.assertRaises(DbError) as caught:
            conn.call(codec.request(codec.PING))
        self.assertEqual(caught.exception.code, ErrorCode.CONNECTION)


class Administration(Case):
    def test_databases_and_passwords(self) -> None:
        admin = self.server.admin()
        self.addCleanup(admin.close)
        self.assertIn(self.name, admin.admin.list_databases())
        other = f"{self.name}_other"
        admin.admin.create_database(other, "first")
        self.code(ErrorCode.ALREADY_EXISTS, admin.admin.create_database, other, "first")
        admin.admin.create_database_if_not_exists(other, "ignored")
        Client.connect(f"database://{other}:first@127.0.0.1:{self.server.port}").close()
        admin.admin.set_password(other, "s3cr@t/2")
        self.code(ErrorCode.ACCESS_DENIED, Client.connect, f"database://{other}:first@127.0.0.1:{self.server.port}")
        Client.connect(f"database://anything:s3cr%40t%2F2@127.0.0.1:{self.server.port}/{other}?pool=1&name=test").close()
        self.code(ErrorCode.ACCESS_DENIED, Client.connect, f"database://nothing:{PASSWORD}@127.0.0.1:{self.server.port}")
        # A database's own password does not administer the server.
        self.code(ErrorCode.ACCESS_DENIED, self.db.admin.list_databases)
        self.code(ErrorCode.ACCESS_DENIED, self.db.admin.create_database, "x", "y")
        admin.admin.drop_database(other)
        admin.admin.drop_database_if_exists(other)
        self.code(ErrorCode.NOT_FOUND, admin.admin.drop_database, other)
        self.assertNotIn(other, admin.admin.list_databases())
        unreachable = self.code(ErrorCode.CONNECTION, Client.connect, "database://a:b@127.0.0.1:1")
        self.assertIn("127.0.0.1:1", unreachable.message)
        # Times longer than a socket's timeout holds, which a URL may ask
        # for, are waited in pieces.
        far = Client.connect(f"database://{PASSWORD}@127.0.0.1:{self.server.port}/{self.name}?connect_timeout=1e10&statement_timeout=1e10")
        self.addCleanup(far.close)
        far.ping()

    def test_check_checkpoint_stats_dump_and_restore(self) -> None:
        db = self.db
        db.create_table(users())
        db.insert(Insert.into("users").columns(["name", "age"]).values(["ann", 30]).values(["bob", 40]))
        db.admin.checkpoint()
        self.assertIn("users", db.admin.check())
        self.assertIn("optimistic transactions", db.admin.stats())

        dump = io.BytesIO()
        written = db.admin.dump(dump)
        self.assertEqual(written, len(dump.getvalue()))
        self.assertGreater(written, 0)

        admin = self.server.admin()
        self.addCleanup(admin.close)
        db.delete(Delete.from_("users").key([1]))
        dump.seek(0)
        self.code(ErrorCode.ALREADY_EXISTS, admin.admin.restore, dump)
        dump.seek(0)
        admin.admin.restore(dump, replace=True)
        # The database was replaced under the old connections: they are
        # told to reconnect, and a new client reads what the dump held.
        self.code(ErrorCode.UNAVAILABLE, db.query, Query.table("users"))
        restored = Client.connect(self.server.config(self.name))
        self.addCleanup(restored.close)
        self.assertEqual(restored.query(Query.table("users")).column("name"), ["ann", "bob"])
        self.code(ErrorCode.INVALID_REQUEST, admin.admin.restore, io.BytesIO(dump.getvalue()[: written // 2]), True)
        self.assertEqual(restored.query(Query.table("users")).column("name"), ["ann", "bob"])


class Restarts(unittest.TestCase):
    def test_a_pooled_connection_from_before_a_restart_is_replaced(self) -> None:
        own = Server()
        self.addCleanup(own.stop)
        db = own.client("kept")
        self.addCleanup(db.close)
        db.create_table(TableDef("t").column(ColumnDef("id", Type.INT)).primary_key(["id"]))
        db.insert(Insert.into("t").set("id", 1))
        own.restart()
        # The pooled connection is dead, and a client cannot know before
        # it tries: that request fails, and the next has a new connection.
        with self.assertRaises(DbError) as caught:
            db.ping()
        self.assertEqual(caught.exception.code, ErrorCode.CONNECTION)
        self.assertEqual(db.query(Query.table("t")).column(), [1])
        # A connection that has sat idle is tried with a ping first, so
        # its caller sees nothing of the restart.
        own.restart()
        for conn in db._pool._idle:
            conn.last_used -= 60
        self.assertEqual(db.query(Query.table("t")).column(), [1])


if __name__ == "__main__":
    unittest.main()
