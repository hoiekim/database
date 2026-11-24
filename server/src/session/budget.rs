//! Memory for large requests in flight.
//!
//! A request is read whole before it is answered, and while it waits for
//! its turn to write it is held twice: as the bytes that came, and as the
//! values they decode to. Four connections that each load megabyte
//! statements held that much four times over, and the allocator, which
//! keeps a pool of memory per thread, kept most of it after it was
//! freed. So a large request, once its bytes have all come, takes a share
//! of a fixed budget before it is decoded, and gives it back once it has
//! been answered; one that finds the budget spent waits with its bytes
//! in hand, as they would be on a server without the budget. What the
//! budget bounds is the decoding, the cutting of texts into words and
//! the wait for the turn, not the bytes of requests waiting for it.
//!
//! Nothing shared is held while the server waits on a client: the share
//! is taken only after the whole frame has come, and a dump, which
//! writes to the client as it goes, gives its share back first. A client
//! that sends a large statement slowly, or announces one and goes quiet,
//! keeps only its own connection waiting.
//!
//! As a large request is answered, the memory it held is handed back to
//! the system (at most five times a second), rather than left in the
//! allocator's pool of whichever thread decoded it: where the allocator
//! is the GNU C library's, which keeps such pools. Elsewhere only the
//! budget bounds the memory.
//!
//! - A request of ordinary size (`BRIEF_BYTES` or less) takes nothing
//!   and never waits.
//! - A request larger than the whole budget takes all of it, and so runs
//!   alone among large requests.
//! - A connection that holds the turn to write between its requests (an
//!   exclusive transaction) never waits: another connection may hold the
//!   budget while it waits for that turn. It takes what is free, if any,
//!   and runs.
//! - The budget is taken in arrival order, so a large request is not
//!   passed over for ever by smaller large ones.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, SemaphorePermit};

use super::BRIEF_BYTES;

/// The memory the large requests being decoded or answered may take
/// together, counted as their bytes on the wire. Of four connections
/// that load megabyte statements, three have one decoded at a time: one
/// writing, two cutting their texts into words or waiting for the turn.
/// The fourth waits with its statement's bytes in hand, not decoded.
pub(super) const BUDGET_BYTES: usize = 4 * 1024 * 1024;

/// How often, at most, the memory that large requests held is given
/// back to the system as they are answered. A trim takes the allocator's
/// locks for a moment; the server also trims every few seconds while
/// requests arrive, which is too seldom for a load of megabyte requests.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const TRIM_EVERY: Duration = Duration::from_millis(200);

/// What the budget is counted in.
const UNIT: usize = 4 * 1024;

/// A budget of `bytes`, shared by every connection of the process.
pub(super) struct Budget {
    units: Semaphore,
    total: u32,
}

