//! Keeping the process's memory near what it uses.
//!
//! The server's allocations are of two very different kinds: thousands of
//! page-sized buffers a second, made by one thread and dropped by another,
//! and now and then a request or a text of megabytes. The C library's
//! allocator, left to itself, keeps a pool for every thread and returns
//! little to the system: after the benchmark in `bench/` the process held
//! 230 MB with 60 MB in use. Two settings and an occasional trim bring
//! that to about 90 MB without slowing a request.
//!
//! - **Fewer pools.** Two per core instead of eight. Fewer still held less
//!   memory but made threads wait for each other: with two pools in all,
//!   reads on four connections ran at half speed.
//! - **Large blocks straight from the system**, and straight back when
//!   freed. Fixing the size at which that happens stops the library from
//!   raising it as large blocks come and go.
//! - **A trim** every few seconds while requests arrive (`trim`, called by
//!   the server), which returns free pages inside the pools.
//! - **Pages apart** (`pages`): the blocks of database pages, which may
//!   live for minutes, are not taken from the library's pools, where they
//!   kept the memory of short-lived requests around them from being
//!   given back. The server's `Allocator` takes them from a pool of its
//!   own, and everything else from the library.
//!
//! A setting the operator has made through the C library's own
//! environment variables is left alone. This module is the one place
//! the server calls the C library directly; it does so only on Linux
//! with glibc, and elsewhere these functions do nothing.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod pages;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod glibc {
    use std::ffi::c_int;

    const M_MMAP_THRESHOLD: c_int = -3;
    const M_ARENA_MAX: c_int = -8;

    extern "C" {
        fn mallopt(param: c_int, value: c_int) -> c_int;
        fn malloc_trim(pad: usize) -> c_int;
    }

    pub fn configure() {
        let unset = |name: &str| std::env::var_os(name).is_none();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        // SAFETY: `mallopt` only sets allocator parameters; it is called
        // once, before any other thread exists.
        unsafe {
            if unset("MALLOC_ARENA_MAX") {
                mallopt(M_ARENA_MAX, (2 * cores).clamp(2, 8) as c_int);
            }
            if unset("MALLOC_MMAP_THRESHOLD_") {
                mallopt(M_MMAP_THRESHOLD, 256 * 1024);
            }
        }
    }

    pub fn trim() {
        // SAFETY: `malloc_trim` takes the allocator's own locks and gives
        // unused pages back to the system; it may be called at any time.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// Sets the allocator up. Call once, before starting any thread.
pub fn configure() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    glibc::configure();
}

/// Gives memory that is free but still held back to the system.
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        glibc::trim();
        PAGES.trim();
    }
}

/// The memory mapped for pages, in bytes, and how much of it is free:
/// none on systems where pages are not kept apart.
pub fn pages_held() -> (usize, usize) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let (slots, free) = PAGES.counts();
        (slots * pages::SLOT, free * pages::SLOT)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    (0, 0)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
static PAGES: pages::Pool = pages::Pool::new();

/// The server's allocator: blocks the size of a page from a pool of
/// their own (`pages`), everything else from the system's allocator.
/// Elsewhere than on Linux with the GNU C library, all of it from the
/// system's.
pub struct Allocator;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
// SAFETY: every block is the system allocator's or the pool's, and goes
// back to the one it came from, told apart by its layout as `GlobalAlloc`
// promises it is given back.
unsafe impl std::alloc::GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if pages::is_page(layout) {
            return PAGES.take();
        }
        std::alloc::System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        if pages::is_page(layout) {
            let slot = PAGES.take();
            if !slot.is_null() {
                slot.write_bytes(0, layout.size());
            }
            return slot;
        }
        std::alloc::System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        if pages::is_page(layout) {
            return PAGES.give(ptr);
        }
        std::alloc::System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        let new = std::alloc::Layout::from_size_align_unchecked(new_size, layout.align());
        if !pages::is_page(layout) && !pages::is_page(new) {
            return std::alloc::System.realloc(ptr, layout, new_size);
        }
        let moved = self.alloc(new);
        if !moved.is_null() {
            std::ptr::copy_nonoverlapping(ptr, moved, layout.size().min(new_size));
            self.dealloc(ptr, layout);
        }
        moved
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
// SAFETY: the system allocator's, unchanged.
unsafe impl std::alloc::GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        std::alloc::System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        std::alloc::System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        std::alloc::System.realloc(ptr, layout, new_size)
    }
}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod tests {
    use database_engine::page::{Page, PAGE_SIZE};

    #[test]
    fn a_page_is_taken_from_the_pool_of_pages() {
        let page: Page = std::sync::Arc::from(vec![7u8; PAGE_SIZE]);
        let (mapped, free) = super::pages_held();
        assert!(mapped > 0 && free < mapped, "{mapped} bytes mapped for pages, {free} free");
        assert_eq!(page[PAGE_SIZE - 1], 7);
    }
}
