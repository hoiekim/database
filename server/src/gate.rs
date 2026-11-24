//! A database's queue of writers.
//!
//! One writer runs at a time per database, and the others wait their
//! turn here, in order, without holding a thread.
//!
//! What waits is one of two things. A **job** is a small piece of work
//! that needs the turn only while it runs: a statement that is its own
//! transaction, or the commit of a transaction that ran beside the
//! writers. A waiter for the **turn** itself will keep it across several
//! requests, or run something long on a thread of its own.
//!
//! Jobs do not take turns one by one. Whoever holds the turn when it
//! ends runs the jobs that have gathered behind it, back to back, on its
//! own thread; each job tells its connection the outcome, and the
//! connection sends the answer from its own task. So between two writes
//! nothing waits for a task to be woken and scheduled, which costs more
//! than a small write does, and the reading, decoding and answering of
//! requests goes on on the other threads meanwhile.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

/// Work that needs the turn while it runs. It runs on a thread that
/// serves other connections too, so it is brief, and it reports its own
/// outcome.
pub(crate) type Job = Box<dyn FnOnce() + Send>;

enum Waiter {
    Job(Job),
    Turn(oneshot::Sender<Turn>),
}

struct Inner {
    /// Whether someone has the turn.
    held: bool,
    /// Whether whoever has it is running jobs, and so will be done in a
    /// moment.
    brief: bool,
    queue: VecDeque<Waiter>,
}

pub(crate) struct Gate {
    inner: Mutex<Inner>,
    runtime: tokio::runtime::Handle,
}

/// The turn to write. Giving it up (dropping it) passes it on.
pub(crate) struct Turn {
    /// `None` once the turn has gone back without being passed on.
    gate: Option<Arc<Gate>>,
}

/// Jobs one holder runs for others before it goes back to its own work,
/// leaving the rest to a task of their own: its connection waits for its
/// answer meanwhile.
const BATCH: usize = 16;

/// How long a job waits on its own thread for the job ahead of it,
/// before it joins the queue.
const SPIN: std::time::Duration = std::time::Duration::from_micros(30);

impl Gate {
    /// Must be called inside the runtime that will serve the connections.
    pub fn new() -> Arc<Gate> {
        Arc::new(Gate {
            inner: Mutex::new(Inner {
                held: false,
                brief: false,
                queue: VecDeque::new(),
            }),
            runtime: tokio::runtime::Handle::current(),
        })
    }

    /// Runs `job` when its turn comes: now, on this thread, if nobody
    /// has the turn; otherwise later, on the thread of whoever does.
    pub fn run(self: &Arc<Self>, job: Job) {
        let began = std::time::Instant::now();
        let mut spins = 0u32;
        loop {
            let mut inner = self.inner.lock().unwrap();
            if !inner.held {
                inner.held = true;
                inner.brief = true;
                break;
            }
            // Behind a job, and nothing else: it ends within microseconds,
            // sooner than this connection's task could be put to sleep
            // and woken. Wait for it here, a moment.
            if !inner.brief
                || !inner.queue.is_empty()
                || (spins % 16 == 15 && began.elapsed() > SPIN)
            {
                inner.queue.push_back(Waiter::Job(job));
                return;
            }
            drop(inner);
            spins += 1;
            for _ in 0..8 {
                std::hint::spin_loop();
            }
        }
        let turn = Turn {
            gate: Some(self.clone()),
        };
        job();
        // Those that gathered meanwhile: a few here, where the data they
        // touch is at hand, the rest as the turn is passed on.
        turn.run_waiting(BATCH);
    }

