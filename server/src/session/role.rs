//! A database as one connection may use it: as its owner, or as one of
//! its roles.
//!
//! Every transaction a connection's statements run in is begun through
//! `As`, and a role's is begun as that role: the engine then holds each
//! statement to the role's rights at every table it comes to, by
//! whatever way (`Tx::need`, `exec::table_of`). So no path through the
//! session can run a role's statement unchecked without first going
//! around this type, and nothing here decides what a statement may do.
//!
//! The rights are those of the version the transaction is on. A
//! statement on its own, a batch and an exclusive transaction are on the
//! newest, so a revoke that was answered binds them. A reading or an
//! optimistic transaction is on the version it began at, and is ended if
//! its role has changed since: see `Session::role_still_stands`.

use std::sync::Arc;
use std::time::Duration;

use database_engine::catalog::Role;
use database_engine::db::{Database, ReadTxn, WriteTxn};

#[derive(Clone)]
pub(super) struct As {
    db: Arc<Database>,
    /// The role's name; `None` for the database's own password or the
    /// administrator's, which may do anything.
    role: Option<Arc<str>>,
}

impl As {
    pub(super) fn new(db: Arc<Database>, role: Option<Arc<str>>) -> As {
        As { db, role }
    }

    /// The database itself, for what is not a statement: its
    /// checkpoints, its checks, its roles.
    pub(super) fn database(&self) -> &Arc<Database> {
        &self.db
    }

    // The three ways a transaction begins, under the names the database
    // gives them, so that `db.begin_read()` on an `As` is this and never
    // the database's own.

    pub(super) fn begin_read(&self) -> ReadTxn {
        let mut txn = self.db.begin_read();
        txn.set_role(self.role.as_deref());
        txn
    }

    pub(super) fn begin_write(&self, wait: Duration, interactive: bool) -> database_engine::error::Result<WriteTxn> {
        let mut txn = self.db.begin_write(wait, interactive)?;
        txn.set_role(self.role.as_deref());
        Ok(txn)
    }

    pub(super) fn begin_fork(&self) -> database_engine::error::Result<WriteTxn> {
        let mut txn = self.db.begin_fork()?;
        txn.set_role(self.role.as_deref());
        Ok(txn)
    }

    /// The connection's role as the newest version readers see has it:
    /// `None` for a connection that is not a role's.
    pub(super) fn role_now(&self) -> Option<Arc<Role>> {
        self.role.as_ref()?;
        self.begin_read().role().cloned()
    }
}

/// What is not about transactions is the database's own.
impl std::ops::Deref for As {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
