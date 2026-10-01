// ============================================================================
// Counting global allocator — test instrumentation only.
//
// Exists so leak regressions can be asserted on LIVE ALLOCATED BYTES rather
// than process RSS. RSS is process-wide, so with `cargo test`'s parallel
// harness a neighbouring test's working set shows up as this test's "leak":
// the first version of the spec_for leak test reported 15 MB of growth that
// was actually the extractor suite loading an 890 KB fixture.
// ============================================================================

#![cfg(test)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicUsize = AtomicUsize::new(0);

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                ALLOCATED.fetch_add(new_size - layout.size(), Ordering::Relaxed);
            } else {
                FREED.fetch_add(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

/// Bytes allocated and not yet freed, process-wide.
///
/// Still shared across parallel tests, but unlike RSS it goes back DOWN when a
/// neighbour frees, so a leak shows up as monotonic growth attributable to the
/// code under test.
pub fn live_bytes() -> usize {
    ALLOCATED
        .load(Ordering::Relaxed)
        .saturating_sub(FREED.load(Ordering::Relaxed))
}
