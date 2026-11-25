//! Rows as they are stored: the column count, then each column's value in
//! the protocol's encoding, or a reference to a large value kept in the
//! table's blob tree (see `blob.rs`).
//!
//! A stored row may hold fewer columns than its table has now: columns
//! added later are read as their fill value, without rewriting the rows.
//!
//! A reference to a large value may carry the value's first bytes (a
//! column's `keep_start`): what reads only the start of the value then
//! reads it here, and never looks the value up. The start is a copy to
//! read faster from. Every reader works without one, and a row that has
//! no room for it is stored without.

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

/// The same with the value's start kept beside the reference: after the
/// length, the count of the bytes kept and the bytes.
pub const TAG_BLOB_START: u8 = 21;

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

/// A count at `pos`, and where the bytes after it start. Nearly every
/// count in a row is below 128 and takes one byte.
fn count_at(raw: &[u8], mut pos: usize) -> Option<(usize, usize)> {
    match *raw.get(pos)? {
        short if short < 0x80 => Some((short as usize, pos + 1)),
        _ => {
            let n = crate::page::get_varint(raw, &mut pos)?;
            Some((usize::try_from(n).ok()?, pos))
        }
    }
}

/// Where the stored column that starts at `pos` ends: `None` if it is
/// not a column, or runs past the row. Every read of a row begins by
/// finding its columns, so this is done on the bytes where they lie,
/// with no reader between.
fn skip(raw: &[u8], pos: usize) -> Option<usize> {
    let after = pos + 1;
    let end = match *raw.get(pos)? {
        codec::TAG_NULL | codec::TAG_FALSE | codec::TAG_TRUE => after,
        codec::TAG_INT | codec::TAG_TIMESTAMP | codec::TAG_DATE => {
            let mut end = after;
            crate::page::get_varint(raw, &mut end)?;
            end
        }
        codec::TAG_FLOAT => after + 8,
        codec::TAG_DECIMAL => after + 17,
        codec::TAG_UUID => after + 16,
        codec::TAG_TEXT | codec::TAG_BYTES | codec::TAG_JSON => {
            let (n, at) = count_at(raw, after)?;
            at.checked_add(n)?
        }
        codec::TAG_LIST => {
            let (n, mut at) = count_at(raw, after)?;
            // An item takes a byte at least: more items than bytes left
            // is a lie, and must not be walked.
            if n > raw.len() - at {
                return None;
            }
            for _ in 0..n {
                at = skip(raw, at)?;
            }
            at
        }
        tag @ (TAG_BLOB | TAG_BLOB_START) => {
            let mut end = after + 1;
            crate::page::get_varint(raw, &mut end)?;
            crate::page::get_varint(raw, &mut end)?;
            if tag == TAG_BLOB_START {
                let (n, at) = count_at(raw, end)?;
                end = at.checked_add(n)?;
            }
            end
        }
        _ => return None,
    };
    (end <= raw.len()).then_some(end)
}

/// A value that is in its row, from its stored bytes. The kinds a row is
/// mostly made of are read where they lie; the others by the codec, which
/// reads them all and is the measure of what these must give.
fn decode(col: &[u8]) -> Result<Value> {
    let signed = |col: &[u8]| -> Option<i64> {
        let mut pos = 1;
        let v = crate::page::get_varint(col, &mut pos)?;
        (pos == col.len()).then_some(((v >> 1) as i64) ^ -((v & 1) as i64))
    };
    let counted = |col: &'_ [u8]| -> Option<Vec<u8>> {
        let (n, at) = count_at(col, 1)?;
        (at.checked_add(n)? == col.len()).then(|| col[at..].to_vec())
    };
    let value = match *col.first().ok_or_else(malformed)? {
        codec::TAG_NULL if col.len() == 1 => Some(Value::Null),
        codec::TAG_FALSE if col.len() == 1 => Some(Value::Bool(false)),
        codec::TAG_TRUE if col.len() == 1 => Some(Value::Bool(true)),
        codec::TAG_INT => signed(col).map(Value::Int),
        codec::TAG_TIMESTAMP => signed(col).map(Value::Timestamp),
        codec::TAG_UUID => col[1..].try_into().ok().map(Value::Uuid),
        codec::TAG_TEXT => counted(col).and_then(|bytes| String::from_utf8(bytes).ok()).map(Value::Text),
        codec::TAG_BYTES => counted(col).map(Value::Bytes),
        _ => {
            let mut r = Reader::new(col);
            codec::get_value(&mut r).ok().filter(|_| r.is_empty())
        }
    };
    value.ok_or_else(malformed)
}

