//! Rows as they are stored: the column count, then each column's value in
//! the protocol's encoding, or a reference to a large value kept in the
//! table's blob tree (see `blob.rs`).
//!
//! A stored row may hold fewer columns than its table has now: columns
//! added later are read as their fill value, without rewriting the rows.

use std::cell::OnceCell;

use database_protocol::codec::{self, Reader, Writer};
use database_protocol::value::Value;

use crate::blob;
use crate::btree::{Pages, PagesMut};
use crate::error::{Error, Result};
use crate::page::PageId;

/// The tag of a column kept in the blob tree: then the tag the value
/// would have had inline, its number in the blob tree, and its length.
pub const TAG_BLOB: u8 = 20;

/// A value shorter than this is never moved out of its row: the row
/// could not shrink by enough to matter.
const MIN_BLOB: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobRef {
    /// `TAG_TEXT`, `TAG_BYTES`, `TAG_JSON` or `TAG_LIST`.
    pub tag: u8,
    pub id: u64,
    pub len: u64,
}

fn malformed() -> Error {
    Error::corrupt("a stored row is malformed")
}

/// Moves the reader past one stored column.
fn skip(r: &mut Reader) -> Option<()> {
    match r.u8().ok()? {
        codec::TAG_NULL | codec::TAG_FALSE | codec::TAG_TRUE => {}
        codec::TAG_INT | codec::TAG_TIMESTAMP | codec::TAG_DATE => {
            r.varint().ok()?;
        }
        codec::TAG_FLOAT => {
            r.u64().ok()?;
        }
        codec::TAG_DECIMAL => {
            r.u8().ok()?;
            r.u64().ok()?;
            r.u64().ok()?;
        }
        codec::TAG_TEXT | codec::TAG_BYTES | codec::TAG_JSON => {
            r.bytes().ok()?;
        }
        codec::TAG_UUID => {
            r.u64().ok()?;
            r.u64().ok()?;
        }
        codec::TAG_LIST => {
            for _ in 0..r.count().ok()? {
                skip(r)?;
            }
        }
        TAG_BLOB => {
            r.u8().ok()?;
            r.varint().ok()?;
            r.varint().ok()?;
        }
        _ => return None,
    }
    Some(())
}

/// One column's stored bytes for a value kept inline.
pub fn encode_value(value: &Value) -> Vec<u8> {
    let mut w = Writer::new();
    codec::put_value(&mut w, value);
    w.buf
}

fn encode_blob_ref(blob: BlobRef) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(TAG_BLOB);
    w.u8(blob.tag);
    w.varint(blob.id);
    w.varint(blob.len);
    w.buf
}

fn decode_blob_ref(col: &[u8]) -> Option<BlobRef> {
    if col.first() != Some(&TAG_BLOB) {
        return None;
    }
    let mut r = Reader::new(&col[1..]);
    Some(BlobRef { tag: r.u8().ok()?, id: r.varint().ok()?, len: r.varint().ok()? })
}

/// The bytes a value occupies in the blob tree, and its tag, if it is of
/// a kind that can live there.
fn blob_payload(col: &[u8]) -> Option<(u8, &[u8])> {
    let tag = *col.first()?;
    match tag {
        // Text, bytes and JSON are stored as their bytes alone, so the
        // blob's length is the value's length in bytes.
        codec::TAG_TEXT | codec::TAG_BYTES | codec::TAG_JSON => {
            let mut r = Reader::new(&col[1..]);
            Some((tag, r.bytes().ok()?))
        }
        // A list is stored as its whole encoding.
        codec::TAG_LIST => Some((tag, col)),
        _ => None,
    }
}

/// Joins columns into a stored row.
pub fn assemble(cols: &[Vec<u8>]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(cols.len() as u64);
    for col in cols {
        w.buf.extend_from_slice(col);
    }
    w.buf
}

