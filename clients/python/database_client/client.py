"""The client: statements, transactions, named locks, administration."""

from __future__ import annotations

import datetime as _dt
import threading
from typing import Any, BinaryIO, Callable, Dict, Iterable, Iterator, List, Optional, Sequence, TypeVar, Union, overload

from . import ast, codec
from .builders import ColumnDef, Delete, IndexDef, Insert, Query, TableDef, Update, _column, _index, _table
from .conn import Config, Lease, Pool
from .errors import DbError, ErrorCode
from .values import Timestamp

T = TypeVar("T")

#: How many times ``Client.transaction`` runs its work beside other
#: writers before it runs it as the only one.
OPTIMISTIC_RUNS = 2


def _unexpected(what: str) -> DbError:
    return DbError(ErrorCode.CONNECTION, f"the server answered {what} with something else")


class Row(Sequence[Any]):
    """One row of a result: its values in order, also read by column name
    (``row["name"]``)."""

    __slots__ = ("columns", "values")

    def __init__(self, columns: Sequence[str], values: List[Any]) -> None:
        self.columns = columns
        self.values = values

    @overload
    def __getitem__(self, key: int) -> Any: ...

    @overload
    def __getitem__(self, key: str) -> Any: ...

    @overload
    def __getitem__(self, key: slice) -> List[Any]: ...

    def __getitem__(self, key: Union[int, str, slice]) -> Any:
        if isinstance(key, str):
            try:
                return self.values[self.columns.index(key)]
            except ValueError:
                raise KeyError(f"no column {key!r} in the result") from None
        return self.values[key]

    def get(self, name: str, default: Any = None) -> Any:
        """The column's value, or ``default`` if the result has no such column."""
        return self[name] if name in self.columns else default

    def __len__(self) -> int:
        return len(self.values)

    def as_dict(self) -> Dict[str, Any]:
        return dict(zip(self.columns, self.values))

    def __eq__(self, other: object) -> bool:
        if isinstance(other, Row):
            return list(self.columns) == list(other.columns) and self.values == other.values
        return NotImplemented

    def __repr__(self) -> str:
        return f"Row({self.as_dict()!r})"


class Rows(Sequence[Row]):
    """The rows a statement returned, and how many rows it touched."""

    __slots__ = ("columns", "affected", "total", "_rows")

    def __init__(self, rows: ast.RowSet) -> None:
        self.columns: Sequence[str] = tuple(rows.columns)
        #: Rows a write inserted, updated or deleted, whether or not it
        #: returned them; rows a read returned.
        self.affected = rows.affected
        #: For a query built ``with_total``: how many rows there are
        #: before offset and limit.
        self.total = rows.total
        self._rows = [Row(self.columns, values) for values in rows.rows]

    @overload
    def __getitem__(self, index: int) -> Row: ...

    @overload
    def __getitem__(self, index: slice) -> List[Row]: ...

    def __getitem__(self, index: Union[int, slice]) -> Union[Row, List[Row]]:
        return self._rows[index]

    def __len__(self) -> int:
        return len(self._rows)

    def __iter__(self) -> Iterator[Row]:
        return iter(self._rows)

    def first(self) -> Optional[Row]:
        return self._rows[0] if self._rows else None

    def opt(self) -> Optional[Row]:
        """The first row, if there is one."""
        return self.first()

    def one(self) -> Row:
        """The only row. An error if there is none or more than one."""
        if len(self._rows) == 1:
            return self._rows[0]
        if not self._rows:
            raise DbError(ErrorCode.NOT_FOUND, "expected one row, got none")
        raise DbError(ErrorCode.INVALID_REQUEST, "expected one row, got several")

    def column(self, name: Union[str, int] = 0) -> List[Any]:
        """One column of every row: the first, or the one named."""
        return [row[name] for row in self._rows]

    def dicts(self) -> List[Dict[str, Any]]:
        return [row.as_dict() for row in self._rows]

    def __repr__(self) -> str:
        return f"Rows(affected={self.affected}, total={self.total}, rows={self._rows!r})"


def _rows(response: ast.Response) -> Rows:
    if response.kind != codec.R_ROWS:
        raise _unexpected("a statement")
    return Rows(response.value)


def _ok(response: ast.Response) -> None:
    if response.kind != codec.R_OK:
        raise _unexpected("a command")


