//! A byte budget the threads rendering pages at once share, so a pool of workers at a
//! high resolution holds about as much memory as the budget, not one page per worker.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

/// Bytes the pages rendering at once may hold.
pub(crate) const DEFAULT_BYTES: u64 = 2 << 30;

/// Bytes one pixel costs while its page renders and saves: the RGBA pixmap (4) with its
/// RGB copy (3), then, once the pixmap is dropped, the RGB copy with the filtered PNG rows
/// (3) and the compressed stream (at most 3) or its copy into the file's bytes (3), with
/// room to spare.
pub(crate) const BYTES_PER_PIXEL: u64 = 11;

pub(crate) struct Budget {
    capacity: u64,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    free: u64,
    /// The ticket the next caller of [`Budget::reserve`] takes.
    next: u64,
    /// The ticket allowed to reserve now; later tickets wait their turn.
    serving: u64,
}

/// Bytes held until this drops.
#[must_use]
pub(crate) struct Reservation<'a> {
    budget: &'a Budget,
    bytes: u64,
}

impl Budget {
    pub(crate) fn new(capacity: u64) -> Budget {
        let capacity = capacity.max(1);
        Budget {
            capacity,
            state: Mutex::new(State {
                free: capacity,
                next: 0,
                serving: 0,
            }),
            changed: Condvar::new(),
        }
    }

    /// Waits until `bytes` are free, first come first served, and holds them. A request
    /// over the whole budget takes the whole budget, so that page renders alone.
    pub(crate) fn reserve(&self, bytes: u64) -> Reservation<'_> {
        let bytes = bytes.min(self.capacity);
        let mut state = self.lock();
        let ticket = state.next;
        state.next += 1;
        while state.serving != ticket || state.free < bytes {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.serving += 1;
        state.free -= bytes;
        drop(state);
        self.changed.notify_all();
        Reservation {
            budget: self,
            bytes,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.budget.lock().free += self.bytes;
        self.budget.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::Budget;

    #[test]
    fn threads_never_hold_more_than_the_budget() {
        let budget = Budget::new(100);
        let held = AtomicU64::new(0);
        let peak = AtomicU64::new(0);
        thread::scope(|scope| {
            for worker in 0..8u64 {
                let (budget, held, peak) = (&budget, &held, &peak);
                scope.spawn(move || {
                    for round in 0..20u64 {
                        let bytes = 10 + (worker * 7 + round * 13) % 50;
                        let _reservation = budget.reserve(bytes);
                        let now = held.fetch_add(bytes, Ordering::SeqCst) + bytes;
                        peak.fetch_max(now, Ordering::SeqCst);
                        thread::sleep(Duration::from_micros(200));
                        held.fetch_sub(bytes, Ordering::SeqCst);
                    }
                });
            }
        });
        let peak = peak.into_inner();
        assert!(peak <= 100, "peak {peak}");
        assert!(peak > 60, "pages ran together: peak {peak}");
    }

    #[test]
    fn a_request_over_the_budget_waits_for_the_whole_budget_and_runs_alone() {
        let budget = Budget::new(100);
        let small = budget.reserve(30);
        thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                let _all = budget.reserve(500);
                assert_eq!(budget.lock().free, 0);
            });
            thread::sleep(Duration::from_millis(20));
            assert!(!waiting.is_finished(), "it waited for the held bytes");
            drop(small);
            waiting.join().unwrap();
        });
        assert_eq!(budget.lock().free, 100);
    }
}
