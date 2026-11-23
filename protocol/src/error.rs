//! The one error type a client sees.

use std::fmt;

/// What went wrong, in a form code can branch on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ErrorCode {
    /// A unique index already holds the key. `DbError::constraint` names it.
    UniqueViolation = 1,
    /// A row points at a parent that is not there, or a parent still has
    /// children that forbid its removal.
    ForeignKeyViolation = 2,
    /// NULL in a column that does not allow it.
    NotNullViolation = 3,
    /// A row fails one of its table's checks.
    CheckViolation = 4,
    /// A value of the wrong type for its column or for an operator.
    TypeMismatch = 5,
    /// No such database, table, column or index.
    NotFound = 6,
    /// A database, table, column or index of that name is already there.
    AlreadyExists = 7,
    /// The request makes no sense as written.
    InvalidRequest = 8,
    /// An index key longer than the engine can store.
    KeyTooLong = 9,
    /// Waited too long for the write lock or a named lock.
    LockTimeout = 10,
    /// The statement ran past its deadline.
    Timeout = 11,
    /// The transaction was already ended, by an error or by the server.
    TransactionAborted = 12,
    /// Wrong name or password, or no right to the database.
    AccessDenied = 13,
    /// The client and the server do not speak the same protocol version.
    ProtocolMismatch = 14,
    /// The connection broke. Whether the last request took effect is not
    /// known.
    Connection = 15,
    /// The disk failed under the server.
    Io = 16,
    /// The server found its own files damaged.
    Corrupt = 17,
    /// The server is shutting down or cannot take the request now.
    Unavailable = 18,
    /// A bug in the server.
    Internal = 19,
    /// Arithmetic that has no answer: division by zero, overflow.
    Arithmetic = 20,
    /// A number too large for its column.
    OutOfRange = 21,
}

impl ErrorCode {
    pub fn from_u8(v: u8) -> ErrorCode {
        use ErrorCode::*;
        match v {
            1 => UniqueViolation,
            2 => ForeignKeyViolation,
            3 => NotNullViolation,
            4 => CheckViolation,
            5 => TypeMismatch,
            6 => NotFound,
            7 => AlreadyExists,
            8 => InvalidRequest,
            9 => KeyTooLong,
            10 => LockTimeout,
            11 => Timeout,
            12 => TransactionAborted,
            13 => AccessDenied,
            14 => ProtocolMismatch,
            15 => Connection,
            16 => Io,
            17 => Corrupt,
            18 => Unavailable,
            20 => Arithmetic,
            21 => OutOfRange,
            _ => Internal,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError {
    pub code: ErrorCode,
    pub message: String,
    /// The index or check that was broken, for the constraint errors.
    pub constraint: Option<String>,
    pub table: Option<String>,
}

impl DbError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        DbError { code, message: message.into(), constraint: None, table: None }
    }

    pub fn with_constraint(mut self, table: &str, constraint: &str) -> Self {
        self.table = Some(table.to_string());
        self.constraint = Some(constraint.to_string());
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        DbError::new(ErrorCode::InvalidRequest, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        DbError::new(ErrorCode::NotFound, message)
    }

    pub fn type_mismatch(message: impl Into<String>) -> Self {
        DbError::new(ErrorCode::TypeMismatch, message)
    }

    pub fn is_unique_violation(&self) -> bool {
        self.code == ErrorCode::UniqueViolation
    }

    /// Whether a unique index of this name was the one violated.
    pub fn violates(&self, constraint: &str) -> bool {
        self.constraint.as_deref() == Some(constraint)
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(c) = &self.constraint {
            write!(f, " ({c})")?;
        }
        Ok(())
    }
}

impl std::error::Error for DbError {}

pub type DbResult<T> = Result<T, DbError>;
