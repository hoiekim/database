//! Errors inside the engine. `Db` carries the ones a client is meant to
//! see and act on (a broken constraint, an unknown table); the rest mean
//! the disk or the files failed.

use database_protocol::error::{DbError, ErrorCode};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The files do not hold what the engine wrote: a checksum or a
    /// structure does not add up.
    Corrupt(String),
    /// An index key longer than a tree page can hold.
    KeyTooLong(usize),
    Db(DbError),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn corrupt(what: impl Into<String>) -> Self {
        Error::Corrupt(what.into())
    }

    pub fn db(code: ErrorCode, message: impl Into<String>) -> Self {
        Error::Db(DbError::new(code, message))
    }

    /// The error as a client sees it.
    pub fn into_db(self) -> DbError {
        match self {
            Error::Db(e) => e,
            Error::Io(e) => DbError::new(ErrorCode::Io, e.to_string()),
            Error::Corrupt(what) => DbError::new(ErrorCode::Corrupt, what),
            Error::KeyTooLong(len) => DbError::new(
                ErrorCode::KeyTooLong,
                format!("an index key of {len} bytes is longer than the {} allowed", crate::btree::MAX_KEY),
            ),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Corrupt(what) => write!(f, "corrupt database: {what}"),
            Error::KeyTooLong(len) => write!(f, "index key of {len} bytes is too long"),
            Error::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<DbError> for Error {
    fn from(e: DbError) -> Self {
        Error::Db(e)
    }
}