/// Whether a stored column's value costs more to make than to copy: one
/// that is parsed (JSON), built of others (a list), or read from the
/// blob tree. A row keeps such a value once it has made it.
fn costly(col: &[u8]) -> bool {
    matches!(col.first(), Some(&(codec::TAG_JSON | codec::TAG_LIST | TAG_BLOB | TAG_BLOB_START)))
}

/// A row of this many columns keeps their places in itself; one of more
/// has a list of them made for it.
const FEW: usize = 15;

/// Where each stored column starts, and where the last one ends.
enum Starts {
    Few([u32; FEW + 1]),
    Many(Vec<u32>),
}

impl Starts {
    fn for_columns(n: usize) -> Starts {
        if n <= FEW {
            Starts::Few([0; FEW + 1])
        } else {
            Starts::Many(vec![0; n + 1])
        }
    }

    fn set(&mut self, i: usize, at: usize) {
        match self {
            Starts::Few(starts) => starts[i] = at as u32,
            Starts::Many(starts) => starts[i] = at as u32,
        }
    }

    fn at(&self, i: usize) -> usize {
        match self {
            Starts::Few(starts) => starts[i] as usize,
            Starts::Many(starts) => starts[i] as usize,
        }
    }
}

/// One column's stored bytes for a value kept inline.
pub fn encode_value(value: &Value) -> Vec<u8> {
    let mut w = Writer::new();
    codec::put_value(&mut w, value);
    w.buf
}

/// A reference as it is stored, with as much of the value's start as
/// `start` holds: none, for the plain form.
fn encode_blob_ref(blob: BlobRef, start: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(24 + start.len());
    w.u8(if start.is_empty() { TAG_BLOB } else { TAG_BLOB_START });
    w.u8(blob.tag);
    w.varint(blob.id);
    w.varint(blob.len);
    if !start.is_empty() {
        w.bytes(start);
    }
    w.buf
}

/// A stored column as a reference and the start kept with it, which is
/// empty if none is: `None` for a column that holds its value.
///
/// Read on the bytes where they lie, as `skip` reads them: a page of
/// previews asks this of every row.
fn blob_parts(col: &[u8]) -> Option<(BlobRef, &[u8])> {
    let tag = *col.first()?;
    if tag != TAG_BLOB && tag != TAG_BLOB_START {
        return None;
    }
    let mut pos = 2;
    let blob = BlobRef { tag: *col.get(1)?, id: crate::page::get_varint(col, &mut pos)?, len: crate::page::get_varint(col, &mut pos)? };
    let start = if tag == TAG_BLOB_START {
        let (n, at) = count_at(col, pos)?;
        col.get(at..at.checked_add(n)?)?
    } else {
        &[]
    };
    Some((blob, start))
}

fn decode_blob_ref(col: &[u8]) -> Option<BlobRef> {
    blob_parts(col).map(|(blob, _)| blob)
}

/// How many of a value's first `n` bytes to keep as its start: all of
/// them, or for a text as many as end with a whole character.
fn start_len(tag: u8, bytes: &[u8], n: usize) -> usize {
    let mut n = n.min(bytes.len());
    if tag == codec::TAG_TEXT {
        // A byte that continues a character is 10xxxxxx.
        while n > 0 && n < bytes.len() && bytes[n] & 0xC0 == 0x80 {
            n -= 1;
        }
    }
    n
}

