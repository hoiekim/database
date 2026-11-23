//! What the client and the server share: the values a column can hold,
//! table definitions, the expressions and queries a client builds, and how
//! all of it is written as bytes.

pub mod codec;
pub mod decimal;
pub mod error;
pub mod expr;
pub mod message;
pub mod query;
pub mod schema;
pub mod time;
pub mod value;
