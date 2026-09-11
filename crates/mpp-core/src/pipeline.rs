//! An ordered work pipeline: many workers produce items by index, one
//! consumer takes them in index order, and a window bounds how far the
//! producers may run ahead of the consumer.
//!
//! Both the TIFF encoder (independent strips/tiles compressed in parallel,
//! written in file order) and the TIFF decoder (independent chunks decoded
//! in parallel, delivered in chunk order) have exactly this shape, so the
//! bookkeeping lives here once. Workers call [`Pipeline::claim`] for the
//! next index and [`Pipeline::complete`] with the result; the consumer
//! calls [`Pipeline::take`] for each index in turn and [`Pipeline::taken`]
//! once it has finished with it, which lets the window advance. Either side
//! can [`Pipeline::fail`], which wakes and stops everyone.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

/// Shared state of one ordered pipeline over `len` items producing `R`.
pub struct Pipeline<R> {
    next: AtomicUsize,
    len: usize,
    window: usize,
    state: Mutex<State<R>>,
    changed: Condvar,
}

struct State<R> {
    /// Items the consumer has finished with (`0..taken` are done).
    taken: usize,
    /// Completed items awaiting the consumer, by index.
    ready: BTreeMap<usize, R>,
    /// First failure from either side; stops everyone.
    failed: Option<String>,
}

impl<R> Pipeline<R> {
    /// A pipeline over indices `0..len` in which a worker may claim index
    /// `i` only once the consumer has taken every index below `i - window`
    /// — so at most `window` items are ever claimed-but-not-taken.
    pub fn new(len: usize, window: usize) -> Self {
        Self {
            next: AtomicUsize::new(0),
            len,
            window: window.max(1),
            state: Mutex::new(State {
                taken: 0,
                ready: BTreeMap::new(),
                failed: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<R>> {
        // A poisoned lock means a worker panicked mid-update; the state is
        // still consistent (every update is a single insert/assignment).
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Worker side: the next index to produce, once it is inside the
    /// window, or `None` when every index has been claimed or the pipeline
    /// has failed.
    pub fn claim(&self) -> Option<usize> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed);
        if idx >= self.len {
            return None;
        }
        let mut st = self.lock();
        while st.failed.is_none() && idx >= st.taken + self.window {
            st = self.changed.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        if st.failed.is_some() {
            return None;
        }
        Some(idx)
    }

    /// Worker side: item `idx` is produced.
    pub fn complete(&self, idx: usize, item: R) {
        let mut st = self.lock();
        st.ready.insert(idx, item);
        self.changed.notify_all();
    }

    /// Either side: stop the pipeline with `message`. The first failure
    /// wins; later ones are dropped.
    pub fn fail(&self, message: String) {
        let mut st = self.lock();
        st.failed.get_or_insert(message);
        self.changed.notify_all();
    }

    /// Consumer side: item `idx`, blocking until a worker has produced it,
    /// or the pipeline's failure message.
    pub fn take(&self, idx: usize) -> Result<R, String> {
        let mut st = self.lock();
        loop {
            if let Some(failed) = &st.failed {
                return Err(failed.clone());
            }
            if let Some(item) = st.ready.remove(&idx) {
                return Ok(item);
            }
            st = self.changed.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Consumer side: item `idx` is finished with; workers may advance.
    pub fn taken(&self, idx: usize) {
        let mut st = self.lock();
        st.taken = idx + 1;
        self.changed.notify_all();
    }
}

/// Worker threads to use for `items` independent pieces of work: every
/// core up to `max`, never more than there are items, at least one.
pub fn worker_count(items: usize, max: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, max.max(1))
        .clamp(1, items.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_come_out_in_index_order_whatever_the_completion_order() {
        let p = Pipeline::<usize>::new(50, 4);
        let out: Vec<usize> = std::thread::scope(|s| {
            for t in 0..3 {
                let p = &p;
                s.spawn(move || {
                    while let Some(i) = p.claim() {
                        // Uneven per-worker delays scramble completion order.
                        std::thread::sleep(std::time::Duration::from_micros(
                            (i * 7 % 5 + t) as u64,
                        ));
                        p.complete(i, i * 10);
                    }
                });
            }
            (0..50)
                .map(|i| {
                    let v = p.take(i).unwrap();
                    p.taken(i);
                    v
                })
                .collect()
        });
        assert_eq!(out, (0..50).map(|i| i * 10).collect::<Vec<_>>());
    }

    #[test]
    fn a_failure_stops_workers_and_surfaces_to_the_consumer() {
        let p = Pipeline::<()>::new(1000, 2);
        let result = std::thread::scope(|s| {
            let worker = s.spawn(|| {
                let mut produced = 0;
                while let Some(i) = p.claim() {
                    if i == 3 {
                        p.fail("boom".to_string());
                        break;
                    }
                    p.complete(i, ());
                    produced += 1;
                }
                produced
            });
            let mut r = Ok(());
            for i in 0..1000 {
                r = p.take(i);
                if r.is_err() {
                    break;
                }
                p.taken(i);
            }
            assert!(worker.join().unwrap() <= 3);
            r
        });
        assert_eq!(result, Err("boom".to_string()));
    }

    #[test]
    fn the_window_bounds_how_far_workers_run_ahead() {
        let p = Pipeline::<usize>::new(10, 2);
        // Without the consumer taking anything, only indices 0 and 1 may be
        // claimed; a third claim must block. Probe that with a timeout.
        assert_eq!(p.claim(), Some(0));
        assert_eq!(p.claim(), Some(1));
        let blocked = std::thread::scope(|s| {
            let h = s.spawn(|| p.claim());
            std::thread::sleep(std::time::Duration::from_millis(50));
            let still_blocked = !h.is_finished();
            p.complete(0, 0);
            assert_eq!(p.take(0), Ok(0));
            p.taken(0);
            assert_eq!(h.join().unwrap(), Some(2));
            still_blocked
        });
        assert!(blocked, "the third claim should have waited on the window");
    }
}