/// The bytes to store for `row` under a budget: the row's own if they
/// fit, which is the usual case and copies nothing column by column;
/// otherwise the row with its largest values moved to the blob tree.
pub fn fitted<P: PagesMut>(
    p: &mut P,
    row: &Row,
    budget: usize,
    blobs: &mut PageId,
    next_blob: &mut u64,
) -> Result<Option<Vec<u8>>> {
    if row.columns_len() + 3 <= budget {
        return Ok(None);
    }
    let mut cols: Vec<Vec<u8>> = (0..row.stored())
        .map(|i| row.raw_col(i).unwrap_or_default().to_vec())
        .collect();
    fit(p, &mut cols, budget, blobs, next_blob)?;
    Ok(Some(assemble(&cols)))
}

/// Moves the largest values into the blob tree until the row fits in
/// `budget` bytes. `blobs` is the tree's root and `next_blob` the next
/// unused value number; both are updated.
pub fn fit<P: PagesMut>(p: &mut P, cols: &mut [Vec<u8>], budget: usize, blobs: &mut PageId, next_blob: &mut u64) -> Result<()> {
    loop {
        let total: usize = cols.iter().map(Vec::len).sum::<usize>() + 3;
        if total <= budget {
            return Ok(());
        }
        let candidate = cols.iter().enumerate().filter(|(_, col)| col.len() >= MIN_BLOB && blob_payload(col).is_some()).max_by_key(|(_, col)| col.len());
        let Some((i, _)) = candidate else {
            return Err(Error::db(
                database_protocol::error::ErrorCode::InvalidRequest,
                format!("a row of {total} bytes does not fit: it has too many columns that cannot be stored outside it"),
            ));
        };
        let (tag, payload) = blob_payload(&cols[i]).unwrap();
        let blob = BlobRef { tag, id: *next_blob, len: payload.len() as u64 };
        *blobs = blob::write(p, *blobs, blob.id, payload)?;
        *next_blob += 1;
        cols[i] = encode_blob_ref(blob);
    }
}

/// A row read from a table, decoded column by column as each is asked for.
pub struct Row {
    raw: Vec<u8>,
    /// Where each stored column starts, and where the last one ends.
    starts: Vec<u32>,
    values: Vec<OnceCell<Value>>,
}

impl Row {
    pub fn parse(raw: Vec<u8>) -> Result<Row> {
        let mut r = Reader::new(&raw);
        let n = r.count().map_err(|_| malformed())?;
        let mut starts = Vec::with_capacity(n + 1);
        for _ in 0..n {
            starts.push(r.position() as u32);
            skip(&mut r).ok_or_else(malformed)?;
        }
        starts.push(r.position() as u32);
        if !r.is_empty() {
            return Err(malformed());
        }
        let values = (0..n).map(|_| OnceCell::new()).collect();
        Ok(Row { raw, starts, values })
    }

    /// A row that was never stored, from its values.
    pub fn from_values(values: &[Value]) -> Row {
        let row = Row::build(values.len(), |i, out| codec::put_value(out, &values[i]));
        for (cell, value) in row.values.iter().zip(values) {
            let _ = cell.set(value.clone());
        }
        row
    }

    /// A row of `n` columns, each written by `column` in the stored
    /// encoding: a value's, or bytes copied from another stored row.
    pub fn build(n: usize, mut column: impl FnMut(usize, &mut Writer)) -> Row {
        // Room for a row of ordinary size at once, not by doubling.
        let mut w = Writer::with_capacity(32 + 24 * n);
        w.varint(n as u64);
        let mut starts = Vec::with_capacity(n + 1);
        for i in 0..n {
            starts.push(w.buf.len() as u32);
            column(i, &mut w);
        }
        starts.push(w.buf.len() as u32);
        Row {
            raw: w.buf,
            starts,
            values: (0..n).map(|_| OnceCell::new()).collect(),
        }
    }

