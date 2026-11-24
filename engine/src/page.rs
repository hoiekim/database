//! Pages: the unit the data file is read, written and cached in.
//!
//! Every page starts with a 16-byte header:
//!
//! ```text
//! 0..4    CRC-32 of bytes 4..PAGE_SIZE, set when the page is written
//! 4       kind
//! 5       unused
//! 6..8    cell count (tree pages)
//! 8..12   next page (chain pages)
//! 12..16  payload length (chain pages)
//! ```

use std::sync::Arc;

pub const PAGE_SIZE: usize = 8192;
pub const HEADER: usize = 16;

/// A page number. The file offset is `id * PAGE_SIZE`. Pages 0 and 1 hold
/// the two copies of the file header, so 0 also serves as "no page".
pub type PageId = u32;
pub const NO_PAGE: PageId = 0;
pub const FIRST_DATA_PAGE: PageId = 2;

/// A page in memory. Shared and never changed once another thread can see
/// it: a writer builds a new buffer instead.
pub type Page = Arc<[u8]>;

/// Hashes page numbers for the maps the engine keeps of them, which are
/// looked in for every page a transaction touches. One multiplication:
/// the numbers are handed out in order by the engine itself, so nothing
/// here needs a hash that withstands chosen keys.
#[derive(Clone, Copy, Default)]
pub struct PageHasher(u64);

impl std::hash::Hasher for PageHasher {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u32(u32::from(*byte));
        }
    }
    fn write_u32(&mut self, n: u32) {
        self.0 = (self.0 ^ u64::from(n)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    fn finish(&self) -> u64 {
        // The high bits are the mixed ones, and a table reads both ends.
        self.0 ^ (self.0 >> 32)
    }
}

pub type PageHash = std::hash::BuildHasherDefault<PageHasher>;
/// A map keyed by page number.
pub type PageMap<V> = std::collections::HashMap<PageId, V, PageHash>;
/// A set of page numbers.
pub type PageSet = std::collections::HashSet<PageId, PageHash>;

pub const KIND_LEAF: u8 = 1;
pub const KIND_BRANCH: u8 = 2;
/// A link in a chain of pages holding one long value: a large column
/// value, the catalog, or the free list.
pub const KIND_CHAIN: u8 = 3;

pub fn new_buf(kind: u8) -> Vec<u8> {
    let mut buf = vec![0u8; PAGE_SIZE];
    buf[4] = kind;
    buf
}

pub fn kind(page: &[u8]) -> u8 {
    page[4]
}

pub fn get_u16(page: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([page[at], page[at + 1]])
}

pub fn put_u16(page: &mut [u8], at: usize, v: u16) {
    page[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn get_u32(page: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]])
}

pub fn put_u32(page: &mut [u8], at: usize, v: u32) {
    page[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn get_u64(page: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(page[at..at + 8].try_into().unwrap())
}

pub fn put_u64(page: &mut [u8], at: usize, v: u64) {
    page[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

pub fn checksum(page: &[u8]) -> u32 {
    crc32fast::hash(&page[4..])
}

/// A copy of the page with its checksum filled in, ready to write.
pub fn sealed(page: &[u8]) -> Vec<u8> {
    let mut out = page.to_vec();
    let crc = checksum(&out);
    put_u32(&mut out, 0, crc);
    out
}

pub fn verify(page: &[u8]) -> bool {
    get_u32(page, 0) == checksum(page)
}

// ------------------------------------------------------------- varints

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads a varint at `*pos`, advancing it. `None` if the bytes end first
/// or the number does not fit in 64 bits.
pub fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

pub const fn varint_len(v: u64) -> usize {
    ((64 - (v | 1).leading_zeros() as usize) + 6) / 7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for v in [0u64, 1, 127, 128, 300, 16383, 16384, u32::MAX as u64, u64::MAX - 1, u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(out.len(), varint_len(v), "{v}");
            let mut pos = 0;
            assert_eq!(get_varint(&out, &mut pos), Some(v));
            assert_eq!(pos, out.len());
        }
        assert_eq!(get_varint(&[0x80], &mut 0), None);
        assert_eq!(get_varint(&[0xff; 11], &mut 0), None);
    }

    #[test]
    fn page_numbers_hash_apart() {
        use std::hash::{BuildHasher, Hasher};
        // Numbers in order, as they are handed out, share no bucket in a
        // table sized for them, at either end of the hash.
        let hash = |n: u32| {
            let mut h = PageHash::default().build_hasher();
            h.write_u32(n);
            h.finish()
        };
        let low: std::collections::HashSet<u64> = (0..4096).map(|n| hash(n) & 4095).collect();
        assert!(low.len() > 2500, "{} of 4096 low ends differ", low.len());
        let high: std::collections::HashSet<u64> = (0..4096).map(|n| hash(n) >> 57).collect();
        assert_eq!(high.len(), 128);
        let mut map: PageMap<u32> = PageMap::default();
        for n in 0..10_000 {
            map.insert(n * 7, n);
        }
        assert!((0..10_000).all(|n| map.get(&(n * 7)) == Some(&n)));
    }

    #[test]
    fn a_changed_byte_fails_the_checksum() {
        let mut page = sealed(&new_buf(KIND_LEAF));
        assert!(verify(&page));
        page[100] ^= 1;
        assert!(!verify(&page));
    }
}
