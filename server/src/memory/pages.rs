//! Memory for pages, kept apart from the C library's pools.
//!
//! A page is allocated by whichever thread reads or writes it, and may
//! then stay in the cache for minutes, among the short-lived memory of
//! the request that made it: the values it decoded, the words it cut,
//! the records it logged. When that memory is freed, the page still in
//! use keeps the pool around it from shrinking. Under a bulk load from
//! four connections the C library's pools spanned 134 to 153 MB, of
//! which 43 to 53 MB was in use; with the pages kept here, they spanned
//! 36 to 42 MB, and the pages' own memory 47 to 49 MB.
//!
//! So every block of a page's size is taken from a pool of its own:
//! slots of exactly that size, in regions mapped from the system, with
//! a stack of the free ones. A slot freed is taken again before any
//! more memory is mapped, so the pool never holds more than the most
//! pages that were in use at one time: what the cache, the committed
//! pages waiting to be written and the open transactions may hold
//! (`docs/OPERATIONS.md`, "Memory"). `trim` gives the memory of free
//! slots beyond a small reserve back to the system; the slots stay, and
//! are filled again from zeroed memory when they are next taken.

use std::alloc::Layout;
use std::ffi::{c_int, c_long, c_void};
use std::sync::Mutex;

use database_engine::page::PAGE_SIZE;

/// The size of the block a page is allocated as: the page and the two
/// counts of the `Arc` that holds it.
pub(super) const SLOT: usize = PAGE_SIZE + 2 * std::mem::size_of::<usize>();

/// Slots mapped at a time: 2 MB.
const CHUNK_SLOTS: usize = 255;

/// The free slots `trim` leaves as they are, ready to be taken again
/// without a fault: 4 MB, half of what one write transaction keeps of
/// its own with the default cache, so that a trim between two writes
/// leaves the next most of the pages it takes already in memory. It
/// bounds nothing; the pool is bounded by the pages in use.
const RESERVE: usize = 4 * 1024 * 1024 / SLOT;

const PROT_READ_WRITE: c_int = 0x1 | 0x2;
#[cfg(not(any(target_arch = "mips", target_arch = "mips64", target_arch = "mips32r6", target_arch = "mips64r6")))]
const MAP_PRIVATE_ANONYMOUS: c_int = 0x02 | 0x20;
#[cfg(any(target_arch = "mips", target_arch = "mips64", target_arch = "mips32r6", target_arch = "mips64r6"))]
const MAP_PRIVATE_ANONYMOUS: c_int = 0x02 | 0x800;
const MADV_DONTNEED: c_int = 4;
const OS_PAGE: usize = 4096;

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: c_long) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
}

/// Whether a block of `layout` is a page's.
pub(super) fn is_page(layout: Layout) -> bool {
    layout.size() == SLOT && layout.align() <= 16
}

fn map(len: usize) -> Option<*mut u8> {
    // SAFETY: a new private anonymous mapping, touching nothing else.
    let at = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ_WRITE, MAP_PRIVATE_ANONYMOUS, -1, 0) };
    (at as isize != -1).then_some(at as *mut u8)
}

/// The free slots, and how many slots were ever mapped.
struct Free {
    /// A stack of the free slots' addresses, in memory mapped for it
    /// (the pool cannot allocate from the allocator it is part of).
    stack: *mut usize,
    len: usize,
    cap: usize,
    slots: usize,
}

// SAFETY: the stack is only reached through the mutex that holds it.
unsafe impl Send for Free {}

pub(super) struct Pool {
    free: Mutex<Free>,
}