    /// The row as it is stored.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// The bytes its columns take, without the count before them.
    pub fn columns_len(&self) -> usize {
        self.raw.len() - self.starts.first().map_or(0, |s| *s as usize)
    }

    /// How many columns the row was stored with.
    pub fn stored(&self) -> usize {
        self.values.len()
    }

    /// A column's stored bytes, if the row has that column.
    pub fn raw_col(&self, i: usize) -> Option<&[u8]> {
        (i < self.stored()).then(|| &self.raw[self.starts[i] as usize..self.starts[i + 1] as usize])
    }

    pub fn blob(&self, i: usize) -> Option<BlobRef> {
        decode_blob_ref(self.raw_col(i)?)
    }

    /// Every value of the row that lives in the blob tree.
    pub fn blobs(&self) -> Vec<BlobRef> {
        (0..self.stored()).filter_map(|i| self.blob(i)).collect()
    }

    /// A stored column's value, reading it from the blob tree if it lives
    /// there. `None` if the row was stored before the column existed.
    pub fn value<P: Pages + ?Sized>(&self, p: &P, blobs: PageId, i: usize) -> Result<Option<&Value>> {
        let Some(cell) = self.values.get(i) else { return Ok(None) };
        if cell.get().is_none() {
            let col = self.raw_col(i).unwrap();
            let value = match decode_blob_ref(col) {
                Some(blob) => {
                    let bytes = blob::read(p, blobs, blob.id, blob.len)?;
                    match blob.tag {
                        codec::TAG_TEXT => Value::Text(String::from_utf8(bytes).map_err(|_| malformed())?),
                        codec::TAG_BYTES => Value::Bytes(bytes),
                        codec::TAG_JSON => Value::Json(codec::parse_json(&bytes).map_err(|_| malformed())?),
                        _ => codec::get_value(&mut Reader::new(&bytes)).map_err(|_| malformed())?,
                    }
                }
                None => codec::get_value(&mut Reader::new(col)).map_err(|_| malformed())?,
            };
            let _ = cell.set(value);
        }
        Ok(cell.get())
    }

    /// The length in bytes of a text or bytes column, without reading a
    /// large one. `None` for other kinds and for NULL.
    pub fn octet_length(&self, i: usize) -> Option<u64> {
        let col = self.raw_col(i)?;
        if let Some(blob) = decode_blob_ref(col) {
            return matches!(blob.tag, codec::TAG_TEXT | codec::TAG_BYTES).then_some(blob.len);
        }
        match col[0] {
            codec::TAG_TEXT | codec::TAG_BYTES => Reader::new(&col[1..]).bytes().ok().map(|b| b.len() as u64),
            _ => None,
        }
    }

    /// Bytes `start..start + n` of a text or bytes column, reading only
    /// the chunks that hold them when the value is large. `None` for
    /// other kinds and for NULL.
    pub fn byte_range<P: Pages + ?Sized>(&self, p: &P, blobs: PageId, i: usize, start: u64, n: u64) -> Result<Option<Vec<u8>>> {
        let Some(col) = self.raw_col(i) else { return Ok(None) };
        if let Some(blob) = decode_blob_ref(col) {
            if !matches!(blob.tag, codec::TAG_TEXT | codec::TAG_BYTES) {
                return Ok(None);
            }
            return blob::read_range(p, blobs, blob.id, blob.len, start, n).map(Some);
        }
        match col[0] {
            codec::TAG_TEXT | codec::TAG_BYTES => {
                let bytes = Reader::new(&col[1..]).bytes().map_err(|_| malformed())?;
                let lo = (start as usize).min(bytes.len());
                let hi = (start.saturating_add(n) as usize).min(bytes.len());
                Ok(Some(bytes[lo..hi].to_vec()))
            }
            _ => Ok(None),
        }
    }

