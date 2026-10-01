// ============================================================================
// SearchWala v8.0 — Counting global allocator (test instrumentation only)
//
// WHY THREAD-LOCAL
// Leak regressions need a measurement that `cargo test`'s parallel harness
// cannot disturb. Two earlier attempts both failed intermittently:
//
//   1. /proc/self/status VmRSS — process-wide, so the extractor suite loading
//      an 890 KB fixture appeared as a 15 MB "leak" in the generic-engine test.
//   2. A process-wide live-bytes counter — better, because it goes back down
//      when a neighbour frees, but still shared: a neighbour allocating inside
//      the measurement window inflated the delta.
//
// Thread-local counters fix this properly. The code under test (`spec_for`) is
// synchronous and runs entirely on the measuring thread, so its allocations
// are exactly what gets counted, no matter what the other test threads do.
// ============================================================================

#![cfg(test)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
    static FREED: Cell<usize> = const { Cell::new(0) };
}

pub struct Counting;

/// Adds to a thread-local counter, tolerating the case where TLS is being
/// destroyed (which can happen for allocations during thread teardown).
fn bump(key: &'static std::thread::LocalKey<Cell<usize>>, n: usize) {
    let _ = key.try_with(|c| c.set(c.get().wrapping_add(n)));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            bump(&ALLOCATED, layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        bump(&FREED, layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                bump(&ALLOCATED, new_size - layout.size());
            } else {
                bump(&FREED, layout.size() - new_size);
            }
        }
        p
    }
}

/// Bytes allocated and not yet freed **on the calling thread**.
///
/// A leak shows up as growth across a loop; transient allocations net to zero
/// because the same thread frees them.
pub fn live_bytes() -> usize {
    let a = ALLOCATED.try_with(|c| c.get()).unwrap_or(0);
    let f = FREED.try_with(|c| c.get()).unwrap_or(0);
    a.saturating_sub(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_allocations_net_to_zero() {
        let before = live_bytes();
        for _ in 0..10_000 {
            let s = String::from("a transient heap allocation that is dropped");
            std::hint::black_box(&s);
        }
        let growth = live_bytes().saturating_sub(before);
        assert!(growth < 64 * 1024, "transient allocs leaked {growth} bytes");
    }

    #[test]
    fn retained_allocations_are_visible() {
        let before = live_bytes();
        let mut kept: Vec<String> = Vec::new();
        for i in 0..2_000 {
            kept.push(format!("retained string number {i} with some padding"));
        }
        let growth = live_bytes().saturating_sub(before);
        assert!(growth > 50_000, "retained allocs should be visible, saw {growth}");
        std::hint::black_box(&kept);
    }

    #[test]
    fn counters_are_independent_per_thread() {
        // The property that makes this usable under the parallel harness.
        let before = live_bytes();
        std::thread::spawn(|| {
            let mut v: Vec<String> = Vec::new();
            for i in 0..5_000 {
                v.push(format!("other thread allocation {i} padded out a bit"));
            }
            std::hint::black_box(&v);
        })
        .join()
        .unwrap();
        let growth = live_bytes().saturating_sub(before);
        assert!(growth < 64 * 1024, "another thread's allocations leaked in: {growth}");
    }
}
