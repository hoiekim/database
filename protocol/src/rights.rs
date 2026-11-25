//! What a role may do to a table.
//!
//! Five rights, each a bit. They are the five things a statement can do
//! to a table that differ in what they risk:
//!
//! - `READ`: be shown what its rows hold, by any way a statement can
//!   learn it: a query, a join, a subquery, `returning`, the rows an
//!   update or a delete picks, a foreign key's check.
//! - `INSERT`: add rows. A role with this alone can file what it has
//!   (an event, an order) and neither see nor change what others filed.
//! - `UPDATE`: change the rows there are.
//! - `DELETE`: remove them. Apart from `UPDATE`, because a table that
//!   may be corrected is often one that may not be emptied.
//! - `SCHEMA`: change what the table is: its columns and indexes, and
//!   whether it exists.
//!
//! There is no right to grant rights: a database's own password does
//! that, and a role never does.

use crate::error::{DbError, DbResult};

/// A set of rights.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rights(u8);

impl Rights {
    pub const NONE: Rights = Rights(0);
    pub const READ: Rights = Rights(1);
    pub const INSERT: Rights = Rights(2);
    pub const UPDATE: Rights = Rights(4);
    pub const DELETE: Rights = Rights(8);
    pub const SCHEMA: Rights = Rights(16);
    /// Inserting, updating and deleting: every change to rows.
    pub const WRITE: Rights = Rights(2 | 4 | 8);
    pub const ALL: Rights = Rights(31);

    /// Each right with its name, in the order of their bits.
    pub const NAMED: [(Rights, &'static str); 5] = [(Rights::READ, "read"), (Rights::INSERT, "insert"), (Rights::UPDATE, "update"), (Rights::DELETE, "delete"), (Rights::SCHEMA, "schema")];

    pub fn bits(self) -> u8 {
        self.0
    }

    /// The set a byte stands for. A bit that is no right is refused: it
    /// may be one a later version gives a meaning to.
    pub fn from_bits(bits: u8) -> DbResult<Rights> {
        if bits & !Rights::ALL.0 != 0 {
            return Err(DbError::invalid(format!("{bits} names a right that does not exist")));
        }
        Ok(Rights(bits))
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every right of `other` is in this set.
    pub fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any right of `other` is in this set.
    pub fn intersects(self, other: Rights) -> bool {
        self.0 & other.0 != 0
    }

    /// This set without the rights of `other`.
    pub fn without(self, other: Rights) -> Rights {
        Rights(self.0 & !other.0)
    }

    /// The names of the rights in the set.
    pub fn names(self) -> Vec<&'static str> {
        Rights::NAMED.iter().filter(|(right, _)| self.contains(*right)).map(|(_, name)| *name).collect()
    }

    /// Reads names separated by commas: the five rights, `write` for the
    /// three that change rows, `all` for every one.
    pub fn parse(text: &str) -> DbResult<Rights> {
        let mut rights = Rights::NONE;
        for word in text.split(',').map(str::trim) {
            rights = rights
                | match word {
                    "write" => Rights::WRITE,
                    "all" => Rights::ALL,
                    other => Rights::NAMED.iter().find(|(_, name)| *name == other).map(|(right, _)| *right).ok_or_else(|| DbError::invalid(format!("{other:?} is not a right: the rights are read, insert, update, delete and schema, with write for the middle three and all for every one")))?,
                };
        }
        Ok(rights)
    }
}

impl std::ops::BitOr for Rights {
    type Output = Rights;
    fn bitor(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }
}

impl std::fmt::Display for Rights {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.names().join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_are_read_from_their_names_and_written_back() {
        assert_eq!(Rights::parse("read").unwrap(), Rights::READ);
        assert_eq!(Rights::parse("read, insert").unwrap(), Rights::READ | Rights::INSERT);
        assert_eq!(Rights::parse("write").unwrap(), Rights::INSERT | Rights::UPDATE | Rights::DELETE);
        assert_eq!(Rights::parse("all").unwrap(), Rights::ALL);
        assert_eq!(Rights::ALL.to_string(), "read, insert, update, delete, schema");
        assert_eq!(Rights::parse("schema,read").unwrap().names(), ["read", "schema"]);
        assert!(Rights::parse("drop").is_err() && Rights::parse("").is_err());
    }

    #[test]
    fn a_bit_that_is_no_right_is_refused() {
        for bits in 0..=u8::MAX {
            assert_eq!(Rights::from_bits(bits).is_ok(), bits < 32, "{bits}");
        }
        assert!(Rights::ALL.contains(Rights::WRITE) && !Rights::WRITE.contains(Rights::READ));
        assert_eq!(Rights::ALL.without(Rights::WRITE), Rights::READ | Rights::SCHEMA);
        assert!(Rights::WRITE.intersects(Rights::DELETE | Rights::READ) && !Rights::READ.intersects(Rights::WRITE));
    }
}