    /// Whether the column is text (not bytes), stored inline or not.
    pub fn is_text(&self, i: usize) -> bool {
        match self.raw_col(i) {
            Some(col) => match decode_blob_ref(col) {
                Some(blob) => blob.tag == codec::TAG_TEXT,
                None => col[0] == codec::TAG_TEXT,
            },
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::tests::MemPages;
    use crate::page::NO_PAGE;
    use serde_json::json;

    fn values() -> Vec<Value> {
        vec![
            Value::Int(-7),
            Value::Null,
            Value::Text("héllo".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::Bool(true),
            Value::Float(1.5),
            Value::Decimal("12.34".parse().unwrap()),
            Value::Uuid([4; 16]),
            Value::Timestamp(99),
            Value::Date(-3),
            Value::Json(json!({"a": [1, 2]})),
            Value::List(vec![Value::Text("x".into()), Value::Null]),
        ]
    }

    #[test]
    fn a_small_row_is_stored_whole_and_read_back() {
        let p = MemPages::new();
        let cols: Vec<Vec<u8>> = values().iter().map(encode_value).collect();
        let row = Row::parse(assemble(&cols)).unwrap();
        assert_eq!(row.stored(), 12);
        for (i, want) in values().iter().enumerate() {
            assert_eq!(row.value(&p, NO_PAGE, i).unwrap(), Some(want));
            assert_eq!(row.blob(i), None);
        }
        assert_eq!(row.value(&p, NO_PAGE, 12).unwrap(), None, "a column added later");
        assert_eq!(row.octet_length(2), Some(6));
        assert_eq!(row.octet_length(0), None);
        assert_eq!(row.byte_range(&p, NO_PAGE, 2, 1, 2).unwrap(), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn large_values_move_to_the_blob_tree_until_the_row_fits() {
        let mut p = MemPages::new();
        let body = "x".repeat(50_000) + "é";
        let big_list = Value::List((0..500).map(Value::Int).collect());
        let doc = Value::Json(json!({"text": "y".repeat(5_000)}));
        let all = vec![Value::Int(1), Value::Text(body.clone()), Value::Text("short".into()), big_list.clone(), doc.clone(), Value::Bytes(vec![9; 3_000])];
        let mut cols: Vec<Vec<u8>> = all.iter().map(encode_value).collect();
        let (mut blobs, mut next) = (NO_PAGE, 1u64);
        fit(&mut p, &mut cols, 1_000, &mut blobs, &mut next).unwrap();
        let raw = assemble(&cols);
        assert!(raw.len() <= 1_000);

        let row = Row::parse(raw).unwrap();
        assert_eq!(row.blobs().len(), 4);
        assert_eq!(row.blob(1).unwrap().len, body.len() as u64);
        assert_eq!(row.blob(2), None, "small values stay in the row");
        for (i, want) in all.iter().enumerate() {
            assert_eq!(row.value(&p, blobs, i).unwrap(), Some(want));
        }
        // Length and ranges of the body, without the body.
        assert_eq!(row.octet_length(1), Some(50_002));
        assert_eq!(row.byte_range(&p, blobs, 1, 49_999, 10).unwrap(), Some("xé".as_bytes().to_vec()));
        assert!(row.is_text(1) && !row.is_text(5));
    }

    #[test]
    fn a_row_that_cannot_shrink_is_refused() {
        let mut p = MemPages::new();
        let mut cols: Vec<Vec<u8>> = (0..300).map(|n| encode_value(&Value::Int(n << 40))).collect();
        assert!(fit(&mut p, &mut cols, 1_000, &mut NO_PAGE.clone(), &mut 1).is_err());
    }

    #[test]
    fn damaged_rows_are_errors() {
        let cols: Vec<Vec<u8>> = values().iter().map(encode_value).collect();
        let raw = assemble(&cols);
        for cut in 0..raw.len() {
            assert!(Row::parse(raw[..cut].to_vec()).is_err(), "cut at {cut}");
        }
    }
}