def _spec(statement: Any, builder: type, spec: type) -> Any:
    if isinstance(statement, builder):
        return getattr(statement, "spec")
    if isinstance(statement, spec):
        return statement
    raise TypeError(f"expected {builder.__name__}, got {statement!r}")


class Db:
    """What a ``Client`` and a ``Transaction`` have in common, so code that
    reads and writes can be handed either.

    On a ``Client`` each statement is its own transaction: it takes effect
    whole or not at all, and is durable when the call returns."""

    def _call(self, request: ast.Request) -> ast.Response:
        raise NotImplementedError

    def query(self, query: Union[Query, ast.QuerySpec]) -> Rows:
        """Runs a query."""
        return _rows(self._call(codec.request(codec.QUERY, _spec(query, Query, ast.QuerySpec))))

    def query_opt(self, query: Union[Query, ast.QuerySpec]) -> Optional[Row]:
        """Runs a query and gives its first row, if it has one."""
        return self.query(query).opt()

    def query_one(self, query: Union[Query, ast.QuerySpec]) -> Row:
        """Runs a query that must give exactly one row."""
        return self.query(query).one()

    def get(self, table: str, key: Iterable[Any]) -> Optional[Row]:
        """The row with this primary key."""
        return self.query_opt(Query.table(table).key(key))

    def insert(self, insert: Union[Insert, ast.InsertSpec]) -> Rows:
        """Inserts rows. The result holds what ``returning`` asked for
        and, as ``affected``, how many rows were inserted or updated."""
        return _rows(self._call(codec.request(codec.INSERT, _spec(insert, Insert, ast.InsertSpec))))

    def update(self, update: Union[Update, ast.UpdateSpec]) -> Rows:
        return _rows(self._call(codec.request(codec.UPDATE, _spec(update, Update, ast.UpdateSpec))))

    def delete(self, delete: Union[Delete, ast.DeleteSpec]) -> Rows:
        return _rows(self._call(codec.request(codec.DELETE, _spec(delete, Delete, ast.DeleteSpec))))

    def _ddl(self, *args: Any) -> None:
        _ok(self._call(codec.request(codec.DDL, *args)))

    def create_table(self, table: Union[TableDef, ast.TableSpec]) -> None:
        """Creates a table. An error if one of that name exists."""
        self._ddl(codec.CREATE_TABLE, _table(table), False)

    def create_table_if_not_exists(self, table: Union[TableDef, ast.TableSpec]) -> None:
        """Creates a table unless one of that name exists. Does not
        compare the definitions."""
        self._ddl(codec.CREATE_TABLE, _table(table), True)

    def drop_table(self, name: str) -> None:
        self._ddl(codec.DROP_TABLE, name, False)

    def drop_table_if_exists(self, name: str) -> None:
        self._ddl(codec.DROP_TABLE, name, True)

    def add_column(self, table: str, column: Union[ColumnDef, ast.ColumnSpec]) -> None:
        """Adds a column. Existing rows read its default, or NULL."""
        self._ddl(codec.ADD_COLUMN, table, _column(column), False)

    def add_column_if_not_exists(self, table: str, column: Union[ColumnDef, ast.ColumnSpec]) -> None:
        self._ddl(codec.ADD_COLUMN, table, _column(column), True)

    def drop_column(self, table: str, column: str) -> None:
        self._ddl(codec.DROP_COLUMN, table, column, False)

    def drop_column_if_exists(self, table: str, column: str) -> None:
        self._ddl(codec.DROP_COLUMN, table, column, True)

    def create_index(self, table: str, index: Union[IndexDef, ast.IndexSpec]) -> None:
        """Adds an index and fills it from the rows already there."""
        self._ddl(codec.CREATE_INDEX, table, _index(index), False)

    def create_index_if_not_exists(self, table: str, index: Union[IndexDef, ast.IndexSpec]) -> None:
        self._ddl(codec.CREATE_INDEX, table, _index(index), True)

    def drop_index(self, table: str, index: str) -> None:
        self._ddl(codec.DROP_INDEX, table, index, False)

    def drop_index_if_exists(self, table: str, index: str) -> None:
        self._ddl(codec.DROP_INDEX, table, index, True)

    def describe(self) -> List[ast.TableSpec]:
        """Every table's definition as the server holds it."""
        response = self._call(codec.request(codec.DESCRIBE))
        if response.kind != codec.R_TABLES:
            raise _unexpected("describe")
        tables: List[ast.TableSpec] = response.value
        return tables

    def describe_table(self, name: str) -> Optional[ast.TableSpec]:
        """One table's definition, if the table exists."""
        return next((t for t in self.describe() if t.name == name), None)

    def now(self) -> _dt.datetime:
        """The server's clock. Inside a transaction, the moment it began."""
        return Timestamp(self.now_micros()).to_datetime()

    def now_micros(self) -> int:
        """The server's clock, in microseconds since 1970-01-01T00:00:00Z."""
        response = self._call(codec.request(codec.NOW))
        if response.kind != codec.R_TIME:
            raise _unexpected("now")
        micros: int = response.value
        return micros


