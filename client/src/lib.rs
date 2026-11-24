//! The client for the database server.
//!
//! ```ignore
//! use database_client::*;
//!
//! let db = Client::connect_url("database://shop:secret@database:5454").await?;
//! db.create_table_if_not_exists(
//!     TableDef::new("users")
//!         .column(ColumnDef::new("id", Type::Uuid).default_random_uuid())
//!         .column(ColumnDef::new("username", Type::Text))
//!         .column(ColumnDef::new("created", Type::Timestamp).default_now())
//!         .primary_key(["id"])
//!         .unique("users_username_key", ["username"]),
//! ).await?;
//!
//! let id: Uuid = db
//!     .insert(Insert::into("users").set("username", "ann").returning(["id"]))
//!     .await?
//!     .one()?
//!     .get("id")?;
//! let ann = db.query_opt(Query::table("users").index("users_username_key", KeyRange::eq(["ann"]))).await?;
//! ```
//!
//! There is no SQL and no planner: a query says which key or index to
//! read and in what range, and the server does exactly that. See
//! `docs/CLIENT.md` in the repository for the common patterns.

mod client;
mod config;
mod conn;
mod row;

pub use client::{Admin, Batch, Client, Db, Explained, LockGuard, Prepared, Statement, Transaction};
pub use config::{Config, DEFAULT_PORT};
pub use row::{FromRow, Row, Rows};

pub use database_protocol::decimal::Decimal;
pub use database_protocol::error::{DbError, DbResult, ErrorCode};
pub use database_protocol::expr::{self, *};
pub use database_protocol::query::{Access, Assign, Bound, Delete, Direction, Insert, InsertSource, Join, JoinKind, KeyRange, OnConflict, Order, Query, Select, Update, ALL};
pub use database_protocol::schema::{Check, ColumnDef, ForeignKey, IndexDef, IndexKey, IndexKind, OnDelete, TableDef, TextConfig, TextField, PRIMARY};
pub use database_protocol::value::{Date, FromValue, Json, Timestamp, Type, Uuid, Value};
