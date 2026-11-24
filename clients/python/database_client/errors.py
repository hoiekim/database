"""The one error a caller sees, and its codes."""

from __future__ import annotations

from enum import IntEnum
from typing import Optional


class ErrorCode(IntEnum):
    """What went wrong, in a form code can branch on."""

    #: A unique index already holds the key. ``DbError.constraint`` names it.
    UNIQUE_VIOLATION = 1
    #: A row points at a parent that is not there, or a parent still has
    #: children that forbid its removal.
    FOREIGN_KEY_VIOLATION = 2
    #: NULL in a column that does not allow it.
    NOT_NULL_VIOLATION = 3
    #: A row fails one of its table's checks.
    CHECK_VIOLATION = 4
    #: A value of the wrong type for its column or for an operator.
    TYPE_MISMATCH = 5
    #: No such database, table, column or index.
    NOT_FOUND = 6
    #: A database, table, column or index of that name is already there.
    ALREADY_EXISTS = 7
    #: The request makes no sense as written.
    INVALID_REQUEST = 8
    #: An index key longer than the engine can store.
    KEY_TOO_LONG = 9
    #: Waited too long for the write lock or a named lock.
    LOCK_TIMEOUT = 10
    #: The statement ran past its deadline.
    TIMEOUT = 11
    #: The transaction was already ended, by an error or by the server.
    TRANSACTION_ABORTED = 12
    #: Wrong name or password, or no right to the database.
    ACCESS_DENIED = 13
    #: The client and the server do not speak the same protocol version.
    PROTOCOL_MISMATCH = 14
    #: The connection broke. Whether the last request took effect is not
    #: known.
    CONNECTION = 15
    #: The disk failed under the server.
    IO = 16
    #: The server found its own files damaged.
    CORRUPT = 17
    #: The server is shutting down or cannot take the request now.
    UNAVAILABLE = 18
    #: A bug in the server.
    INTERNAL = 19
    #: Arithmetic that has no answer: division by zero, overflow.
    ARITHMETIC = 20
    #: A number too large for its column.
    OUT_OF_RANGE = 21
    #: An optimistic transaction was told something that a commit made
    #: beside it has since changed. Nothing of it took effect: run it again.
    CONFLICT = 22

    @staticmethod
    def from_wire(number: int) -> "ErrorCode":
        """The code a number on the wire stands for. One this version does
        not know reads as ``INTERNAL``, as the server's own decoder has it."""
        try:
            return ErrorCode(number)
        except ValueError:
            return ErrorCode.INTERNAL


class DbError(Exception):
    """An error from the server, or from the connection to it."""

    def __init__(self, code: ErrorCode, message: str, constraint: Optional[str] = None, table: Optional[str] = None) -> None:
        super().__init__(message if constraint is None else f"{message} ({constraint})")
        self.code = code
        self.message = message
        #: The index, column or check that was broken, for the constraint
        #: errors.
        self.constraint = constraint
        self.table = table

    def violates(self, constraint: str) -> bool:
        """Whether a constraint of this name was the one violated."""
        return self.constraint == constraint

    @property
    def is_unique_violation(self) -> bool:
        return self.code == ErrorCode.UNIQUE_VIOLATION

    def __repr__(self) -> str:
        return f"DbError({self.code.name}, {self.message!r}, constraint={self.constraint!r}, table={self.table!r})"

    def __eq__(self, other: object) -> bool:
        return (
            isinstance(other, DbError)
            and (self.code, self.message, self.constraint, self.table) == (other.code, other.message, other.constraint, other.table)
        )

    def __hash__(self) -> int:
        return hash((self.code, self.message, self.constraint, self.table))
