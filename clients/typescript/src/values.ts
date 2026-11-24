// How JavaScript values stand for the database's values.
//
// | Database  | Written as                                   | Read as                          |
// |-----------|----------------------------------------------|----------------------------------|
// | NULL      | `null`                                       | `null`                           |
// | bool      | `boolean`                                    | `boolean`                        |
// | int       | `bigint`, or a `number` that is a safe integer | `number` if safe, else `bigint` |
// | float     | any other `number`, or `float(x)`            | `number`                         |
// | decimal   | `Decimal`                                    | `Decimal`                        |
// | text      | `string`                                     | `string`                         |
// | bytes     | `Uint8Array` (a `Buffer` is one)             | `Uint8Array`                     |
// | uuid      | `Uuid`                                       | `Uuid`                           |
// | timestamp | `Timestamp`, or a `Date`                     | `Timestamp`                      |
// | date      | `CalendarDate`                               | `CalendarDate`                   |
// | json      | a plain object, or `json(value)`             | the parsed value                 |
// | list      | an array                                     | an array                         |
//
// A 64-bit integer does not fit a JavaScript number, so the rule for
// integers is stated here once: a `number` is written as an int exactly
// when `Number.isSafeInteger` says so, and as a float otherwise; a
// `bigint` is always an int. An int is read as a `number` when it is a
// safe integer, and as a `bigint` when it is not. `Row.int` and
// `Row.bigint` read a column as one or the other without the question.

/** The most digits a decimal keeps after the point. */
export const MAX_SCALE = 30;
/** The most digits of a decimal read from text that are kept in all. */
const MAX_DIGITS = 37;

const I64_MIN = -(1n << 63n);
const I64_MAX = (1n << 63n) - 1n;
const I128_MAX = (1n << 127n) - 1n;

/** A lone surrogate, which UTF-8 cannot carry. */
const LONE_SURROGATE = /\p{Cs}/u;

/**
 * A value exactly as the wire carries it, kind and all. `toWire` and
 * `fromWire` go between this and the JavaScript values above.
 */
export type WireValue =
  | { kind: "null" }
  | { kind: "bool"; value: boolean }
  | { kind: "int"; value: bigint }
  | { kind: "float"; value: number }
  | { kind: "decimal"; mantissa: bigint; scale: number }
  | { kind: "text"; value: string }
  | { kind: "bytes"; value: Uint8Array }
  | { kind: "uuid"; value: Uint8Array }
  | { kind: "timestamp"; micros: bigint }
  | { kind: "date"; days: number }
  | { kind: "json"; text: string }
  | { kind: "list"; items: WireValue[] };

const HEX = "0123456789abcdef";

function hex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += HEX[b >> 4]! + HEX[b & 15]!;
  return out;
}

/** A uuid: 16 bytes, printed in the usual five groups. */
export class Uuid {
  readonly bytes: Uint8Array;

  constructor(bytes: Uint8Array) {
    if (bytes.length !== 16) throw new RangeError(`a uuid is 16 bytes, not ${bytes.length}`);
    this.bytes = Uint8Array.from(bytes);
  }

  /** Reads the 36-character form or 32 hex digits, in either case. */
  static parse(text: string): Uuid {
    const digits = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(text) ? text.replaceAll("-", "") : text;
    if (!/^[0-9a-f]{32}$/i.test(digits)) throw new SyntaxError(`not a uuid: ${JSON.stringify(text)}`);
    const bytes = new Uint8Array(16);
    for (let i = 0; i < 16; i++) bytes[i] = parseInt(digits.slice(i * 2, i * 2 + 2), 16);
    return new Uuid(bytes);
  }

  equals(other: Uuid): boolean {
    return this.bytes.every((b, i) => b === other.bytes[i]);
  }

  toString(): string {
    const h = hex(this.bytes);
    return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
  }

  toJSON(): string {
    return this.toString();
  }
}

/**
 * A point in time as the database holds it: microseconds since
 * 1970-01-01T00:00:00Z. A JavaScript `Date` keeps milliseconds only, so
 * a timestamp read from the server is one of these: given back as a key
 * or a cursor it is the same instant, to the microsecond.
 */
export class Timestamp {
  readonly micros: bigint;

  constructor(micros: bigint | number) {
    this.micros = BigInt(micros);
    if (this.micros < I64_MIN || this.micros > I64_MAX) throw new RangeError("a timestamp is a signed 64-bit count of microseconds");
  }