impl Pool {
    pub(super) const fn new() -> Pool {
        Pool { free: Mutex::new(Free { stack: std::ptr::null_mut(), len: 0, cap: 0, slots: 0 }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Free> {
        // Nothing under the lock panics; a poisoned lock is still sound.
        self.free.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A free slot, mapping more if there is none. Null if the system
    /// has no more memory to give.
    pub(super) fn take(&self) -> *mut u8 {
        let mut free = self.lock();
        if free.len == 0 && !free.grow() {
            return std::ptr::null_mut();
        }
        free.len -= 1;
        // SAFETY: `len` is within the stack, which holds valid slots up to it.
        unsafe { *free.stack.add(free.len) as *mut u8 }
    }

    /// Takes a slot back.
    ///
    /// # Safety
    /// `slot` came from `take` of this pool and is not in use.
    pub(super) unsafe fn give(&self, slot: *mut u8) {
        let mut free = self.lock();
        // The stack has room for every slot ever mapped (`grow`).
        let at = free.len;
        *free.stack.add(at) = slot as usize;
        free.len += 1;
    }

    /// Gives the memory of the free slots beyond the reserve back to the
    /// system. Those taken first stay: the lowest, whose memory is the
    /// likeliest to be in use beside them.
    pub(super) fn trim(&self) {
        let free = self.lock();
        if free.len <= RESERVE {
            return;
        }
        let len = free.len;
        // SAFETY: the first `len` entries of the stack are free slots.
        let slots = unsafe { std::slice::from_raw_parts_mut(free.stack, len) };
        // Highest first: the top of the stack, taken next, is the lowest.
        slots.sort_unstable_by(|a, b| b.cmp(a));
        let cold = &slots[..len - RESERVE];
        // Runs of adjacent slots, released as one range: of a slot alone
        // only the whole system pages within it can be.
        let mut i = 0;
        while i < cold.len() {
            let (high, mut low) = (cold[i] + SLOT, cold[i]);
            i += 1;
            while i < cold.len() && cold[i] + SLOT == low {
                low = cold[i];
                i += 1;
            }
            let (from, to) = (low.next_multiple_of(OS_PAGE), high / OS_PAGE * OS_PAGE);
            if from < to {
                // SAFETY: whole system pages within free slots, which
                // nothing reads until they are taken again; they then
                // read as zeros, which a new block may.
                unsafe { madvise(from as *mut c_void, to - from, MADV_DONTNEED) };
            }
        }
    }

    /// Slots ever mapped, and of them free.
    pub(super) fn counts(&self) -> (usize, usize) {
        let free = self.lock();
        (free.slots, free.len)
    }
}

impl Free {
    /// Maps another chunk of slots, and room on the stack for all of
    /// them.
    fn grow(&mut self) -> bool {
        let slots = self.slots + CHUNK_SLOTS;
        if slots > self.cap {
            let cap = (2 * self.cap).max(slots).max(OS_PAGE / std::mem::size_of::<usize>());
            let Some(stack) = map(cap * std::mem::size_of::<usize>()) else { return false };
            let stack = stack as *mut usize;
            if !self.stack.is_null() {
                // SAFETY: the old stack holds `len` entries and is ours
                // to unmap; the new one has room for them.
                unsafe {
                    std::ptr::copy_nonoverlapping(self.stack, stack, self.len);
                    munmap(self.stack as *mut c_void, self.cap * std::mem::size_of::<usize>());
                }
            }
            self.stack = stack;
            self.cap = cap;
        }
        let Some(chunk) = map(CHUNK_SLOTS * SLOT) else { return false };
        // Pushed highest first, so that the lowest is taken first.
        for i in (0..CHUNK_SLOTS).rev() {
            // SAFETY: the stack has room for `slots` entries.
            unsafe { *self.stack.add(self.len) = chunk as usize + i * SLOT };
            self.len += 1;
        }
        self.slots = slots;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn mincore(addr: *mut c_void, len: usize, vec: *mut u8) -> c_int;
    }

    /// How many of the system pages wholly within `slot` are in memory.
    fn resident(slot: *mut u8) -> usize {
        let from = (slot as usize).next_multiple_of(OS_PAGE);
        let to = (slot as usize + SLOT) / OS_PAGE * OS_PAGE;
        let mut vec = [0u8; 3];
        // SAFETY: the range is mapped, and `vec` has a byte per page.
        assert_eq!(unsafe { mincore(from as *mut c_void, to - from, vec.as_mut_ptr()) }, 0);
        vec[..(to - from) / OS_PAGE].iter().filter(|b| **b & 1 == 1).count()
    }

    #[test]
    fn the_block_of_a_page_is_a_slot() {
        // The layout `Arc<[u8]>` allocates for a page.
        let layout = Layout::new::<[usize; 2]>().extend(Layout::array::<u8>(PAGE_SIZE).unwrap()).unwrap().0.pad_to_align();
        assert!(is_page(layout));
    }

    #[test]
    fn freed_slots_are_taken_again_before_more_memory_is_mapped() {
        let pool = Pool::new();
        let taken: Vec<*mut u8> = (0..3 * CHUNK_SLOTS).map(|_| pool.take()).collect();
        assert!(taken.iter().all(|slot| !slot.is_null() && (*slot as usize).is_multiple_of(16)));
        let mut sorted: Vec<usize> = taken.iter().map(|slot| *slot as usize).collect();
        sorted.sort_unstable();
        assert!(sorted.windows(2).all(|w| w[1] - w[0] >= SLOT), "slots do not overlap");
        assert_eq!(pool.counts(), (3 * CHUNK_SLOTS, 0));
        for (n, slot) in taken.into_iter().enumerate() {
            // SAFETY: a slot of this pool's, written within its size.
            unsafe {
                slot.write_bytes(n as u8, SLOT);
                pool.give(slot);
            }
        }
        // Over and over, and never more than at most at once.
        for _ in 0..10 {
            let taken: Vec<*mut u8> = (0..2 * CHUNK_SLOTS).map(|_| pool.take()).collect();
            for slot in taken {
                // SAFETY: as above.
                unsafe { pool.give(slot) };
            }
        }
        assert_eq!(pool.counts(), (3 * CHUNK_SLOTS, 3 * CHUNK_SLOTS));
    }

    #[test]
    fn a_trim_gives_back_the_memory_of_free_slots_beyond_the_reserve() {
        let pool = Pool::new();
        let count = RESERVE + 2 * CHUNK_SLOTS;
        let taken: Vec<*mut u8> = (0..count).map(|_| pool.take()).collect();
        for slot in &taken {
            // SAFETY: a slot of this pool's, written within its size.
            unsafe { slot.write_bytes(1, SLOT) };
        }
        assert!(taken.iter().all(|slot| resident(*slot) >= 1));
        let kept = taken[0];
        for slot in &taken[1..] {
            // SAFETY: as above.
            unsafe { pool.give(*slot) };
        }
        pool.trim();
        let released = taken[1..].iter().filter(|slot| resident(**slot) == 0).count();
        assert!(released >= count - 1 - RESERVE - 4, "{released} of {} free slots given back", count - 1);
        // The slot in use is untouched; the free ones read as zeros.
        // SAFETY: `kept` is in use, and every slot is mapped.
        unsafe {
            assert!(std::slice::from_raw_parts(kept, SLOT).iter().all(|b| *b == 1));
            let again: Vec<*mut u8> = (0..count - 1).map(|_| pool.take()).collect();
            assert_eq!(pool.counts(), (count.next_multiple_of(CHUNK_SLOTS), count.next_multiple_of(CHUNK_SLOTS) - count));
            assert!(again.iter().any(|slot| std::slice::from_raw_parts(*slot, SLOT).iter().all(|b| *b == 0)));
        }
    }
}
