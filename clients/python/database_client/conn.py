"""Where the server is, one connection to it, and the pool that shares a few."""

from __future__ import annotations

import socket
import struct
import threading
import time
from dataclasses import dataclass
from typing import Any, List, Optional
from urllib.parse import unquote_to_bytes

from . import ast, codec
from .errors import DbError, ErrorCode

DEFAULT_PORT = 5454

#: A pooled connection that sat idle this long is pinged before it is
#: trusted with a request: a server restart may have closed it.
_IDLE_CHECK_SECONDS = 20.0

#: The longest one wait on a socket is set to; a socket refuses a timeout
#: of more than about 292 years, and a URL may ask for one. A longer wait
#: is taken in pieces.
_LONGEST_WAIT = 86_400.0


def _lost(what: object) -> DbError:
    return DbError(ErrorCode.CONNECTION, f"connection to the database lost: {what}")


def _decode(text: str) -> str:
    try:
        raw = unquote_to_bytes(text)
        # unquote leaves a bad escape as it is; the server's own client
        # refuses one, and so does this.
        i = text.find("%")
        while i != -1:
            if len(text) < i + 3 or any(c not in "0123456789abcdefABCDEF" for c in text[i + 1 : i + 3]):
                raise ValueError("a bad % escape")
            i = text.find("%", i + 3)
        return raw.decode("utf-8")
    except (ValueError, UnicodeDecodeError) as e:
        raise DbError(ErrorCode.INVALID_REQUEST, f"invalid database URL: {e}") from None


@dataclass
class Config:
    """Where the server is and how to talk to it. Times are in seconds."""

    host: str
    port: int = DEFAULT_PORT
    #: Empty for a connection that only administers the server.
    database: str = ""
    password: str = ""
    #: Connections kept at most. A transaction or a held lock occupies one
    #: until it ends.
    pool_size: int = 8
    connect_timeout: float = 10.0
    #: How long the server may spend on one statement. Zero for no limit
    #: beyond the server's own.
    statement_timeout: float = 60.0
    #: How long a write waits for the database's write lock before it
    #: fails with ``LOCK_TIMEOUT``.
    lock_wait: float = 30.0
    #: Sent to the server in the greeting.
    name: str = ""

    @staticmethod
    def from_url(url: str) -> "Config":
        """Reads ``database://<database>:<password>@<host>[:<port>][/<database>]``
        with optional ``?pool=8&connect_timeout=10&statement_timeout=60&lock_wait=30&name=app``
        (times in seconds). The database may be named before the password
        or as the path; an administrator's URL names none
        (``database://<password>@<host>``). Percent-encoding is decoded in
        the database, the password and the settings."""

        def bad(why: str) -> DbError:
            return DbError(ErrorCode.INVALID_REQUEST, f"invalid database URL: {why}")

        prefix = "database://"
        if not url.startswith(prefix):
            raise bad("it must start with database://")
        rest, _, query = url[len(prefix) :].partition("?")
        userinfo, at, address = rest.rpartition("@")
        if not at:
            raise bad("no password before the host")
        user, colon, password = userinfo.partition(":")
        if not colon:
            user, password = "", userinfo
        hostport, _, path = address.partition("/")
        host, colon, port_text = hostport.rpartition(":")
        if colon:
            if not port_text.isdigit() or not 0 <= int(port_text) <= 65535:
                raise bad("the port is not a number")
            port = int(port_text)
        else:
            host, port = hostport, DEFAULT_PORT
        if not host:
            raise bad("no host")
        config = Config(host, port, _decode(path if path else user), _decode(password))
        for pair in filter(None, query.split("&")):
            key, eq, raw = pair.partition("=")
            if not eq:
                raise bad("a setting without a value")
            value = _decode(raw)

            def seconds() -> float:
                try:
                    s = float(value)
                except ValueError:
                    s = float("nan")
                # Not NaN, not negative, and no more than a duration holds.
                if not 0 <= s < 2**64:
                    raise bad("a time that is not a number of seconds")
                return s

            if key == "pool":
                if not value.isdigit() or int(value) == 0:
                    raise bad("pool must be a positive number")
                config.pool_size = int(value)
            elif key == "connect_timeout":
                config.connect_timeout = seconds()
            elif key == "statement_timeout":
                config.statement_timeout = seconds()
            elif key == "lock_wait":
                config.lock_wait = seconds()
            elif key == "name":
                config.name = value
            else:
                raise bad("an unknown setting")
        return config


