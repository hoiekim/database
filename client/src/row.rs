//! What a statement returns.

use std::sync::Arc;

use database_protocol::error::{DbError, DbResult, ErrorCode};
use database_protocol::message::RowSet;
use database_protocol::value::{FromValue, Value};

/// The rows a statement returned, and how many rows it touched.
#[derive(Clone, Debug)]
pub struct Rows {
    columns: Arc<Vec<String>>,
    rows: Vec<Row>,
    affected: u64,
    total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    columns: Arc<Vec<String>>,
    values: Vec<Value>,
}

impl Rows {
    pub(crate) fn new(set: RowSet) -> Rows {
        let columns = Arc::new(set.columns);
        let rows = set.rows.into_iter().map(|values| Row { columns: columns.clone(), values }).collect();
        Rows { columns, rows, affected: set.affected, total: set.total }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Rows a write inserted, updated or deleted, whether or not it
    /// returned them; rows a read returned.
    pub fn affected(&self) -> u64 {
        self.affected
    }

    /// For a query built `with_total`: how many rows there are before
    /// offset and limit.
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Row> {
        self.rows.iter()
    }

    pub fn first(&self) -> Option<&Row> {
        self.rows.first()
    }

    pub fn into_vec(self) -> Vec<Row> {
        self.rows
    }

    /// The first row, if there is one.
    pub fn opt(self) -> Option<Row> {
        self.rows.into_iter().next()
    }

    /// The only row. An error if there is none or more than one.
    pub fn one(self) -> DbResult<Row> {
        let mut rows = self.rows.into_iter();
        match (rows.next(), rows.next()) {
            (Some(row), None) => Ok(row),
            (None, _) => Err(DbError::new(ErrorCode::NotFound, "expected one row, got none")),
            _ => Err(DbError::invalid("expected one row, got several")),
        }
    }

    /// Each row turned into a `T`.
    pub fn map<T: FromRow>(self) -> DbResult<Vec<T>> {
        self.rows.into_iter().map(T::from_row).collect()
    }

    /// The first column of each row.
    pub fn column<T: FromValue>(self) -> DbResult<Vec<T>> {
        self.rows.into_iter().map(|mut row| row.take_at(0)).collect()
    }
}

impl IntoIterator for Rows {
    type Item = Row;
    type IntoIter = std::vec::IntoIter<Row>;
    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

impl<'a> IntoIterator for &'a Rows {
    type Item = &'a Row;
    type IntoIter = std::slice::Iter<'a, Row>;
    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

impl Row {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn values(&self) -> &[Value] {
        &self.values
    }

    pub fn into_values(self) -> Vec<Value> {
        self.values
    }

    fn position(&self, column: &str) -> DbResult<usize> {
        self.columns.iter().position(|c| c == column).ok_or_else(|| DbError::not_found(format!("no column {column:?} in the result")))
    }

    fn convert<T: FromValue>(value: Value, column: &str) -> DbResult<T> {
        T::from_value(value).map_err(|e| DbError::new(e.code, format!("column {column:?}: {}", e.message)))
    }

    /// A column's value as a `T`: `row.get::<String>("name")?`, or
    /// `Option<String>` where it may be NULL.
    pub fn get<T: FromValue>(&self, column: &str) -> DbResult<T> {
        let i = self.position(column)?;
        Self::convert(self.values[i].clone(), column)
    }

    /// The same without copying: the value is moved out and NULL left in
    /// its place. For large text and bytes.
    pub fn take<T: FromValue>(&mut self, column: &str) -> DbResult<T> {
        let i = self.position(column)?;
        Self::convert(std::mem::replace(&mut self.values[i], Value::Null), column)
    }

    /// A value by its position in the result.
    pub fn at<T: FromValue>(&self, index: usize) -> DbResult<T> {
        let value = self.values.get(index).cloned().ok_or_else(|| DbError::not_found(format!("no column {index} in the result")))?;
        T::from_value(value)
    }

    pub fn take_at<T: FromValue>(&mut self, index: usize) -> DbResult<T> {
        let value = self.values.get_mut(index).ok_or_else(|| DbError::not_found(format!("no column {index} in the result")))?;
        T::from_value(std::mem::replace(value, Value::Null))
    }

    pub fn value(&self, column: &str) -> DbResult<&Value> {
        Ok(&self.values[self.position(column)?])
    }

    pub fn is_null(&self, column: &str) -> DbResult<bool> {
        Ok(self.value(column)?.is_null())
    }
}

/// Builds a Rust value from a row.
///
/// ```ignore
/// struct User { id: Uuid, name: String }
/// impl FromRow for User {
///     fn from_row(mut row: Row) -> DbResult<Self> {
///         Ok(User { id: row.take("id")?, name: row.take("name")? })
///     }
/// }
/// let users: Vec<User> = db.query(Query::table("users")).await?.map()?;
/// ```
pub trait FromRow: Sized {
    fn from_row(row: Row) -> DbResult<Self>;
}

impl FromRow for Row {
    fn from_row(row: Row) -> DbResult<Self> {
        Ok(row)
    }
}

macro_rules! tuple_from_row {
    ($($name:ident $index:tt),+) => {
        impl<$($name: FromValue),+> FromRow for ($($name,)+) {
            fn from_row(mut row: Row) -> DbResult<Self> {
                Ok(($(row.take_at::<$name>($index)?,)+))
            }
        }
    };
}

tuple_from_row!(A 0);
tuple_from_row!(A 0, B 1);
tuple_from_row!(A 0, B 1, C 2);
tuple_from_row!(A 0, B 1, C 2, D 3);
tuple_from_row!(A 0, B 1, C 2, D 3, E 4);
tuple_from_row!(A 0, B 1, C 2, D 3, E 4, F 5);
tuple_from_row!(A 0, B 1, C 2, D 3, E 4, F 5, G 6);
tuple_from_row!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Rows {
        Rows::new(RowSet {
            columns: vec!["id".into(), "name".into(), "age".into()],
            rows: vec![vec![Value::Int(1), Value::Text("ann".into()), Value::Null], vec![Value::Int(2), Value::Text("bob".into()), Value::Int(40)]],
            affected: 2,
            total: Some(10),
        })
    }

    #[test]
    fn typed_access() {
        let rows = rows();
        assert_eq!((rows.len(), rows.affected(), rows.total()), (2, 2, Some(10)));
        let first = rows.first().unwrap();
        assert_eq!(first.get::<i64>("id").unwrap(), 1);
        assert_eq!(first.get::<String>("name").unwrap(), "ann");
        assert_eq!(first.get::<Option<i64>>("age").unwrap(), None);
        assert!(first.get::<i64>("age").is_err());
        assert!(first.get::<i64>("missing").is_err());
        let err = first.get::<i64>("name").unwrap_err();
        assert!(err.message.contains("name"), "{err}");

        let pairs: Vec<(i64, String, Option<i64>)> = rows.clone().map().unwrap();
        assert_eq!(pairs[1], (2, "bob".to_string(), Some(40)));
        assert_eq!(rows.clone().column::<i64>().unwrap(), vec![1, 2]);
        assert!(rows.clone().one().is_err());
        assert_eq!(rows.opt().unwrap().get::<i64>("id").unwrap(), 1);
    }
}
