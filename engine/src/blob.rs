//! Large values, stored outside the rows that hold them.
//!
//! A row must fit in a quarter of a page. A value that would push it over
//! is cut into chunks and kept in the table's blob tree, under a number
//! the row remembers together with the value's length:
//!
//! ```text
//! key    value number (u64 big-endian), chunk number (u32 big-endian)
//! value  up to CHUNK bytes; every chunk full but the last
//! ```
//!
//! So byte `n` of a value is in chunk `n / CHUNK`, found by one descent of
//! the tree, and a row can be read, filtered and listed without touching
//! the megabytes of text it points at. Chunks of one value sit side
//! by side in the tree's leaves, four to a page.

use crate::btree::{self, Cursor, Pages, PagesMut};
use crate::error::{Error, Result};
use crate::page::PageId;

/// Bytes of a value in one chunk: the most that fits beside the key.
pub const CHUNK: usize = btree::max_value(12);

fn key(id: u64, chunk: u32) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..8].copy_from_slice(&id.to_be_bytes());
    k[8..].copy_from_slice(&chunk.to_be_bytes());
    k
}

/// Stores `data` as value `id` and returns the tree's new root.
pub fn write<P: PagesMut>(p: &mut P, mut root: PageId, id: u64, data: &[u8]) -> Result<PageId> {
    for (n, chunk) in data.chunks(CHUNK).enumerate() {
        root = btree::put(p, root, &key(id, n as u32), chunk)?.0;
    }
    Ok(root)
}

/// Reads `n` bytes of value `id` starting at byte `start` (from 0), or
/// fewer where the value ends. `len` is the value's length.
pub fn read_range<P: Pages + ?Sized>(p: &P, root: PageId, id: u64, len: u64, start: u64, n: u64) -> Result<Vec<u8>> {
    let start = start.min(len);
    let end = start.saturating_add(n).min(len);
    let mut out = Vec::with_capacity((end - start) as usize);
    if start == end {
        return Ok(out);
    }
    let first = (start as usize / CHUNK) as u32;
    let last = ((end as usize - 1) / CHUNK) as u32;
    if first == last {
        // Within one chunk, as the start of a text is: looked up alone.
        let (lo, hi) = (start as usize - first as usize * CHUNK, end as usize - first as usize * CHUNK);
        let mut short = false;
        let found = btree::get_with(p, root, &key(id, first), &mut |bytes| match bytes.get(lo..hi) {
            Some(part) => out.extend_from_slice(part),
            None => short = true,
        })?;
        return match (found, short) {
            (false, _) => Err(Error::corrupt(format!("chunk {first} of large value {id} is missing"))),
            (true, true) => Err(Error::corrupt(format!("chunk {first} of large value {id} is short"))),
            (true, false) => Ok(out),
        };
    }
    let mut cursor = Cursor::new(p, root);
    cursor.seek(&key(id, first))?;
    for chunk in first..=last {
        if cursor.key() != Some(&key(id, chunk)[..]) {
            return Err(Error::corrupt(format!("chunk {chunk} of large value {id} is missing")));
        }
        let bytes = cursor.value().unwrap();
        let chunk_start = chunk as usize * CHUNK;
        let lo = (start as usize).saturating_sub(chunk_start);
        let hi = (end as usize - chunk_start).min(CHUNK);
        if hi > bytes.len() {
            return Err(Error::corrupt(format!("chunk {chunk} of large value {id} is short")));
        }
        out.extend_from_slice(&bytes[lo..hi]);
        cursor.next()?;
    }
    Ok(out)
}

pub fn read<P: Pages + ?Sized>(p: &P, root: PageId, id: u64, len: u64) -> Result<Vec<u8>> {
    read_range(p, root, id, len, 0, len)
}

/// Removes value `id` and returns the tree's new root.
pub fn free<P: PagesMut>(p: &mut P, mut root: PageId, id: u64, len: u64) -> Result<PageId> {
    for chunk in 0..(len as usize).div_ceil(CHUNK) as u32 {
        root = btree::delete(p, root, &key(id, chunk))?.0;
    }
    Ok(root)
}

/// How many bytes of chunks the tree holds for value `id`, for the
/// consistency check.
pub fn stored_len<P: Pages + ?Sized>(p: &P, root: PageId, id: u64) -> Result<u64> {
    let mut cursor = Cursor::new(p, root);
    cursor.seek(&key(id, 0))?;
    let mut total = 0;
    while let Some(k) = cursor.key() {
        if k[..8] != id.to_be_bytes() {
            break;
        }
        total += cursor.value().unwrap().len() as u64;
        cursor.next()?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btree::tests::MemPages;
    use crate::page::NO_PAGE;
    use crate::vfs::Rng;

    fn bytes(rng: &mut Rng, len: usize) -> Vec<u8> {
        (0..len).map(|_| rng.next() as u8).collect()
    }

    #[test]
    fn values_of_every_size_come_back_whole_and_leave_nothing_when_freed() {
        let mut rng = Rng::new(1);
        let mut p = MemPages::new();
        let mut root = NO_PAGE;
        let mut stored = Vec::new();
        for (id, len) in [1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK, 100 * CHUNK + 17, 2_000_000].into_iter().enumerate() {
            let data = bytes(&mut rng, len);
            root = write(&mut p, root, id as u64, &data).unwrap();
            stored.push((id as u64, data));
        }
        for (id, data) in &stored {
            assert_eq!(&read(&p, root, *id, data.len() as u64).unwrap(), data);
            assert_eq!(stored_len(&p, root, *id).unwrap(), data.len() as u64);
        }
        for (id, data) in &stored {
            root = free(&mut p, root, *id, data.len() as u64).unwrap();
        }
        assert_eq!(root, NO_PAGE);
        assert!(p.pages.is_empty());
    }

    #[test]
    fn a_range_is_read_without_the_rest() {
        let mut rng = Rng::new(2);
        let mut p = MemPages::new();
        let len = 400 * CHUNK + 77;
        let data = bytes(&mut rng, len);
        let mut root = write(&mut p, NO_PAGE, 7, &data).unwrap();
        root = write(&mut p, root, 8, b"a neighbour").unwrap();
        for _ in 0..500 {
            let start = rng.below(len as u64 + 10);
            let n = rng.below(3 * CHUNK as u64);
            let want = &data[(start as usize).min(len)..((start + n) as usize).min(len)];
            assert_eq!(read_range(&p, root, 7, len as u64, start, n).unwrap(), want);
        }
        assert_eq!(read_range(&p, root, 7, len as u64, len as u64 - 5, 100).unwrap(), &data[len - 5..]);
        // A length that claims more than is stored is caught.
        assert!(read(&p, root, 7, len as u64 + CHUNK as u64).is_err());
    }
}