  static fromDate(date: Date): Timestamp {
    const ms = date.getTime();
    if (Number.isNaN(ms)) throw new RangeError("an invalid Date has no timestamp");
    return new Timestamp(BigInt(ms) * 1000n);
  }

  static fromMillis(ms: number): Timestamp {
    return new Timestamp(BigInt(Math.trunc(ms)) * 1000n);
  }

  static now(): Timestamp {
    return Timestamp.fromMillis(Date.now());
  }

  /** The millisecond the instant falls in. Throws beyond what a `Date` can hold. */
  toDate(): Date {
    const date = new Date(Number(this.millis()));
    if (Number.isNaN(date.getTime())) throw new RangeError("the timestamp is beyond what a Date can hold");
    return date;
  }

  /** Milliseconds since the epoch, rounded down. */
  millis(): bigint {
    return this.micros / 1000n - (this.micros % 1000n < 0n ? 1n : 0n);
  }

  /**
   * RFC 3339 in UTC with six fraction digits, `2026-03-05T10:20:30.123456Z`,
   * as the server prints it: also for years before 1 and after 9999,
   * which a `Date` prints otherwise or not at all.
   */
  toISOString(): string {
    let days = this.micros / DAY_MICROS;
    let rest = this.micros % DAY_MICROS;
    if (rest < 0n) {
      rest += DAY_MICROS;
      days -= 1n;
    }
    const secs = Number(rest / 1_000_000n);
    const two = (n: number): string => String(n).padStart(2, "0");
    const fraction = (rest % 1_000_000n).toString().padStart(6, "0");
    return `${formatDay(Number(days))}T${two(Math.floor(secs / 3600))}:${two(Math.floor(secs / 60) % 60)}:${two(secs % 60)}.${fraction}Z`;
  }

  equals(other: Timestamp): boolean {
    return this.micros === other.micros;
  }

  toString(): string {
    return this.toISOString();
  }

  toJSON(): string {
    return this.toISOString();
  }
}

const DAY_MS = 86_400_000;
const DAY_MICROS = 86_400_000_000n;

/**
 * The calendar day of a count of days since 1970-01-01, proleptic
 * Gregorian, for any day a date holds: Howard Hinnant's
 * `civil_from_days`, as `protocol/src/time.rs` has it.
 */
function civilFromDays(days: number): [year: number, month: number, day: number] {
  const z = days + 719_468;
  const era = Math.floor(z / 146_097);
  const doe = z - era * 146_097;
  const yoe = Math.floor((doe - Math.floor(doe / 1460) + Math.floor(doe / 36_524) - Math.floor(doe / 146_096)) / 365);
  const doy = doe - (365 * yoe + Math.floor(yoe / 4) - Math.floor(yoe / 100));
  const mp = Math.floor((5 * doy + 2) / 153);
  const day = doy - Math.floor((153 * mp + 2) / 5) + 1;
  const month = mp < 10 ? mp + 3 : mp - 9;
  return [yoe + era * 400 + (month <= 2 ? 1 : 0), month, day];
}

/** `YYYY-MM-DD` as Rust's `{:04}` writes the year: `-007`, `10000`. */
function formatDay(days: number): string {
  const [y, m, d] = civilFromDays(days);
  const year = y < 0 ? `-${String(-y).padStart(3, "0")}` : String(y).padStart(4, "0");
  return `${year}-${String(m).padStart(2, "0")}-${String(d).padStart(2, "0")}`;
}

/** A calendar day as the database holds it: days since 1970-01-01. */
export class CalendarDate {
  readonly days: number;

  constructor(days: number) {
    if (!Number.isInteger(days) || days < -(2 ** 31) || days >= 2 ** 31) throw new RangeError("a date is a signed 32-bit count of days");
    this.days = days;
  }

  /** A day of the calendar; throws for one that does not exist (30 February). */
  static fromYmd(year: number, month: number, day: number): CalendarDate {
    const date = new Date(0);
    date.setUTCFullYear(year, month - 1, day);
    if (date.getUTCFullYear() !== year || date.getUTCMonth() !== month - 1 || date.getUTCDate() !== day) {
      throw new RangeError(`there is no day ${year}-${month}-${day}`);
    }
    return new CalendarDate(Math.round(date.getTime() / DAY_MS));
  }

