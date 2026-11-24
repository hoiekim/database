"""How Python values stand for the database's values.

==========  =========================================================
Database    Python
==========  =========================================================
NULL        ``None``
bool        ``bool``
int         ``int`` (64-bit, signed; a larger one is refused)
float       ``float``
decimal     ``decimal.Decimal`` (exact; at most 30 digits after the
            point, and a mantissa that fits 127 bits)
text        ``str``
bytes       ``bytes`` (``bytearray`` and ``memoryview`` are accepted)
uuid        ``uuid.UUID``
timestamp   ``datetime.datetime``, always UTC when read; see below
date        ``datetime.date``
json        any JSON value when read; ``Json(value)`` or a ``dict``
            when written
list        ``list`` (a ``tuple`` is accepted)
==========  =========================================================

**Timestamps** are microseconds since 1970-01-01T00:00:00Z. One read from
the server is a ``datetime`` with ``tzinfo=timezone.utc``. One written may
be in any zone and is converted exactly; a ``datetime`` without a zone is
taken to be UTC, not local time. The database holds timestamps and dates
far outside the years 1 to 9999 that ``datetime`` can express: those are
read as ``Timestamp(micros)`` and ``Date(days)``, which may also be
written.

**JSON** has to be told from the other kinds when written: a ``str`` is
text and a ``list`` is a list, so a JSON string or array is written as
``Json("text")`` or ``Json([1, 2])``. A ``dict`` can only be JSON and
needs no wrapper. Read back, a JSON value is the plain parsed value.
"""

from __future__ import annotations

import datetime as _dt
import decimal as _decimal
import uuid as _uuid
from dataclasses import dataclass
from typing import Any, Dict, List, Tuple, Union

#: The most digits a decimal keeps after the point.
MAX_SCALE = 30
_I64_MIN, _I64_MAX = -(1 << 63), (1 << 63) - 1
_I128_MAX = (1 << 127) - 1

UTC = _dt.timezone.utc
EPOCH = _dt.datetime(1970, 1, 1, tzinfo=UTC)
_EPOCH_ORDINAL = _dt.date(1970, 1, 1).toordinal()


@dataclass(frozen=True)
class Json:
    """A value to be stored as JSON: ``Json([1, 2])``, ``Json("text")``."""

    value: Any


@dataclass(frozen=True, order=True)
class Timestamp:
    """A point in time as the database holds it: microseconds since
    1970-01-01T00:00:00Z. Read in place of a ``datetime`` only for a time
    outside the years ``datetime`` can express."""

    micros: int

    @staticmethod
    def from_datetime(moment: _dt.datetime) -> "Timestamp":
        """Exactly, to the microsecond. Without a zone, taken as UTC."""
        if moment.tzinfo is None:
            moment = moment.replace(tzinfo=UTC)
        delta = moment - EPOCH
        return Timestamp((delta.days * 86_400 + delta.seconds) * 1_000_000 + delta.microseconds)

    def to_datetime(self) -> _dt.datetime:
        """Raises ``OverflowError`` outside the years 1 to 9999."""
        return EPOCH + _dt.timedelta(microseconds=self.micros)


@dataclass(frozen=True, order=True)
class Date:
    """A calendar day as the database holds it: days since 1970-01-01.
    Read in place of a ``date`` only for a day outside the years ``date``
    can express."""

    days: int

    @staticmethod
    def from_date(day: _dt.date) -> "Date":
        return Date(day.toordinal() - _EPOCH_ORDINAL)

    def to_date(self) -> _dt.date:
        """Raises ``OverflowError`` or ``ValueError`` outside the years 1 to 9999."""
        return _dt.date.fromordinal(self.days + _EPOCH_ORDINAL)


#: What may be given where the database expects a value.
Value = Union[None, bool, int, float, _decimal.Decimal, str, bytes, bytearray, memoryview, _uuid.UUID, _dt.datetime, _dt.date, Timestamp, Date, Json, Dict[str, Any], List[Any], Tuple[Any, ...]]


def decimal_parts(number: _decimal.Decimal) -> Tuple[int, int]:
    """A decimal as the wire carries it: ``mantissa / 10**scale``."""
    sign, digits, exponent = number.as_tuple()
    if not isinstance(exponent, int):
        raise ValueError("a decimal cannot hold NaN or infinity")
    mantissa = int("".join(map(str, digits)) or "0")
    scale = 0
    if exponent > 0:
        mantissa *= 10**exponent
    else:
        scale = -exponent
    if scale > MAX_SCALE:
        raise ValueError(f"a decimal keeps at most {MAX_SCALE} digits after the point; {number} has {scale}")
    if mantissa > _I128_MAX:
        raise ValueError(f"{number} has more digits than a decimal can hold")
    return (-mantissa if sign else mantissa, scale)


def decimal_from_parts(mantissa: int, scale: int) -> _decimal.Decimal:
    """Exact whatever the decimal context's precision."""
    digits = tuple(int(c) for c in str(abs(mantissa)))
    return _decimal.Decimal((1 if mantissa < 0 else 0, digits, -scale))


def check_int(number: int) -> int:
    if not _I64_MIN <= number <= _I64_MAX:
        raise ValueError(f"{number} does not fit the database's 64-bit integers")
    return number