/// A share of the budget, given back when dropped.
pub(super) struct Share(#[allow(dead_code)] Option<SemaphorePermit<'static>>);

impl Share {
    /// Gives the share back, once the request it was for is answered,
    /// and the memory the request held to the system, if that was not
    /// done a moment ago.
    pub(super) fn give_back(self) {
        let Some(permit) = self.0 else { return };
        drop(permit);
        trim_soon();
    }
}

/// Has the allocator give free memory back to the system, unless it did
/// a moment ago. Only the GNU C library keeps a pool per thread and is
/// asked to (`memory::trim`); with any other, the budget is all there is.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_soon() {
    // Milliseconds since the first trim, plus one interval, at the last
    // trim: zero means none yet.
    static START: OnceLock<Instant> = OnceLock::new();
    static LAST: AtomicU64 = AtomicU64::new(0);
    let every = TRIM_EVERY.as_millis() as u64;
    let now = START.get_or_init(Instant::now).elapsed().as_millis() as u64 + every;
    let last = LAST.load(Ordering::Relaxed);
    if now >= last + every && LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        tokio::task::spawn_blocking(crate::memory::trim);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_soon() {}

impl Budget {
    pub(super) const fn new(bytes: usize) -> Budget {
        let total = (bytes / UNIT) as u32;
        Budget { units: Semaphore::const_new(total as usize), total }
    }

    /// The share a request of `bytes` needs: none for one of ordinary
    /// size, and at most the whole budget.
    fn units_for(&self, bytes: usize) -> u32 {
        if bytes <= BRIEF_BYTES {
            return 0;
        }
        bytes.div_ceil(UNIT).min(self.total as usize) as u32
    }

    /// Takes the share of a request of `bytes`, waiting for it if
    /// `may_wait`; otherwise takes it only if it is free, and returns an
    /// empty share if not.
    pub(super) async fn take(&'static self, bytes: usize, may_wait: bool) -> Share {
        let units = self.units_for(bytes);
        if units == 0 {
            return Share(None);
        }
        if !may_wait {
            return Share(self.units.try_acquire_many(units).ok());
        }
        // The semaphore is never closed.
        Share(self.units.acquire_many(units).await.ok())
    }

    /// Units free now: for tests.
    #[cfg(test)]
    fn free(&self) -> usize {
        self.units.available_permits()
    }
}

/// The budget of this process's server.
pub(super) fn budget() -> &'static Budget {
    static BUDGET: OnceLock<Budget> = OnceLock::new();
    BUDGET.get_or_init(|| Budget::new(BUDGET_BYTES))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn leak(bytes: usize) -> &'static Budget {
        Box::leak(Box::new(Budget::new(bytes)))
    }

    #[tokio::test]
    async fn a_small_request_never_waits() {
        let budget = leak(1024 * 1024);
        let all = budget.take(10 * 1024 * 1024, true).await;
        assert_eq!(budget.free(), 0);
        // Spent, and still a request of ordinary size goes at once.
        let small = tokio::time::timeout(Duration::from_millis(100), budget.take(BRIEF_BYTES, true)).await;
        assert!(small.is_ok());
        drop(all);
        assert_eq!(budget.free(), 1024 * 1024 / UNIT);
    }

    #[tokio::test]
    async fn a_request_larger_than_the_budget_runs_alone() {
        let budget = leak(1024 * 1024);
        let huge = budget.take(64 * 1024 * 1024, true).await;
        let waiting = tokio::spawn(budget.take(200 * 1024, true));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "a large request waits while a larger one runs");
        drop(huge);
        let share = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("it goes once the budget is back").unwrap();
        assert_eq!(budget.free(), (1024 * 1024 - 200 * 1024) / UNIT);
        drop(share);
    }

    #[tokio::test]
    async fn the_holder_of_the_turn_does_not_wait() {
        let budget = leak(1024 * 1024);
        let all = budget.take(1024 * 1024, true).await;
        // Whoever holds the budget may be waiting for this connection's
        // turn: this one takes nothing and goes on.
        let share = tokio::time::timeout(Duration::from_millis(100), budget.take(512 * 1024, false)).await.expect("it does not wait");
        assert!(share.0.is_none());
        drop(all);
        let share = budget.take(512 * 1024, false).await;
        assert!(share.0.is_some(), "it takes what is free");
    }

    #[tokio::test]
    async fn large_requests_take_the_budget_in_order() {
        let budget = leak(1024 * 1024);
        let first = budget.take(768 * 1024, true).await;
        // The whole budget is asked for, then a little: the little waits
        // behind the whole, though it would fit now.
        let whole = tokio::spawn(budget.take(1024 * 1024, true));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let little = tokio::spawn(budget.take(128 * 1024, true));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!whole.is_finished() && !little.is_finished());
        drop(first);
        let whole = tokio::time::timeout(Duration::from_secs(5), whole).await.unwrap().unwrap();
        assert!(!little.is_finished());
        drop(whole);
        tokio::time::timeout(Duration::from_secs(5), little).await.unwrap().unwrap();
    }
}
