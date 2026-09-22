//! Test-only, thread-local allocation measurement for synchronous production paths.
//!
//! Counts successful allocation requests, including the full new size of a
//! reallocation. This is allocation traffic, not live bytes, copied bytes, or RSS.
//! Other test threads are excluded; work moved to another thread is not measured.

#![cfg(test)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AllocationStats {
    pub(crate) requested_bytes: usize,
    pub(crate) allocation_calls: usize,
}

thread_local! {
    // Const initialization and Copy/Cell access avoid allocation and reentrancy
    // inside the allocator. try_with also tolerates thread-local destruction.
    static ACTIVE_MEASUREMENT: Cell<Option<AllocationStats>> = const { Cell::new(None) };
}

struct MeasuredSystem;

#[global_allocator]
static ALLOCATOR: MeasuredSystem = MeasuredSystem;

fn record_request(bytes: usize) {
    let _ = ACTIVE_MEASUREMENT.try_with(|active| {
        if let Some(mut stats) = active.get() {
            stats.requested_bytes = stats.requested_bytes.saturating_add(bytes);
            stats.allocation_calls = stats.allocation_calls.saturating_add(1);
            active.set(Some(stats));
        }
    });
}

// SAFETY: every allocation operation delegates to System with the caller's
// original pointer/layout. Accounting neither touches allocation contents nor
// allocates, panics, or changes the result returned by System.
unsafe impl GlobalAlloc for MeasuredSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_request(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_request(layout.size());
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !pointer.is_null() {
            record_request(new_size);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}

struct MeasurementScope;

impl Drop for MeasurementScope {
    fn drop(&mut self) {
        let _ = ACTIVE_MEASUREMENT.try_with(|active| active.set(None));
    }
}

/// Measure only this thread until the synchronous closure returns or unwinds.
/// Construct inputs and inspect/serialize outputs outside the closure. Nested
/// measurements are rejected rather than silently corrupting the outer result.
pub(crate) fn measure_allocations<T>(operation: impl FnOnce() -> T) -> (T, AllocationStats) {
    assert!(
        ACTIVE_MEASUREMENT.with(|active| active.get().is_none()),
        "allocation measurements cannot be nested"
    );
    ACTIVE_MEASUREMENT.with(|active| active.set(Some(AllocationStats::default())));
    let scope = MeasurementScope;
    let result = operation();
    let stats = ACTIVE_MEASUREMENT
        .with(|active| active.replace(None))
        .expect("the measurement scope is still active");
    drop(scope);
    (result, stats)
}

#[test]
fn memory_efficiency_allocation_meter_counts_requests_and_resets_between_scopes() {
    let outside = std::hint::black_box(vec![42_u8; 16 * 1024]);
    let ((grown, zeroed), stats) = measure_allocations(|| {
        let mut grown = Vec::with_capacity(1024);
        grown.resize(1024, 42_u8);
        std::hint::black_box(&grown);
        grown.reserve_exact(1024);
        let zeroed = std::hint::black_box(vec![0_u8; 4096]);
        (std::hint::black_box(grown), zeroed)
    });
    assert!(stats.requested_bytes >= 1024 + 2048 + 4096, "{stats:?}");
    assert!(stats.allocation_calls >= 3, "{stats:?}");
    assert_eq!(grown.as_slice(), &[42_u8; 1024]);
    assert_eq!(zeroed.as_slice(), &[0_u8; 4096]);
    std::hint::black_box(outside);

    let (_, empty) = measure_allocations(|| std::hint::black_box(7));
    assert_eq!(empty, AllocationStats::default());
}

#[test]
fn memory_efficiency_allocation_meter_resets_after_unwinding() {
    // Exercise a panic in measured work without relying on a panic as a TDD
    // outcome: this calibration succeeds only if the next scope is clean.
    let result = std::panic::catch_unwind(|| {
        measure_allocations(|| {
            std::panic::resume_unwind(Box::new("allocation meter calibration"));
        });
    });
    assert!(result.is_err());
    let (_, next) = measure_allocations(|| std::hint::black_box(11));
    assert_eq!(next, AllocationStats::default());
}
