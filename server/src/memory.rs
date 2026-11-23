//! Keeping the process's memory near what it uses.
//!
//! The server's allocations are of two very different kinds: thousands of
//! page-sized buffers a second, made by one thread and dropped by another,
//! and now and then a request or a mail body of megabytes. The C library's
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
//!
//! A setting the operator has made through the C library's own
//! environment variables is left alone. This is the one place the server
//! calls the C library directly; it does so only on Linux with glibc, and
//! elsewhere these functions do nothing.

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
    glibc::trim();
}
