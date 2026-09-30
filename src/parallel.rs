//! The pool a refresh fans out over: `std::thread::scope` and one atomic
//! counter, which is all a bounded walk over a slice needs.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// Runs `work` over `items` on up to `limit` threads, the calling thread
/// included. Before each item the calling thread takes, `between` runs; it
/// returning false stops the walk — the other threads finish the item they
/// are on and claim no more. Returns false when the walk was stopped.
///
/// At a limit of one this is a plain loop on the calling thread with
/// `between` before every item, which is what the tests run at.
#[must_use]
pub fn each<T: Sync>(
    items: &[T],
    limit: usize,
    mut between: impl FnMut() -> bool,
    work: impl Fn(usize, &T) + Sync,
) -> bool {
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let claim = || {
        (!stopped.load(Relaxed))
            .then(|| next.fetch_add(1, Relaxed))
            .filter(|&index| index < items.len())
    };
    thread::scope(|scope| {
        for _ in 1..limit.clamp(1, items.len().max(1)) {
            scope.spawn(|| {
                while let Some(index) = claim() {
                    work(index, &items[index]);
                }
            });
        }
        loop {
            if !between() {
                stopped.store(true, Relaxed);
                break;
            }
            let Some(index) = claim() else { break };
            work(index, &items[index]);
        }
    });
    !stopped.load(Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_is_read_once_across_threads() {
        let reads: Vec<AtomicUsize> = (0..50).map(|_| AtomicUsize::new(0)).collect();
        let read_once = |_, read: &AtomicUsize| _ = read.fetch_add(1, Relaxed);
        assert!(each(&reads, 4, || true, read_once));
        assert!(reads.iter().all(|read| read.load(Relaxed) == 1));
        assert!(each(&reads[..0], 4, || true, |_, _| unreachable!()));
    }

    #[test]
    fn between_runs_before_each_item_this_thread_takes_and_can_stop_the_walk() {
        let items: Vec<usize> = (0..100).collect();
        let visited = AtomicUsize::new(0);
        let mut turns = 0;
        let finished = each(
            &items,
            1,
            || {
                turns += 1;
                turns <= 3
            },
            |_, _| {
                visited.fetch_add(1, Relaxed);
            },
        );
        assert!(!finished, "a stopped walk says so");
        assert_eq!(visited.load(Relaxed), 3, "three items, then stop");
    }
}