    /// Waits for the turn itself, for at most `wait`.
    pub async fn turn(self: &Arc<Self>, wait: Duration) -> Option<Turn> {
        let coming = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.held {
                inner.held = true;
                inner.brief = false;
                return Some(Turn {
                    gate: Some(self.clone()),
                });
            }
            let (give, coming) = oneshot::channel();
            inner.queue.push_back(Waiter::Turn(give));
            coming
        };
        // Given up on, the turn is refused when it is offered (see
        // `pass`), or handed straight back if it had already arrived.
        match tokio::time::timeout(wait, coming).await {
            Ok(Ok(turn)) => Some(turn),
            _ => None,
        }
    }

    /// The next waiting job, if a job is what waits first.
    fn next_job(&self) -> Option<Job> {
        let mut inner = self.inner.lock().unwrap();
        match inner.queue.front() {
            Some(Waiter::Job(_)) => match inner.queue.pop_front() {
                Some(Waiter::Job(job)) => Some(job),
                _ => unreachable!("the front was a job"),
            },
            _ => None,
        }
    }

    /// Gives the turn to whoever waits first, or to nobody.
    fn pass(self: &Arc<Self>) {
        loop {
            let give = {
                let mut inner = self.inner.lock().unwrap();
                match inner.queue.front() {
                    None => {
                        inner.held = false;
                        return;
                    }
                    Some(Waiter::Job(_)) => {
                        inner.brief = true;
                        None
                    }
                    Some(Waiter::Turn(_)) => match inner.queue.pop_front() {
                        Some(Waiter::Turn(give)) => {
                            inner.brief = false;
                            Some(give)
                        }
                        _ => unreachable!("the front wanted the turn"),
                    },
                }
            };
            match give {
                // Jobs: a task of their own runs them, holding the turn.
                None => {
                    let turn = Turn {
                        gate: Some(self.clone()),
                    };
                    self.runtime.spawn(async move {
                        loop {
                            if !turn.run_some(BATCH) {
                                break;
                            }
                            // Let the connections' own tasks at this
                            // thread now and then.
                            tokio::task::yield_now().await;
                        }
                    });
                    return;
                }
                Some(give) => match give.send(Turn {
                    gate: Some(self.clone()),
                }) {
                    Ok(()) => return,
                    // Its waiter gave up: the turn is still ours to pass.
                    Err(mut turn) => {
                        turn.gate.take();
                    }
                },
            }
        }
    }
}

impl Turn {
    /// Runs up to `most` waiting jobs. Returns whether that many ran, and
    /// so whether more may be waiting.
    fn run_some(&self, most: usize) -> bool {
        let Some(gate) = &self.gate else { return false };
        for _ in 0..most {
            match gate.next_job() {
                Some(job) => job(),
                None => return false,
            }
        }
        true
    }

    /// Runs up to `most` waiting jobs, then gives the turn up.
    fn run_waiting(self, most: usize) {
        self.run_some(most);
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            gate.pass();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn jobs_run_one_at_a_time_and_all_of_them() {
        let gate = Gate::new();
        let running = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let (gate, running, done) = (gate.clone(), running.clone(), done.clone());
            tasks.push(tokio::spawn(async move {
                for _ in 0..500 {
                    let (tell, told) = oneshot::channel();
                    let (running, done) = (running.clone(), done.clone());
                    gate.run(Box::new(move || {
                        assert_eq!(
                            running.fetch_add(1, Ordering::SeqCst),
                            0,
                            "two jobs ran at once"
                        );
                        std::hint::spin_loop();
                        running.fetch_sub(1, Ordering::SeqCst);
                        done.fetch_add(1, Ordering::SeqCst);
                        let _ = tell.send(());
                    }));
                    told.await.unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(done.load(Ordering::SeqCst), 16 * 500);
        // Nothing is left holding the turn.
        assert!(gate.turn(Duration::ZERO).await.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_held_turn_keeps_jobs_and_other_holders_waiting_in_order() {
        let gate = Gate::new();
        let turn = gate.turn(Duration::ZERO).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (tell, first_job) = oneshot::channel();
        let seen = order.clone();
        gate.run(Box::new(move || {
            seen.lock().unwrap().push("job 1");
            let _ = tell.send(());
        }));
        let second = {
            let (gate, order) = (gate.clone(), order.clone());
            tokio::spawn(async move {
                let turn = gate.turn(Duration::from_secs(10)).await.unwrap();
                order.lock().unwrap().push("turn 2");
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(turn);
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (tell, last_job) = oneshot::channel();
        let seen = order.clone();
        gate.run(Box::new(move || {
            seen.lock().unwrap().push("job 3");
            let _ = tell.send(());
        }));
        // One that will not wait is refused, and does not jam the queue.
        assert!(gate.turn(Duration::from_millis(1)).await.is_none());
        assert!(order.lock().unwrap().is_empty());
        drop(turn);
        first_job.await.unwrap();
        second.await.unwrap();
        last_job.await.unwrap();
        assert_eq!(*order.lock().unwrap(), ["job 1", "turn 2", "job 3"]);
        assert!(gate.turn(Duration::ZERO).await.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_that_panics_passes_the_turn_on() {
        let gate = Gate::new();
        let held = gate.turn(Duration::ZERO).await.unwrap();
        gate.run(Box::new(|| panic!("a job failed")));
        let (tell, told) = oneshot::channel();
        gate.run(Box::new(move || {
            let _ = tell.send(());
        }));
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), told)
            .await
            .unwrap()
            .unwrap();
        assert!(gate.turn(Duration::from_secs(5)).await.is_some());
    }
}