  /** Reads `YYYY-MM-DD`. */
  static parse(text: string): CalendarDate {
    const parts = /^(\d{4})-(\d{2})-(\d{2})$/.exec(text);
    if (!parts) throw new SyntaxError(`not a date: ${JSON.stringify(text)}`);
    return CalendarDate.fromYmd(Number(parts[1]), Number(parts[2]), Number(parts[3]));
  }

  /** The day a `Date` falls on in UTC. */
  static fromDate(date: Date): CalendarDate {
    return new CalendarDate(Math.floor(date.getTime() / DAY_MS));
  }

  /** Midnight UTC of the day. */
  toDate(): Date {
    return new Date(this.days * DAY_MS);
  }

  ymd(): [year: number, month: number, day: number] {
    return civilFromDays(this.days);
  }

  equals(other: CalendarDate): boolean {
    return this.days === other.days;
  }

  /** `YYYY-MM-DD`, as the server prints it. */
  toString(): string {
    return formatDay(this.days);
  }

  toJSON(): string {
    return this.toString();
  }
}

/**
 * An exact decimal number: `mantissa / 10^scale`. Sums of money come out
 * to the cent, which a JavaScript number does not promise.
 */
export class Decimal {
  readonly mantissa: bigint;
  readonly scale: number;

  constructor(mantissa: bigint, scale = 0) {
    if (!Number.isInteger(scale) || scale < 0 || scale > MAX_SCALE) throw new RangeError(`a decimal keeps 0 to ${MAX_SCALE} digits after the point`);
    if (mantissa > I128_MAX || mantissa < -I128_MAX) throw new RangeError("a decimal's digits must fit 127 bits");
    this.mantissa = mantissa;
    this.scale = scale;
  }

  /**
   * Reads `-12.34`, `.5`, `1e3`, `2.5E-4` as `protocol/src/decimal.rs`
   * does: digits after the point beyond what can be kept are rounded
   * away, halves away from zero; a whole part beyond 127 bits is a
   * `RangeError`.
   */
  static parse(text: string): Decimal {
    const parts = /^([+-]?)(\d*)(?:\.(\d*))?(?:[eE]([+-]?\d+))?$/.exec(text.trim());
    const whole = parts?.[2] ?? "";
    let fraction = parts?.[3] ?? "";
    if (!parts || whole.length + fraction.length === 0) throw new SyntaxError(`not a decimal number: ${JSON.stringify(text)}`);
    const tooLarge = (): RangeError => new RangeError(`${text.trim()} is beyond what a decimal holds`);
    const exponent = Number(parts[4] ?? 0);
    if (!(Math.abs(exponent) < 2 ** 31)) throw tooLarge();
    // Room after the point: 37 digits in all, at most MAX_SCALE of them there.
    const room = Math.min(Math.max(0, MAX_DIGITS - whole.replace(/^0+/, "").length), MAX_SCALE);
    const roundUp = fraction.length > room && fraction.charCodeAt(room) >= 0x35;
    fraction = fraction.slice(0, room);
    const digits = (whole + fraction).replace(/^0+/, "");
    if (digits.length > 39) throw tooLarge();
    let mantissa = BigInt(digits || "0") + (roundUp ? 1n : 0n);
    let scale = fraction.length - exponent;
    if (scale < 0) {
      // 10^38 is the largest power of ten 127 bits hold.
      if (-scale > 38) throw tooLarge();
      mantissa *= 10n ** BigInt(-scale);
      scale = 0;
    }
    if (mantissa > I128_MAX) throw tooLarge();
    if (scale > 255) throw tooLarge();
    if (scale > MAX_SCALE) {
      // Rounded to MAX_SCALE digits, halves away from zero.
      const excess = scale - MAX_SCALE;
      if (excess > 38) {
        mantissa = 0n;
      } else {
        const divisor = 10n ** BigInt(excess);
        mantissa = mantissa / divisor + ((mantissa % divisor) * 2n >= divisor ? 1n : 0n);
      }
      scale = MAX_SCALE;
    }
    return new Decimal(parts[1] === "-" ? -mantissa : mantissa, scale);
  }

  /** The nearest number: for display and arithmetic that need not be exact. */
  toNumber(): number {
    return Number(this.toString());
  }

  equals(other: Decimal): boolean {
    const scale = Math.max(this.scale, other.scale);
    return this.mantissa * 10n ** BigInt(scale - this.scale) === other.mantissa * 10n ** BigInt(scale - other.scale);
  }