def _ended() -> DbError:
    return DbError(ErrorCode.TRANSACTION_ABORTED, "the transaction has already ended")


class Transaction(Db):
    """An open transaction. Statements run through it see each other's
    effects; nobody else sees them until ``commit``.

    As a context manager it commits when the block ends and rolls back if
    the block raises. After a statement fails, the transaction can only be
    rolled back, or rolled back to a savepoint taken before the failure."""

    def __init__(self, lease: Lease) -> None:
        self._lease: Optional[Lease] = lease
        self._lock = threading.Lock()
        #: Whether the server has ended the transaction because a commit
        #: made beside it changed something it had read: it can be run
        #: again. Only an optimistic transaction ever is.
        self.conflicted = False

    def _note(self, e: DbError) -> None:
        if e.code == ErrorCode.CONFLICT:
            self.conflicted = True

    def _call(self, request: ast.Request) -> ast.Response:
        with self._lock:
            if self._lease is None or self._lease.conn is None:
                raise _ended()
            try:
                return self._lease.conn.call(request)
            except DbError as e:
                self._note(e)
                raise

    def _finish(self, kind: int) -> None:
        with self._lock:
            lease, self._lease = self._lease, None
            if lease is None or lease.conn is None:
                raise _ended()
            try:
                # Whatever the answer, the server has ended the
                # transaction: an error on commit means it was rolled back.
                response = lease.conn.call(codec.request(kind))
            except DbError as e:
                self._note(e)
                raise
            finally:
                lease.held = False
                lease.release()
        _ok(response)

    @property
    def ended(self) -> bool:
        return self._lease is None

    def commit(self) -> None:
        """Makes the transaction's changes permanent and visible. When
        this returns they are on disk."""
        self._finish(codec.COMMIT)

    def rollback(self) -> None:
        self._finish(codec.ROLLBACK)

    def savepoint(self, name: str) -> None:
        """Marks a point to return to with ``rollback_to``."""
        _ok(self._call(codec.request(codec.SAVEPOINT, name)))

    def rollback_to(self, name: str) -> None:
        """Undoes everything since the savepoint, which stays in place.
        Also clears the failed state a statement error left behind."""
        _ok(self._call(codec.request(codec.ROLLBACK_TO, name)))

    def release(self, name: str) -> None:
        """Forgets the savepoint, keeping what was done since."""
        _ok(self._call(codec.request(codec.RELEASE, name)))

    def _abandon(self) -> None:
        if not self.ended:
            try:
                self.rollback()
            except DbError:
                pass

    def __enter__(self) -> "Transaction":
        return self

    def __exit__(self, kind: Any, error: Any, traceback: Any) -> None:
        if kind is not None:
            self._abandon()
        elif not self.ended:
            self.commit()


class LockGuard:
    """A named lock held on the server. See ``Client.lock``. As a context
    manager it unlocks when the block ends."""

    def __init__(self, lease: Lease, name: str) -> None:
        self._lease: Optional[Lease] = lease
        self.name = name

    def unlock(self) -> None:
        """Gives the lock up."""
        lease, self._lease = self._lease, None
        if lease is None or lease.conn is None:
            return
        try:
            _ok(lease.conn.call(codec.request(codec.UNLOCK, self.name)))
            lease.held = False
        finally:
            # Still held, the connection is closed, which gives the lock up too.
            lease.release()

    def __enter__(self) -> "LockGuard":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.unlock()