/// The bytes a count takes before the bytes it counts.
fn count_len(n: usize) -> usize {
    crate::page::varint_len(n as u64)
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
///
/// `keep` says, by column, how many bytes of a value's start its column
/// asks to keep in the row when the value is moved out of it.
pub fn fitted<P: PagesMut>(
    p: &mut P,
    row: &Row,
    budget: usize,
    blobs: &mut PageId,
    next_blob: &mut u64,
    keep: &dyn Fn(usize) -> usize,
) -> Result<Option<Vec<u8>>> {
    if row.columns_len() + 3 <= budget {
        return Ok(None);
    }
    let mut cols: Vec<Vec<u8>> = (0..row.stored())
        .map(|i| row.raw_col(i).unwrap_or_default().to_vec())
        .collect();
    fit(p, &mut cols, budget, blobs, next_blob, keep)?;
    Ok(Some(assemble(&cols)))
}

/// Moves the largest values into the blob tree until the row fits in
/// `budget` bytes. `blobs` is the tree's root and `next_blob` the next
/// unused value number; both are updated.
///
/// Which values are moved does not depend on the starts kept of them: a
/// start is given only the room the row has left once it fits without
/// any, and is shortened, or left out, where that is not enough. So a
/// column that keeps a start never pushes another column's value out of
/// the row.
pub fn fit<P: PagesMut>(p: &mut P, cols: &mut [Vec<u8>], budget: usize, blobs: &mut PageId, next_blob: &mut u64, keep: &dyn Fn(usize) -> usize) -> Result<()> {
    // The starts the row holds already (of values that stay where they
    // are, when a row is written again) are set aside, to be put back
    // with the new ones as far as there is room.
    let mut starts: Vec<Vec<u8>> = vec![Vec::new(); cols.len()];
    for (col, start) in cols.iter_mut().zip(&mut starts) {
        if let Some((blob, kept)) = blob_parts(col).filter(|(_, kept)| !kept.is_empty()) {
            *start = kept.to_vec();
            *col = encode_blob_ref(blob, &[]);
        }
    }
    let total = loop {
        let total: usize = cols.iter().map(Vec::len).sum::<usize>() + 3;
        if total <= budget {
            break total;
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
        if matches!(tag, codec::TAG_TEXT | codec::TAG_BYTES) {
            starts[i] = payload[..start_len(tag, payload, keep(i))].to_vec();
        }
        cols[i] = encode_blob_ref(blob, &[]);
    };
    let mut room = budget - total;
    for (col, start) in cols.iter_mut().zip(&starts) {
        if start.is_empty() {
            continue;
        }
        let Some(blob) = decode_blob_ref(col) else { continue };
        // As much of it as fits beside its own count.
        let mut n = start.len().min(room);
        while n > 0 && n + count_len(n) > room {
            n -= 1;
        }
        let n = start_len(blob.tag, start, n);
        if n > 0 {
            *col = encode_blob_ref(blob, &start[..n]);
            room -= n + count_len(n);
        }
    }
    Ok(())
}

/// A row read from a table, decoded column by column as each is asked for.
///
/// Reading a row allocates once, for its bytes. Where its columns are
/// is kept in the row itself, and a value is made from the bytes each
/// time it is asked for: for a number or a text that is no more than
/// copying a value kept ready would be. Only what costs more to make
/// (see `costly`) is kept.
pub struct Row {
    raw: Vec<u8>,
    /// How many columns the row was stored with.
    cols: usize,
    starts: Starts,
    /// The costly values made so far, by column. Not made at all for a
    /// row of which none is read.
    made: OnceCell<Box<[OnceCell<Value>]>>,
}

impl Row {
    pub fn parse(raw: Vec<u8>) -> Result<Row> {
        let (cols, mut pos) = count_at(&raw, 0).ok_or_else(malformed)?;
        // A column takes a byte at least.
        if cols > raw.len() - pos {
            return Err(malformed());
        }
        let mut starts = Starts::for_columns(cols);
        for i in 0..cols {
            starts.set(i, pos);
            pos = skip(&raw, pos).ok_or_else(malformed)?;
        }
        starts.set(cols, pos);
        if pos != raw.len() {
            return Err(malformed());
        }
        Ok(Row { raw, cols, starts, made: OnceCell::new() })
    }

    /// A row that was never stored, from its values.
    pub fn from_values(values: &[Value]) -> Row {
        let row = Row::build(values.len(), |i, out| codec::put_value(out, &values[i]));
        // What would be costly to make again from the bytes is at hand.
        for (i, value) in values.iter().enumerate() {
            if matches!(value, Value::Json(_) | Value::List(_)) {
                let _ = row.made()[i].set(value.clone());
            }
        }
        row
    }

    /// A row of `n` columns, each written by `column` in the stored
    /// encoding: a value's, or bytes copied from another stored row.
    pub fn build(n: usize, mut column: impl FnMut(usize, &mut Writer)) -> Row {
        // Room for a row of ordinary size at once, not by doubling.
        let mut w = Writer::with_capacity(32 + 24 * n);
        w.varint(n as u64);
        let mut starts = Starts::for_columns(n);
        for i in 0..n {
            starts.set(i, w.buf.len());
            column(i, &mut w);
        }
        starts.set(n, w.buf.len());
        Row { raw: w.buf, cols: n, starts, made: OnceCell::new() }
    }

    fn made(&self) -> &[OnceCell<Value>] {
        self.made.get_or_init(|| (0..self.cols).map(|_| OnceCell::new()).collect())
    }

    /// The row as it is stored.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// The row's bytes, for the buffer they are in to be used again.
    pub fn into_raw(self) -> Vec<u8> {
        self.raw
    }

    /// The bytes its columns take, without the count before them.
    pub fn columns_len(&self) -> usize {
        self.raw.len() - self.starts.at(0)
    }

    /// How many columns the row was stored with.
    pub fn stored(&self) -> usize {
        self.cols
    }

    /// A column's stored bytes, if the row has that column.
    pub fn raw_col(&self, i: usize) -> Option<&[u8]> {
        (i < self.cols).then(|| &self.raw[self.starts.at(i)..self.starts.at(i + 1)])
    }

    pub fn blob(&self, i: usize) -> Option<BlobRef> {
        decode_blob_ref(self.raw_col(i)?)
    }

    /// The reference to a value that lives in the blob tree, with the
    /// start the row keeps of it (empty if none): `blob` and `kept_start`
    /// at once.
    pub fn reference(&self, i: usize) -> Option<(BlobRef, &[u8])> {
        blob_parts(self.raw_col(i)?)
    }

    /// The start the row keeps of a value that lives in the blob tree,
    /// and the value's whole length: `None` if the column holds its
    /// value, or keeps nothing of it.
    pub fn kept_start(&self, i: usize) -> Option<(&[u8], u64)> {
        let (blob, start) = blob_parts(self.raw_col(i)?)?;
        (!start.is_empty()).then_some((start, blob.len))
    }

    /// Every value of the row that lives in the blob tree.
    pub fn blobs(&self) -> Vec<BlobRef> {
        (0..self.stored()).filter_map(|i| self.blob(i)).collect()
    }

    /// A stored column's value, reading it from the blob tree if it lives
    /// there. `None` if the row was stored before the column existed.
    pub fn value<P: Pages + ?Sized>(&self, p: &P, blobs: PageId, i: usize) -> Result<Option<Value>> {
        let Some(col) = self.raw_col(i) else { return Ok(None) };
        if !costly(col) {
            return decode(col).map(Some);
        }
        let cell = &self.made()[i];
        if cell.get().is_none() {
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
                None => decode(col)?,
            };
            let _ = cell.set(value);
        }
        Ok(cell.get().cloned())
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
        if let Some((blob, kept)) = blob_parts(col) {
            if !matches!(blob.tag, codec::TAG_TEXT | codec::TAG_BYTES) {
                return Ok(None);
            }
            // Within the start the row keeps, the bytes are here.
            let end = start.saturating_add(n).min(blob.len);
            if end <= kept.len() as u64 {
                return Ok(Some(kept[(start.min(end)) as usize..end as usize].to_vec()));
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
    use crate::vfs::Rng;
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
            assert_eq!(row.value(&p, NO_PAGE, i).unwrap().as_ref(), Some(want));
            assert_eq!(row.value(&p, NO_PAGE, i).unwrap().as_ref(), Some(want), "and when asked again");
            assert_eq!(row.blob(i), None);
        }
        assert_eq!(row.value(&p, NO_PAGE, 12).unwrap(), None, "a column added later");
        // A row made of values reads as the same row read from its bytes.
        let made = Row::from_values(&values());
        assert_eq!(made.raw(), row.raw());
        for (i, want) in values().iter().enumerate() {
            assert_eq!(made.value(&p, NO_PAGE, i).unwrap().as_ref(), Some(want));
        }
        assert_eq!(row.octet_length(2), Some(6));
        assert_eq!(row.octet_length(0), None);
        assert_eq!(row.byte_range(&p, NO_PAGE, 2, 1, 2).unwrap(), Some("é".as_bytes().to_vec()));
    }

    /// Where a stored column ends, found by the codec's own reader: slow,
    /// and what `skip` must agree with on every column, and on bytes that
    /// are none.
    fn skip_by_reader(r: &mut Reader) -> Option<()> {
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
                    skip_by_reader(r)?;
                }
            }
            TAG_BLOB => {
                r.u8().ok()?;
                r.varint().ok()?;
                r.varint().ok()?;
            }
            TAG_BLOB_START => {
                r.u8().ok()?;
                r.varint().ok()?;
                r.varint().ok()?;
                r.bytes().ok()?;
            }
            _ => return None,
        }
        Some(())
    }

    /// A reference and the start kept with it, read by the codec's own
    /// reader: what `blob_parts` must agree with.
    fn blob_parts_by_reader(col: &[u8]) -> Option<(BlobRef, &[u8])> {
        let tag = *col.first()?;
        if tag != TAG_BLOB && tag != TAG_BLOB_START {
            return None;
        }
        let mut r = Reader::new(&col[1..]);
        let blob = BlobRef { tag: r.u8().ok()?, id: r.varint().ok()?, len: r.varint().ok()? };
        let start = if tag == TAG_BLOB_START { r.bytes().ok()? } else { &[] };
        Some((blob, start))
    }

    /// Where a row's columns start and the last one ends, by the reader.
    fn starts_by_reader(raw: &[u8]) -> Option<Vec<usize>> {
        let mut r = Reader::new(raw);
        let n = r.count().ok()?;
        let mut starts = Vec::new();
        for _ in 0..n {
            starts.push(r.position());
            skip_by_reader(&mut r)?;
        }
        starts.push(r.position());
        r.is_empty().then_some(starts)
    }

    fn random_text(rng: &mut Rng) -> String {
        let alphabet = ["a", "Z", " ", "é", "ß", "日", "本", "📧", "\u{0}", "\u{7f}"];
        (0..rng.below(40)).map(|_| alphabet[rng.below(alphabet.len() as u64) as usize]).collect()
    }

    fn random_value(rng: &mut Rng, depth: u32) -> Value {
        let extreme = [0, 1, -1, 63, 64, -64, -65, i64::MAX, i64::MIN, 1 << 40];
        let int = |rng: &mut Rng| if rng.chance(3) { extreme[rng.below(extreme.len() as u64) as usize] } else { rng.next() as i64 >> rng.below(64) };
        match rng.below(if depth == 0 { 11 } else { 13 }) {
            0 => Value::Null,
            1 => Value::Bool(rng.chance(2)),
            2 => Value::Int(int(rng)),
            3 => Value::Float(int(rng) as f64 / 7.0),
            4 => Value::Decimal(database_protocol::decimal::Decimal::new(i128::from(int(rng)), rng.below(10) as u8)),
            5 => Value::Text(random_text(rng)),
            6 => Value::Bytes((0..rng.below(300)).map(|_| rng.next() as u8).collect()),
            7 => Value::Uuid(std::array::from_fn(|_| rng.next() as u8)),
            8 => Value::Timestamp(int(rng)),
            9 => Value::Date(int(rng) as i32),
            10 => Value::Text(String::new()),
            11 => Value::Json(json!({"n": int(rng), "s": random_text(rng), "list": [1, null, {"deep": true}]})),
            _ => Value::List((0..rng.below(5)).map(|_| random_value(rng, depth - 1)).collect()),
        }
    }

    #[test]
    fn columns_are_found_and_made_as_the_codec_finds_and_makes_them() {
        let p = MemPages::new();
        let mut rng = Rng::new(11);
        for round in 0..3_000 {
            // Rows of few columns and of many, of every kind of value,
            // with a reference to a value kept elsewhere now and then.
            let values: Vec<Value> = (0..rng.below(if round % 10 == 0 { 60 } else { 12 })).map(|_| random_value(&mut rng, 2)).collect();
            let mut cols: Vec<Vec<u8>> = values.iter().map(encode_value).collect();
            let mut references = Vec::new();
            for (i, col) in cols.iter_mut().enumerate() {
                if rng.chance(8) {
                    let start: Vec<u8> = (0..rng.below(3) * rng.below(200)).map(|_| rng.next() as u8).collect();
                    *col = encode_blob_ref(BlobRef { tag: codec::TAG_BYTES, id: rng.next() >> rng.below(64), len: rng.next() >> rng.below(64) }, &start);
                    references.push(i);
                }
            }
            let raw = assemble(&cols);
            let row = Row::parse(raw.clone()).unwrap();
            assert_eq!(row.stored(), values.len());
            for (i, want) in values.iter().enumerate() {
                assert_eq!(row.raw_col(i), Some(&cols[i][..]));
                assert_eq!(row.reference(i), blob_parts_by_reader(&cols[i]));
                if !references.contains(&i) {
                    assert_eq!(row.value(&p, NO_PAGE, i).unwrap().as_ref(), Some(want), "column {i} of {values:?}");
                }
            }
            assert_eq!(Row::from_values(&values).raw(), assemble(&values.iter().map(encode_value).collect::<Vec<_>>()));

            // The same bytes damaged: a byte changed, some cut off, some
            // added. Whether they are still a row, and where its columns
            // then are, is what the reader says; and a column that can
            // still be read gives what the codec makes of its bytes.
            for _ in 0..12 {
                let mut bad = raw.clone();
                match rng.below(4) {
                    0 => bad.truncate(rng.below(bad.len() as u64 + 1) as usize),
                    1 => bad.extend((0..1 + rng.below(3)).map(|_| rng.next() as u8)),
                    _ => {
                        for _ in 0..1 + rng.below(3) {
                            if !bad.is_empty() {
                                let at = rng.below(bad.len() as u64) as usize;
                                bad[at] = if rng.chance(2) { rng.next() as u8 } else { bad[at] ^ (1 << rng.below(8)) };
                            }
                        }
                    }
                }
                let by_reader = starts_by_reader(&bad);
                let Ok(row) = Row::parse(bad.clone()) else {
                    assert_eq!(by_reader, None, "{bad:?} is a row to the reader");
                    continue;
                };
                let starts = by_reader.unwrap_or_else(|| panic!("{bad:?} is no row to the reader"));
                assert_eq!(row.stored() + 1, starts.len());
                for i in 0..row.stored() {
                    let col = &bad[starts[i]..starts[i + 1]];
                    assert_eq!(row.raw_col(i), Some(col));
                    assert_eq!(blob_parts(col), blob_parts_by_reader(col), "{col:?}");
                    // And a column's bytes damaged alone, as a reference
                    // whose own parts are cut short or run long.
                    for cut in 0..col.len() {
                        assert_eq!(blob_parts(&col[..cut]), blob_parts_by_reader(&col[..cut]), "{col:?} cut at {cut}");
                    }
                    if blob_parts(col).is_some() {
                        continue;
                    }
                    let mut r = Reader::new(col);
                    let by_codec = codec::get_value(&mut r).ok().filter(|_| r.is_empty());
                    match (row.value(&p, NO_PAGE, i), by_codec) {
                        (Ok(Some(made)), Some(want)) => assert!(made.same(&want) || made == want, "{col:?}: {made:?} for {want:?}"),
                        (Err(_), None) => {}
                        (made, want) => panic!("{col:?}: {made:?} where the codec gives {want:?}"),
                    }
                }
            }
        }
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
        fit(&mut p, &mut cols, 1_000, &mut blobs, &mut next, &|_| 0).unwrap();
        let raw = assemble(&cols);
        assert!(raw.len() <= 1_000);

        let row = Row::parse(raw).unwrap();
        assert_eq!(row.blobs().len(), 4);
        assert_eq!(row.blob(1).unwrap().len, body.len() as u64);
        assert_eq!(row.blob(2), None, "small values stay in the row");
        for (i, want) in all.iter().enumerate() {
            assert_eq!(row.value(&p, blobs, i).unwrap().as_ref(), Some(want));
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
        assert!(fit(&mut p, &mut cols, 1_000, &mut NO_PAGE.clone(), &mut 1, &|_| 0).is_err());
    }

    /// A table's blob tree as a test holds it: the pages, the root, the
    /// next value's number.
    struct Tree {
        p: MemPages,
        blobs: PageId,
        next: u64,
    }

    impl Tree {
        fn new() -> Tree {
            Tree { p: MemPages::new(), blobs: NO_PAGE, next: 1 }
        }

        /// Stores columns as a row of at most `budget` bytes, each
        /// keeping as much of its value's start as `keep` says.
        fn store(&mut self, mut cols: Vec<Vec<u8>>, budget: usize, keep: &[usize]) -> Row {
            fit(&mut self.p, &mut cols, budget, &mut self.blobs, &mut self.next, &|i| keep[i]).unwrap();
            let raw = assemble(&cols);
            assert!(raw.len() <= budget, "{} bytes for a budget of {budget}", raw.len());
            Row::parse(raw).unwrap()
        }

        fn store_values(&mut self, values: &[Value], budget: usize, keep: &[usize]) -> Row {
            self.store(values.iter().map(encode_value).collect(), budget, keep)
        }

        fn value(&self, row: &Row, i: usize) -> Value {
            row.value(&self.p, self.blobs, i).unwrap().unwrap()
        }
    }

    fn kept(row: &Row, i: usize) -> Option<usize> {
        row.kept_start(i).map(|(start, _)| start.len())
    }

    /// The budget under which a row of `len` bytes just fits: `fit`
    /// allows three bytes for the count of columns, where a row of a few
    /// columns takes one.
    fn just(len: usize) -> usize {
        len + 2
    }

    #[test]
    fn the_start_of_a_large_value_stays_in_its_row() {
        let mut tree = Tree::new();
        // Two-byte characters, so that a cut after an odd number of
        // bytes would fall inside one.
        let body = "é".repeat(3_000);
        let bytes: Vec<u8> = (0..5_000u32).map(|n| n as u8).collect();
        let values = vec![Value::Int(1), Value::Text(body.clone()), Value::Bytes(bytes.clone()), Value::Text("short".into())];
        let row = tree.store_values(&values, 1_000, &[0, 101, 100, 50]);

        // The text keeps whole characters only; the bytes keep what was
        // asked; a value that stays in the row keeps nothing beside it.
        assert_eq!(row.kept_start(1), Some((&body.as_bytes()[..100], 6_000)));
        assert_eq!(row.kept_start(2), Some((&bytes[..100], 5_000)));
        assert_eq!((row.kept_start(0), row.kept_start(3)), (None, None));
        assert_eq!(row.blobs().len(), 2);
        // Everything reads as it does without a start.
        for (i, want) in values.iter().enumerate() {
            assert_eq!(&tree.value(&row, i), want);
        }
        assert_eq!((row.octet_length(1), row.octet_length(2)), (Some(6_000), Some(5_000)));
        assert!(row.is_text(1) && !row.is_text(2));

        // A range inside the start never reaches the tree: here there is
        // no tree to reach.
        let nowhere = MemPages::new();
        assert_eq!(row.byte_range(&nowhere, NO_PAGE, 2, 0, 100).unwrap(), Some(bytes[..100].to_vec()));
        assert_eq!(row.byte_range(&nowhere, NO_PAGE, 2, 40, 10).unwrap(), Some(bytes[40..50].to_vec()));
        assert_eq!(row.byte_range(&nowhere, NO_PAGE, 2, 100, 0).unwrap(), Some(Vec::new()));
        assert_eq!(row.byte_range(&nowhere, NO_PAGE, 1, 98, 2).unwrap(), Some(body.as_bytes()[98..100].to_vec()));
        // One that runs past it is read from the tree, and is the same
        // bytes as without a start.
        assert!(row.byte_range(&nowhere, NO_PAGE, 2, 0, 101).is_err());
        for (start, n) in [(0usize, 101usize), (99, 2), (100, 1), (4_990, 100), (0, 5_000), (6_000, 5)] {
            let end = (start + n).min(5_000);
            assert_eq!(row.byte_range(&tree.p, tree.blobs, 2, start as u64, n as u64).unwrap(), Some(bytes[start.min(end)..end].to_vec()), "{start}+{n}");
        }
    }

    #[test]
    fn a_start_takes_only_the_room_the_row_has_left() {
        let body = "x".repeat(4_000);
        let other = "y".repeat(300);
        let values = vec![Value::Text(body.clone()), Value::Text(other.clone()), Value::Text(other.clone())];
        // What the row is with no start asked for: the body outside it,
        // the other values in it.
        let bare = Tree::new().store_values(&values, 1_000, &[0, 0, 0]).raw().to_vec();

        // Room for everything: the start is kept whole.
        let mut tree = Tree::new();
        let row = tree.store_values(&values, 1_000, &[256, 0, 0]);
        assert_eq!((kept(&row, 0), row.blobs().len()), (Some(256), 1));
        assert_eq!(row.raw().len(), bare.len() + 256 + 2, "the start and its count");

        // Less room than the start asks for: it is shortened to what is
        // left, and the row is as large as it may be. A start never
        // pushes another value out of the row.
        let mut tree = Tree::new();
        let row = tree.store_values(&values, just(bare.len()) + 51, &[256, 0, 0]);
        assert_eq!((kept(&row, 0), row.blobs().len()), (Some(50), 1));
        assert_eq!(row.raw().len(), bare.len() + 51);
        assert_eq!(tree.value(&row, 0), Value::Text(body.clone()));

        // No room at all: no start, and the row is what it would be had
        // none been asked for.
        let mut tree = Tree::new();
        let row = tree.store_values(&values, just(bare.len()), &[256, 0, 0]);
        assert_eq!(kept(&row, 0), None);
        assert_eq!(row.raw(), bare);

        // Starts are given room in the order of their columns.
        let two = vec![Value::Bytes(vec![1; 3_000]), Value::Bytes(vec![2; 2_000])];
        let bare = Tree::new().store_values(&two, 500, &[0, 0]).raw().len();
        let row = Tree::new().store_values(&two, just(bare) + 101 + 41, &[100, 100]);
        assert_eq!((kept(&row, 0), kept(&row, 1)), (Some(100), Some(40)));

        // A value is never both outside its row and whole inside it: it
        // left because the row had no room for it.
        let mut tree = Tree::new();
        let row = tree.store_values(&[Value::Text("z".repeat(200)), Value::Int(7)], 150, &[1_024, 0]);
        assert_eq!(row.blob(0).map(|b| b.len), Some(200));
        assert!(kept(&row, 0).is_some_and(|n| n < 200));
        assert_eq!(tree.value(&row, 0), Value::Text("z".repeat(200)));
    }

    #[test]
    fn a_row_written_again_keeps_the_starts_it_has_room_for() {
        let mut tree = Tree::new();
        let body = "x".repeat(4_000);
        let first = tree.store_values(&[Value::Text(body.clone()), Value::Text("a".into())], 1_000, &[256, 256]);
        assert_eq!(kept(&first, 0), Some(256));
        // An update carries the reference over as it is stored, start
        // and all, beside the column it changed.
        let reference = first.raw_col(0).unwrap().to_vec();
        let plain = encode_blob_ref(first.blob(0).unwrap(), &[]);
        let grown = |n: usize| vec![reference.clone(), encode_value(&Value::Text("b".repeat(n)))];
        let bare = |n: usize| assemble(&[plain.clone(), encode_value(&Value::Text("b".repeat(n)))]).len();

        // Room for both: nothing is moved, the start stays.
        let before = tree.next;
        let row = tree.store(grown(300), 1_000, &[256, 256]);
        assert_eq!((kept(&row, 0), row.blob(1), tree.next), (Some(256), None, before));
        // Too little for both: the start gives way, the value that grew
        // stays in the row.
        let row = tree.store(grown(300), just(bare(300)) + 11, &[256, 256]);
        assert_eq!((kept(&row, 0), row.blob(1), tree.next), (Some(10), None, before));
        let row = tree.store(grown(300), just(bare(300)), &[256, 256]);
        assert_eq!((kept(&row, 0), row.blob(1), tree.next), (None, None, before));
        // Too little even without the start: the value that grew leaves
        // the row, and the room that makes goes to the starts, the one
        // the row had and the new one.
        let row = tree.store(grown(3_000), 1_000, &[256, 256]);
        assert_eq!((kept(&row, 0), kept(&row, 1), tree.next), (Some(256), Some(256), before + 1));
        assert_eq!(row.blob(0), first.blob(0), "the body itself was not written again");
        assert_eq!((tree.value(&row, 0), tree.value(&row, 1)), (Value::Text(body), Value::Text("b".repeat(3_000))));
    }

    #[test]
    fn a_reference_stored_without_a_start_reads_as_it_always_did() {
        // Every reference was stored so before a column could keep a
        // start, and one still is where a row has no room for it: its tag,
        // the value's own tag, its number and its length.
        let blob = BlobRef { tag: codec::TAG_TEXT, id: 300, len: 6_000 };
        assert_eq!(encode_blob_ref(blob, &[]), [TAG_BLOB, codec::TAG_TEXT, 0xAC, 0x02, 0xF0, 0x2E]);

        let mut tree = Tree::new();
        let body = "é".repeat(3_000);
        let old = tree.store_values(&[Value::Text(body.clone()), Value::Int(1)], 1_000, &[0, 0]);
        assert_eq!((kept(&old, 0), old.blob(0).map(|b| b.len)), (None, Some(6_000)));
        // Written again around it in a column that now keeps a start: the
        // value is not read to make one, and the reference stays as it was.
        let before = tree.next;
        let row = tree.store(vec![old.raw_col(0).unwrap().to_vec(), encode_value(&Value::Int(2))], 1_000, &[256, 0]);
        assert_eq!((row.raw_col(0), kept(&row, 0), tree.next), (old.raw_col(0), None, before));
        // Every read goes to the value, and finds it.
        assert_eq!(tree.value(&row, 0), Value::Text(body.clone()));
        assert_eq!(row.byte_range(&tree.p, tree.blobs, 0, 0, 10).unwrap(), Some(body.as_bytes()[..10].to_vec()));
        assert!(row.byte_range(&MemPages::new(), NO_PAGE, 0, 0, 10).is_err(), "nothing of it is in the row");
    }

    #[test]
    fn damaged_rows_are_errors() {
        let mut cols: Vec<Vec<u8>> = values().iter().map(encode_value).collect();
        cols.push(encode_blob_ref(BlobRef { tag: codec::TAG_TEXT, id: 9, len: 5_000 }, b"the start of it"));
        cols.push(encode_blob_ref(BlobRef { tag: codec::TAG_BYTES, id: 10, len: 300 }, &[]));
        let raw = assemble(&cols);
        assert_eq!(Row::parse(raw.clone()).unwrap().kept_start(12), Some((&b"the start of it"[..], 5_000)));
        for cut in 0..raw.len() {
            assert!(Row::parse(raw[..cut].to_vec()).is_err(), "cut at {cut}");
        }
    }
}
