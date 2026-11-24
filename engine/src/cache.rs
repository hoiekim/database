//! The page cache: the one place the engine's memory use is decided.
//!
//! It holds pages that are also on disk, so any of them can be dropped. All
//! databases of a server share one cache and one budget. Pages not yet on
//! disk live in their store's dirty set instead (see `store.rs`), which
//! writes them out before it grows large.
//!
//! Eviction is CLOCK: a hand sweeps the slots, sparing each page touched
//! since it last passed. The slots are split into shards, each with its own
//! lock, so readers on different pages rarely wait for each other.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::page::{Page, PageId, PAGE_SIZE};

const SHARDS: usize = 8;

type Key = (u32, PageId);

struct Slot {
    key: Key,
    page: Page,
    touched: bool,
}

#[derive(Default)]
struct Shard {
    map: HashMap<Key, usize, crate::page::PageHash>,
    slots: Vec<Slot>,
    hand: usize,
}

pub struct PageCache {
    shards: Vec<Mutex<Shard>>,
    per_shard: usize,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl PageCache {
    /// A cache that holds about `bytes` of pages, and at least a few per
    /// shard so a scan can always make progress.
    pub fn new(bytes: usize) -> Self {
        let per_shard = (bytes / PAGE_SIZE / SHARDS).max(4);
        PageCache {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
            per_shard,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn shard(&self, key: Key) -> &Mutex<Shard> {
        let h = (key.1 as usize).wrapping_mul(0x9E37_79B1) ^ (key.0 as usize).wrapping_mul(0x85EB_CA6B);
        &self.shards[(h >> 7) % SHARDS]
    }

    pub fn get(&self, store: u32, id: PageId) -> Option<Page> {
        let key = (store, id);
        let mut shard = self.shard(key).lock().unwrap();
        match shard.map.get(&key).copied() {
            Some(i) => {
                shard.slots[i].touched = true;
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(shard.slots[i].page.clone())
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn insert(&self, store: u32, id: PageId, page: Page) {
        let key = (store, id);
        let mut shard = self.shard(key).lock().unwrap();
        if let Some(i) = shard.map.get(&key).copied() {
            shard.slots[i].page = page;
            shard.slots[i].touched = true;
            return;
        }
        if shard.slots.len() < self.per_shard {
            let i = shard.slots.len();
            shard.slots.push(Slot { key, page, touched: false });
            shard.map.insert(key, i);
            return;
        }
        // Full: sweep for a page not touched since the hand last passed.
        loop {
            let i = shard.hand;
            shard.hand = (i + 1) % shard.slots.len();
            if shard.slots[i].touched {
                shard.slots[i].touched = false;
                continue;
            }
            let old = shard.slots[i].key;
            shard.map.remove(&old);
            shard.slots[i] = Slot { key, page, touched: false };
            shard.map.insert(key, i);
            return;
        }
    }

    pub fn remove(&self, store: u32, id: PageId) {
        let key = (store, id);
        let mut shard = self.shard(key).lock().unwrap();
        let Some(i) = shard.map.remove(&key) else { return };
        let last = shard.slots.len() - 1;
        shard.slots.swap_remove(i);
        if i != last {
            let moved = shard.slots[i].key;
            shard.map.insert(moved, i);
        }
        if shard.hand >= shard.slots.len() {
            shard.hand = 0;
        }
    }

    /// Drops every page of one store, when its database is closed or dropped.
    pub fn remove_store(&self, store: u32) {
        for shard in &self.shards {
            let mut shard = shard.lock().unwrap();
            let slots = std::mem::take(&mut shard.slots);
            shard.map.clear();
            shard.hand = 0;
            for slot in slots {
                if slot.key.0 != store {
                    let i = shard.slots.len();
                    shard.map.insert(slot.key, i);
                    shard.slots.push(slot);
                }
            }
        }
    }

    /// Pages held right now.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().slots.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.per_shard * SHARDS
    }

    /// Lookups that found their page, and those that did not.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits.load(Ordering::Relaxed), self.misses.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn page(tag: u8) -> Page {
        Arc::from(vec![tag; PAGE_SIZE])
    }

    #[test]
    fn holds_no_more_than_its_capacity() {
        let cache = PageCache::new(64 * PAGE_SIZE);
        for id in 0..10_000 {
            cache.insert(1, id, page(id as u8));
        }
        assert!(cache.len() <= cache.capacity());
        assert!(cache.len() >= cache.capacity() / 2);
    }

    #[test]
    fn a_page_in_use_outlives_its_idle_neighbours() {
        let cache = PageCache::new(64 * PAGE_SIZE);
        cache.insert(1, 7, page(7));
        for id in 100..5_000 {
            cache.get(1, 7);
            cache.insert(1, id, page(0));
        }
        assert_eq!(cache.get(1, 7).unwrap()[0], 7);
    }

    #[test]
    fn removal_leaves_the_rest_reachable() {
        let cache = PageCache::new(1024 * PAGE_SIZE);
        for id in 0..200 {
            cache.insert(1, id, page(id as u8));
            cache.insert(2, id, page(id as u8));
        }
        for id in (0..200).step_by(2) {
            cache.remove(1, id);
        }
        for id in 0..200 {
            assert_eq!(cache.get(1, id).is_some(), id % 2 == 1);
            assert_eq!(cache.get(2, id).unwrap()[0], id as u8);
        }
        cache.remove_store(2);
        assert!(cache.get(2, 3).is_none());
        assert_eq!(cache.get(1, 3).unwrap()[0], 3);
    }
}