class Client(Db):
    """A handle on the database, safe to share between threads. It keeps
    a small pool of connections and opens them as they are needed."""

    def __init__(self, config: Config) -> None:
        self.config = config
        self._pool = Pool(config)

    @staticmethod
    def connect(target: Union[str, Config]) -> "Client":
        """Connects to a ``database://`` URL or a ``Config``, checking the
        address and the password with one connection."""
        client = Client(Config.from_url(target) if isinstance(target, str) else target)
        client.ping()
        return client

    def _call(self, request: ast.Request) -> ast.Response:
        with self._pool.acquire() as conn:
            return conn.call(request)

    def ping(self) -> None:
        """Whether the server answers."""
        _ok(self._call(codec.request(codec.PING)))

    def close(self) -> None:
        """Closes idle connections. The client can still be used and will
        reconnect."""
        self._pool.clear()

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    # ---- transactions

    def _begin(self, kind: ast.TxnKind) -> Transaction:
        lease = self._pool.acquire()
        try:
            assert lease.conn is not None
            _ok(lease.conn.call(codec.request(codec.BEGIN, kind, int(self.config.lock_wait * 1000))))
        except BaseException:
            lease.release()
            raise
        lease.held = True
        return Transaction(lease)

    def begin(self) -> Transaction:
        """Starts a transaction that is the database's only writer until
        it ends. It waits for any other writer to finish, and every other
        writer then waits for it: keep it short. Its commit cannot
        conflict."""
        return self._begin(ast.TxnKind.EXCLUSIVE)

    def begin_optimistic(self) -> Transaction:
        """Starts a transaction that may write and holds nobody up: it
        runs beside other writers, on a copy of the database that only it
        sees. Its ``commit`` checks that no commit made meanwhile has
        changed anything it was told; if one has, the commit fails with
        ``ErrorCode.CONFLICT``, nothing of the transaction takes effect,
        and it can be run again. ``transaction`` does all of that."""
        return self._begin(ast.TxnKind.OPTIMISTIC)

    def begin_read(self) -> Transaction:
        """Starts a transaction that only reads. It sees the database as
        of this moment whatever is written meanwhile, and blocks nobody."""
        return self._begin(ast.TxnKind.READ)

    def transaction(self, work: Callable[[Transaction], T]) -> T:
        """Runs ``work(tx)`` in a writing transaction: commits if it
        returns, rolls back if it raises. The result is as if nothing
        else had written while it ran.

        ``work`` may be called more than once. It runs first beside other
        writers (see ``begin_optimistic``). If another writer's commit
        then proves to have changed something it read, that run is undone
        whole and ``work`` is called again: once more the same way, and
        after that as the database's only writer, which cannot fail so.
        ``work`` should therefore do nothing but talk to the database
        through the transaction it is given; use ``exclusive`` for work
        that must run exactly once."""
        for _ in range(OPTIMISTIC_RUNS):
            tx = self.begin_optimistic()
            try:
                value = work(tx)
            except BaseException as e:
                conflicted = tx.conflicted
                tx._abandon()
                if conflicted and isinstance(e, Exception):
                    continue
                raise
            try:
                tx.commit()
                return value
            except DbError as e:
                if e.code != ErrorCode.CONFLICT:
                    raise
        return self.exclusive(work)

    def exclusive(self, work: Callable[[Transaction], T]) -> T:
        """Runs ``work(tx)``, once, in a transaction that is the
        database's only writer while it runs (see ``begin``)."""
        tx = self.begin()
        try:
            value = work(tx)
        except BaseException:
            tx._abandon()
            raise
        tx.commit()
        return value

    def read(self, work: Callable[[Transaction], T]) -> T:
        """The same for a transaction that only reads: ``work`` sees one
        moment of the database."""
        tx = self.begin_read()
        try:
            return work(tx)
        finally:
            tx._abandon()

    # ---- named locks

    def lock(self, name: str) -> LockGuard:
        """Takes the named lock, waiting as long as it takes. The lock
        belongs to the guard, not to a transaction: it is held until
        ``unlock``. For work that only one process should do at a time,
        such as migrations."""
        while True:
            # Wait in slices, so a statement timeout cannot cut the wait.
            guard = self._lock_for(name, 5_000)
            if guard is not None:
                return guard

    def try_lock(self, name: str) -> Optional[LockGuard]:
        """Takes the named lock if nobody holds it."""
        return self._lock_for(name, 0)

    def _lock_for(self, name: str, wait_ms: int) -> Optional[LockGuard]:
        lease = self._pool.acquire()
        try:
            assert lease.conn is not None
            response = lease.conn.call(codec.request(codec.LOCK, name, wait_ms))
            if response.kind != codec.R_FLAG:
                raise _unexpected("lock")
        except BaseException:
            lease.release()
            raise
        if response.value:
            lease.held = True
            return LockGuard(lease, name)
        lease.release()
        return None

    @property
    def admin(self) -> "Admin":
        """Server-wide operations. Most need the administrator's password."""
        return Admin(self)