  /** Every digit, with exactly `scale` of them after the point. */
  toString(): string {
    const negative = this.mantissa < 0n;
    const digits = (negative ? -this.mantissa : this.mantissa).toString().padStart(this.scale + 1, "0");
    const sign = negative ? "-" : "";
    if (this.scale === 0) return sign + digits;
    return `${sign}${digits.slice(0, digits.length - this.scale)}.${digits.slice(digits.length - this.scale)}`;
  }

  toJSON(): string {
    return this.toString();
  }
}

/** A number to be written as a float even though it is whole: `float(1)`. */
export class Float {
  constructor(readonly value: number) {}
}

/** A value to be stored as JSON: `json([1, 2])`, `json("text")`. */
export class Json {
  constructor(readonly value: unknown) {}
}

export function float(value: number): Float {
  return new Float(value);
}

export function json(value: unknown): Json {
  return new Json(value);
}

/** What may be given where the database expects a value. */
export type Value =
  | null
  | boolean
  | number
  | bigint
  | string
  | Uint8Array
  | Uuid
  | Timestamp
  | Date
  | CalendarDate
  | Decimal
  | Float
  | Json
  | readonly Value[]
  | { readonly [key: string]: unknown };

function jsonText(value: unknown): string {
  // `JSON.stringify` writes half a surrogate pair as an escape, which is
  // not text either: refused here, as in `text`.
  const text = JSON.stringify(value, function (key, item: unknown) {
    if (LONE_SURROGATE.test(key) || (typeof item === "string" && LONE_SURROGATE.test(item))) {
      throw new RangeError(`${JSON.stringify(typeof item === "string" ? item : key)} holds half of a surrogate pair, which is not text`);
    }
    return item;
  });
  if (text === undefined) throw new TypeError("that value has no JSON form");
  return text;
}

/** The wire's form of a JavaScript value. Throws `TypeError` for one the database has no kind for. */
export function toWire(value: Value): WireValue {
  if (value === null) return { kind: "null" };
  switch (typeof value) {
    case "boolean":
      return { kind: "bool", value };
    case "bigint":
      if (value < I64_MIN || value > I64_MAX) throw new RangeError(`${value} does not fit the database's 64-bit integers`);
      return { kind: "int", value };
    case "number":
      return Number.isSafeInteger(value) ? { kind: "int", value: BigInt(value) } : { kind: "float", value };
    case "string":
      return { kind: "text", value };
    case "object":
      break;
    default:
      throw new TypeError(`the database has no value for a ${typeof value}`);
  }
  if (value instanceof Uint8Array) return { kind: "bytes", value };
  if (value instanceof Uuid) return { kind: "uuid", value: value.bytes };
  if (value instanceof Timestamp) return { kind: "timestamp", micros: value.micros };
  if (value instanceof Date) return { kind: "timestamp", micros: Timestamp.fromDate(value).micros };
  if (value instanceof CalendarDate) return { kind: "date", days: value.days };
  if (value instanceof Decimal) return { kind: "decimal", mantissa: value.mantissa, scale: value.scale };
  if (value instanceof Float) return { kind: "float", value: value.value };
  if (value instanceof Json) return { kind: "json", text: jsonText(value.value) };
  if (Array.isArray(value)) return { kind: "list", items: (value as readonly Value[]).map(toWire) };
  const prototype = Object.getPrototypeOf(value);
  if (prototype === Object.prototype || prototype === null) return { kind: "json", text: jsonText(value) };
  throw new TypeError(`the database has no value for a ${(value as object).constructor?.name ?? "object"}; wrap it in json() to store it as JSON`);
}

/** What a value read from the server is in JavaScript. */
export type ReadValue = null | boolean | number | bigint | string | Uint8Array | Uuid | Timestamp | CalendarDate | Decimal | ReadValue[] | JsonValue;

/** A parsed JSON value. */
export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

/** The JavaScript value of a wire value. */
export function fromWire(value: WireValue): ReadValue {
  switch (value.kind) {
    case "null":
      return null;
    case "bool":
    case "float":
    case "text":
    case "bytes":
      return value.value;
    case "int":
      return value.value >= BigInt(Number.MIN_SAFE_INTEGER) && value.value <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(value.value) : value.value;
    case "decimal":
      return new Decimal(value.mantissa, value.scale);
    case "uuid":
      return new Uuid(value.value);
    case "timestamp":
      return new Timestamp(value.micros);
    case "date":
      return new CalendarDate(value.days);
    case "json":
      return JSON.parse(value.text) as JsonValue;
    case "list":
      return value.items.map(fromWire);
  }
}
