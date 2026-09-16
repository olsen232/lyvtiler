//! A global allocator that counts live bytes, for `--memory`.
//!
//! Resident set is the pages a process has touched, and allocators keep freed
//! pages, so it can grow while live data shrinks. [`live`] and [`peak`] count
//! what is actually owned. Peak live close to peak RSS means a data structure
//! holds the memory; well under means the allocator is retaining it. No
//! attribution — that's a heap profiler's job.
//!
//! Counting is batched per thread: one global atomic per allocation cost 2.4x
//! in wall time. Each thread keeps a delta and publishes it past `FLUSH`
//! bytes, so [`live`] is accurate to `FLUSH` per running thread. A thread that
//! exits must publish its remainder (see `Drift`): the cut spawns workers per
//! batch, and the lost remainders grew with the input.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicIsize, Ordering};

/// How far one thread's count may drift before it is published.
const FLUSH: isize = 256 * 1024;

/// Signed: a block freed on another thread arrives there as a debit. The total
/// is right, but can pass through negative.
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

/// One thread's unpublished drift, published when the thread exits.
///
/// Without that, each exiting thread's remainder (under [`FLUSH`]) is lost.
/// Short-lived cut workers allocate chains that outlive them, so the loss was
/// one-sided: `live()` once reported 524 MB while one map held 576 MB.
struct Drift(Cell<isize>);

impl Drop for Drift {
    fn drop(&mut self) {
        let left = self.0.replace(0);
        if left != 0 {
            publish(left);
        }
    }
}

thread_local! {
    /// Const-initialised, so touching it inside the allocator doesn't allocate
    /// (which would recurse). The destructor doesn't change that.
    static DELTA: Drift = const { Drift(Cell::new(0)) };
}

/// Bytes currently allocated and not yet freed.
pub fn live() -> usize {
    LIVE.load(Ordering::Relaxed).max(0) as usize
}

/// The most bytes that were live at any one moment.
pub fn peak() -> usize {
    PEAK.load(Ordering::Relaxed).max(0) as usize
}

/// Publish a thread's drift, raising the peak if needed.
fn publish(amount: isize) {
    let now = LIVE.fetch_add(amount, Ordering::Relaxed) + amount;
    let mut seen = PEAK.load(Ordering::Relaxed);
    while now > seen {
        match PEAK.compare_exchange_weak(seen, now, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => seen = actual,
        }
    }
}

/// Count a change, publishing once the thread has drifted far enough.
fn record(delta: isize) {
    let drifted = DELTA.try_with(|drift| {
        let next = drift.0.get() + delta;
        if next.abs() >= FLUSH {
            drift.0.set(0);
            Some(next)
        } else {
            drift.0.set(next);
            None
        }
    });
    match drifted {
        Ok(Some(amount)) => publish(amount),
        Ok(None) => {}
        // The thread-local is gone during teardown; count it globally.
        Err(_) => publish(delta),
    }
}

/// Any allocator, counted. Generic so the shipped allocator (mimalloc) is what
/// gets measured: allocators differ in exactly what they keep.
pub struct Counting<A>(pub A);

/// The system allocator, counted.
pub const fn system() -> Counting<System> {
    Counting(System)
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc(layout) };
        if !p.is_null() {
            record(layout.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(-(layout.size() as isize));
        unsafe { self.0.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc_zeroed(layout) };
        if !p.is_null() {
            record(layout.size() as isize);
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            // One difference, not a free and an allocate, so a grow in place
            // never dips below the truth.
            record(new_size as isize - layout.size() as isize);
        }
        p
    }
}

/// Megabytes, for the one line this prints.
pub fn mb(bytes: usize) -> String {
    format!("{:.0} MB", bytes as f64 / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The allocator isn't installed in this test binary, so these drive the
    // counting directly. The counters are process-wide and tests run in
    // parallel, so they take a lock: one asserts the total doesn't move.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn alone() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_published_change_moves_the_live_total() {
        let _guard = alone();
        let before = live();
        publish(1_000);
        assert_eq!(live(), before + 1_000);
        publish(-1_000);
        assert_eq!(live(), before);
    }

    #[test]
    fn the_peak_remembers_the_highest_moment() {
        let _guard = alone();
        publish(5_000_000);
        let high = peak();
        publish(-5_000_000);
        assert_eq!(peak(), high, "the peak must not fall when memory is freed");
        assert!(high >= 5_000_000);
    }

    #[test]
    fn small_changes_stay_on_the_thread_until_they_add_up() {
        let _guard = alone();
        // Small allocations mustn't touch the shared counter.
        std::thread::spawn(|| {
            let before = LIVE.load(Ordering::Relaxed);
            for _ in 0..10 {
                record(1_024);
            }
            assert_eq!(
                LIVE.load(Ordering::Relaxed),
                before,
                "10 KB of churn should not have been published yet"
            );
            record(FLUSH);
            assert!(
                LIVE.load(Ordering::Relaxed) > before,
                "crossing the threshold should publish"
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_thread_that_frees_what_another_allocated_does_not_corrupt_the_total() {
        let _guard = alone();
        // The freeing thread publishes a debit its counter never saw a credit
        // for.
        let before = live();
        std::thread::spawn(|| publish(2_000_000)).join().unwrap();
        std::thread::spawn(|| publish(-2_000_000)).join().unwrap();
        assert_eq!(live(), before);
    }

    #[test]
    fn counting_delegates_to_whatever_it_wraps() {
        // Every call must pass through and be counted. (A misconfiguration once
        // installed neither allocator, and peak live read zero.)
        let _guard = alone();
        let counted = system();
        let layout = Layout::from_size_align(4096, 8).unwrap();

        let before = live();
        let p = unsafe { counted.alloc(layout) };
        assert!(!p.is_null(), "the inner allocator must actually have run");
        // Writing proves it's real memory.
        unsafe { p.write_bytes(0xAB, 4096) };
        assert_eq!(unsafe { *p }, 0xAB);

        // Under the threshold, so still on the thread: force it out.
        record(FLUSH);
        assert!(live() >= before + 4096, "the allocation was not counted");

        unsafe { counted.dealloc(p, layout) };
        record(-FLUSH);
    }

    #[test]
    fn a_thread_that_dies_owing_the_counter_pays_up() {
        // A thread that exits under FLUSH must still publish; on a real layer
        // the lost remainders were a fifth of the total.
        let _guard = alone();

        const THREADS: isize = 64;
        // Well under FLUSH, so no thread publishes on its own.
        const EACH: isize = 8 * 1024;
        const { assert!(EACH < FLUSH, "the point is that this never flushes") };

        let before = live() as isize;
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| record(EACH));
            }
        });

        // Polled: the destructor runs after `thread::scope` returns, so an
        // immediate assert failed about one run in ten.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut grew = live() as isize - before;
        while grew < THREADS * EACH && std::time::Instant::now() < deadline {
            std::thread::yield_now();
            grew = live() as isize - before;
        }
        assert_eq!(
            grew,
            THREADS * EACH,
            "threads exited without handing over their drift"
        );
        publish(-(THREADS * EACH));
    }

    #[test]
    fn megabytes_read_as_megabytes() {
        assert_eq!(mb(1_500_000), "2 MB");
        assert_eq!(mb(0), "0 MB");
    }
}