class Admin:
    """Server-wide operations. See ``Client.admin``."""

    def __init__(self, client: Client) -> None:
        self._client = client

    def _call(self, *args: Any) -> ast.Response:
        return self._client._call(codec.request(codec.ADMIN, *args))

    def create_database(self, name: str, password: str) -> None:
        _ok(self._call(codec.CREATE_DATABASE, name, password, False))

    def create_database_if_not_exists(self, name: str, password: str) -> None:
        _ok(self._call(codec.CREATE_DATABASE, name, password, True))

    def drop_database(self, name: str) -> None:
        """Removes a database and its files."""
        _ok(self._call(codec.DROP_DATABASE, name, False))

    def drop_database_if_exists(self, name: str) -> None:
        _ok(self._call(codec.DROP_DATABASE, name, True))

    def set_password(self, name: str, password: str) -> None:
        _ok(self._call(codec.SET_PASSWORD, name, password))

    def list_databases(self) -> List[str]:
        response = self._call(codec.LIST_DATABASES)
        if response.kind != codec.R_NAMES:
            raise _unexpected("list databases")
        names: List[str] = response.value
        return names

    def checkpoint(self) -> None:
        """Moves everything the log holds into the data file."""
        _ok(self._call(codec.CHECKPOINT))

    def _text(self, sub: int, what: str) -> str:
        response = self._call(sub)
        if response.kind != codec.R_TEXT:
            raise _unexpected(what)
        text: str = response.value
        return text

    def check(self) -> str:
        """Verifies the files of the connection's database, or of every
        database on an administering connection. Returns a report; raises
        if anything is wrong."""
        return self._text(codec.CHECK, "check")

    def stats(self) -> str:
        return self._text(codec.STATS, "stats")

    def dump(self, out: BinaryIO, databases: Sequence[str] = ()) -> int:
        """Writes a dump of the named databases, or of all of them when
        none is named, to ``out``. The dump is each database as of one
        moment, taken without stopping writers. Returns the bytes written."""
        written = 0
        with self._client._pool.acquire() as conn:
            conn.send(codec.request(codec.ADMIN, codec.DUMP, list(databases)))
            while True:
                response = conn.receive()
                if response.kind == codec.R_CHUNK:
                    out.write(response.value)
                    written += len(response.value)
                elif response.kind == codec.R_OK:
                    conn.settle()
                    out.flush()
                    return written
                elif response.kind == codec.R_ERROR:
                    conn.settle()
                    raise response.value
                else:
                    raise _unexpected("dump")

    def restore(self, source: BinaryIO, replace: bool = False) -> None:
        """Loads a dump read from ``source``. Each database in it is
        created; one that already exists is an error unless ``replace``,
        which swaps it for the dump's. Nothing is changed unless the whole
        dump is read and verified."""
        lease = self._client._pool.acquire()
        try:
            conn = lease.conn
            assert conn is not None
            # Until the restore ends, the connection holds a half-loaded dump.
            lease.held = True
            _ok(conn.call(codec.request(codec.ADMIN, codec.RESTORE_BEGIN, replace)))
            while True:
                chunk = source.read(1 << 20)
                if not chunk:
                    break
                _ok(conn.call(codec.request(codec.ADMIN, codec.RESTORE_CHUNK, chunk)))
            try:
                _ok(conn.call(codec.request(codec.ADMIN, codec.RESTORE_END)))
            finally:
                lease.held = False
        finally:
            lease.release()