class Conn:
    """One connection: a socket that carries one request at a time."""

    def __init__(self, config: Config) -> None:
        self.statement_timeout_ms = int(config.statement_timeout * 1000)
        # The server enforces the statement's deadline and answers with
        # an error; this outer limit only catches a server gone silent.
        self.patience = 86_400.0 if config.statement_timeout == 0 else config.statement_timeout + config.lock_wait + 10.0
        #: Set while the stream cannot be trusted to be between two
        #: requests: after an I/O error, a timeout, or a caller that went
        #: away before the answer was read.
        self.broken = False
        self.last_used = time.monotonic()
        where = f"{config.host}:{config.port}"
        try:
            # A connect that has not succeeded within a day is not going to.
            self.sock = socket.create_connection((config.host.strip("[]"), config.port), timeout=min(config.connect_timeout, _LONGEST_WAIT))
        except socket.timeout:
            raise DbError(ErrorCode.CONNECTION, f"timed out connecting to the database at {where}") from None
        except OSError as e:
            raise DbError(ErrorCode.CONNECTION, f"cannot reach the database at {where}: {e}") from None
        try:
            # Requests are small and each waits for its answer: never
            # delay one to batch it with the next.
            self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            hello = codec.request(codec.HELLO, codec.PROTOCOL_VERSION, config.database, config.password, config.name)
            answer = self._exchange(hello, time.monotonic() + config.connect_timeout)
            if answer.kind != codec.R_HELLO:
                raise _lost("unexpected answer to the greeting")
        except BaseException:
            self.close()
            raise

    def close(self) -> None:
        self.broken = True
        try:
            self.sock.close()
        except OSError:
            pass

    def _read(self, n: int, deadline: float) -> bytes:
        chunks: List[bytes] = []
        while n:
            left = deadline - time.monotonic()
            if left <= 0:
                raise _lost("no answer in time")
            self.sock.settimeout(min(left, _LONGEST_WAIT))
            try:
                chunk = self.sock.recv(min(n, 1 << 20))
            except socket.timeout:
                if left > _LONGEST_WAIT:
                    continue
                raise _lost("no answer in time") from None
            except OSError as e:
                raise _lost(e) from None
            if not chunk:
                raise _lost("the server closed the connection")
            chunks.append(chunk)
            n -= len(chunk)
        return b"".join(chunks)

    def send(self, request: ast.Request, deadline: Optional[float] = None) -> None:
        payload = codec.encode_request(request, self.statement_timeout_ms)
        if len(payload) > codec.MAX_FRAME:
            raise DbError(ErrorCode.INVALID_REQUEST, f"a request of {len(payload)} bytes is larger than the {codec.MAX_FRAME} allowed")
        if self.broken:
            # An earlier call that was abandoned midway left its answer
            # unread: the next answer read would be that one.
            raise _lost("an earlier request on this connection was abandoned before its answer was read")
        self.broken = True
        self.sock.settimeout(min(max(0.001, (deadline or time.monotonic() + self.patience) - time.monotonic()), _LONGEST_WAIT))
        try:
            self.sock.sendall(struct.pack("<I", len(payload)) + payload)
        except socket.timeout:
            raise _lost("the request could not be sent in time") from None
        except OSError as e:
            raise _lost(e) from None

    def receive(self, deadline: Optional[float] = None) -> ast.Response:
        """Reads one frame. The connection stays marked broken until the
        caller has read the last frame of the answer and calls ``settle``."""
        if deadline is None:
            deadline = time.monotonic() + self.patience
        (length,) = struct.unpack("<I", self._read(4, deadline))
        if length > codec.MAX_FRAME:
            raise _lost("the server sent a frame larger than allowed")
        try:
            return codec.decode_response(self._read(length, deadline))
        except codec.ProtocolError as e:
            raise _lost(e.message) from None

    def settle(self) -> None:
        self.broken = False
        self.last_used = time.monotonic()

    def _exchange(self, request: ast.Request, deadline: float) -> ast.Response:
        self.send(request, deadline)
        merged: Optional[ast.RowSet] = None
        while True:
            response = self.receive(deadline)
            if response.kind == codec.R_ROWS:
                rows: ast.RowSet = response.value
                if merged is not None:
                    # A result in several frames: the first frame's head,
                    # every frame's rows.
                    merged.rows.extend(rows.rows)
                    rows = merged
                if response.more:
                    merged = rows
                    continue
                self.settle()
                return ast.Response(codec.R_ROWS, rows)
            self.settle()
            if response.kind == codec.R_ERROR:
                raise response.value
            return response

    def call(self, request: ast.Request) -> ast.Response:
        """Sends a request and reads its whole answer. An error from the
        server is raised and leaves the connection usable."""
        return self._exchange(request, time.monotonic() + self.patience)


