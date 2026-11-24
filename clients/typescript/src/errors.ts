// The one error a caller sees, and its codes.

/** What went wrong, in a form code can branch on. The numbers are the wire's. */
export const ErrorCode = {
  /** A unique index already holds the key. `DbError.constraint` names it. */
  UniqueViolation: 1,
  /** A row points at a parent that is not there, or a parent still has children that forbid its removal. */
  ForeignKeyViolation: 2,
  /** NULL in a column that does not allow it. */
  NotNullViolation: 3,
  /** A row fails one of its table's checks. */
  CheckViolation: 4,
  /** A value of the wrong type for its column or for an operator. */
  TypeMismatch: 5,
  /** No such database, table, column or index. */
  NotFound: 6,
  /** A database, table, column or index of that name is already there. */
  AlreadyExists: 7,
  /** The request makes no sense as written. */
  InvalidRequest: 8,
  /** An index key longer than the engine can store. */
  KeyTooLong: 9,
  /** Waited too long for the write lock or a named lock. */
  LockTimeout: 10,
  /** The statement ran past its deadline. */
  Timeout: 11,
  /** The transaction was already ended, by an error or by the server. */
  TransactionAborted: 12,
  /** Wrong name or password, or no right to the database. */
  AccessDenied: 13,
  /** The client and the server do not speak the same protocol version. */
  ProtocolMismatch: 14,
  /** The connection broke. Whether the last request took effect is not known. */
  Connection: 15,
  /** The disk failed under the server. */
  Io: 16,
  /** The server found its own files damaged. */
  Corrupt: 17,
  /** The server is shutting down or cannot take the request now. */
  Unavailable: 18,
  /** A bug in the server. */
  Internal: 19,
  /** Arithmetic that has no answer: division by zero, overflow. */
  Arithmetic: 20,
  /** A number too large for its column. */
  OutOfRange: 21,
  /**
   * An optimistic transaction was told something that a commit made
   * beside it has since changed. Nothing of it took effect: run it again.
   */
  Conflict: 22,
} as const;

export type ErrorCodeName = keyof typeof ErrorCode;
export type ErrorCode = (typeof ErrorCode)[ErrorCodeName];

const NAMES = new Map<number, ErrorCodeName>(
  (Object.entries(ErrorCode) as [ErrorCodeName, number][]).map(([name, code]) => [code, name]),
);

/**
 * The code a number on the wire stands for. One this version does not
 * know reads as `Internal`, as the server's own decoder has it.
 */
export function errorCodeFromWire(code: number): ErrorCode {
  return NAMES.has(code) ? (code as ErrorCode) : ErrorCode.Internal;
}

export function errorCodeName(code: ErrorCode): ErrorCodeName {
  return NAMES.get(code) ?? "Internal";
}

/** An error from the server, or from the connection to it. */
export class DbError extends Error {
  readonly code: ErrorCode;
  /** The index, column or check that was broken, for the constraint errors. */
  readonly constraint: string | null;
  readonly table: string | null;

  constructor(code: ErrorCode, message: string, constraint: string | null = null, table: string | null = null) {
    super(message);
    this.name = "DbError";
    this.code = code;
    this.constraint = constraint;
    this.table = table;
  }

  /** The code's name: `"UniqueViolation"`, `"Conflict"`, and so on. */
  get codeName(): ErrorCodeName {
    return errorCodeName(this.code);
  }

  /** Whether a constraint of this name was the one violated. */
  violates(constraint: string): boolean {
    return this.constraint === constraint;
  }

  get isUniqueViolation(): boolean {
    return this.code === ErrorCode.UniqueViolation;
  }
}