class Lease:
    """A connection out of the pool. Released, it goes back, unless it is
    broken or still holds a transaction or a lock, in which case it is
    closed and the server undoes what it held."""

    def __init__(self, pool: "Pool", conn: Conn, generation: int) -> None:
        self._pool = pool
        self._generation = generation
        self.conn: Optional[Conn] = conn
        #: The server holds state for this connection: an open
        #: transaction or a named lock.
        self.held = False

    def release(self) -> None:
        conn, self.conn = self.conn, None
        if conn is not None:
            self._pool._release(conn, not conn.broken and not self.held, self._generation)

    def __enter__(self) -> Conn:
        assert self.conn is not None
        return self.conn

    def __exit__(self, *exc: Any) -> None:
        self.release()

    def __del__(self) -> None:
        # A transaction or a lock guard that was simply forgotten.
        try:
            self.release()
        except Exception:
            pass


class Pool:
    def __init__(self, config: Config) -> None:
        self.config = config
        self._idle: List[Conn] = []
        # Reentrant: a forgotten lease is given back by its finalizer,
        # which the garbage collector may run on a thread that is inside
        # the pool already, holding this lock.
        self._lock = threading.RLock()
        self._freed = threading.Condition(self._lock)
        #: Connections that may still be leased.
        self._free = max(1, config.pool_size)
        #: Counts the calls of ``clear``: a connection leased before one
        #: is closed when it comes back.
        self._generation = 0

    def acquire(self) -> Lease:
        with self._lock:
            while not self._free:
                # A finalizer that gives a lease back while this thread
                # is getting ready to wait wakes nobody: look again now
                # and then.
                self._freed.wait(1.0)
            self._free -= 1
        try:
            while True:
                with self._lock:
                    conn = self._idle.pop() if self._idle else None
                    generation = self._generation
                if conn is None:
                    return Lease(self, Conn(self.config), generation)
                if time.monotonic() - conn.last_used > _IDLE_CHECK_SECONDS:
                    try:
                        conn.call(codec.request(codec.PING))
                    except DbError:
                        conn.close()
                        continue
                return Lease(self, conn, generation)
        except BaseException:
            self._give_slot()
            raise

    def _give_slot(self) -> None:
        with self._lock:
            self._free += 1
            self._freed.notify()

    def _release(self, conn: Conn, keep: bool, generation: int) -> None:
        with self._lock:
            keep = keep and generation == self._generation
            if keep:
                self._idle.append(conn)
            self._free += 1
            self._freed.notify()
        if not keep:
            conn.close()

    def clear(self) -> None:
        """Closes the idle connections. Ones in use close when released."""
        with self._lock:
            self._generation += 1
            idle, self._idle = self._idle, []
        for conn in idle:
            conn.close()
