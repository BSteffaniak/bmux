//! Heap allocations counted, for benchmarks whose numbers must not depend on machine load.
//!
//! A benchmark binary installs [`CountingAllocator`] as its global allocator:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOCATOR: bmux_bench::allocations::CountingAllocator =
//!     bmux_bench::allocations::CountingAllocator;
//! ```
//!
//! and measures the work it cares about with [`measure`]. Counts are kept per thread, so work on
//! other threads (a test harness, a logger) never lands in a measurement; live and peak bytes are
//! process-wide. Every allocation request counts, a reallocation included: a `Vec` growing three
//! times is three allocations, which is what reusing its storage saves.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Allocation requests made and the bytes they asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Allocations {
    /// Allocation requests (`alloc`, `alloc_zeroed` and `realloc` calls).
    pub count: u64,
    /// Bytes those requests asked for.
    pub bytes: u64,
}

impl Allocations {
    /// What was allocated between `earlier` and `self` (both from [`current`]).
    #[must_use]
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            count: self.count.saturating_sub(earlier.count),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

impl std::ops::Add for Allocations {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            count: self.count + other.count,
            bytes: self.bytes + other.bytes,
        }
    }
}

impl std::ops::AddAssign for Allocations {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}

thread_local! {
    static THREAD: Cell<Allocations> = const { Cell::new(Allocations { count: 0, bytes: 0 }) };
}

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn allocated(size: usize) {
    // `try_with`: a thread's locals are gone while it exits, and its last frees still run.
    let _ = THREAD.try_with(|counts| {
        let old = counts.get();
        counts.set(Allocations {
            count: old.count + 1,
            bytes: old.bytes + size as u64,
        });
    });
}

fn grew(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn shrank(size: usize) {
    LIVE.fetch_sub(size, Ordering::Relaxed);
}

/// The system allocator, counting what is allocated (see the module docs).
pub struct CountingAllocator;

// SAFETY: every call forwards its pointer and layout unchanged to `System`, which upholds the
// `GlobalAlloc` contract; the accounting allocates nothing and touches no memory it is handed.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract for `layout`.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
            grew(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
            grew(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller passes a pointer this allocator returned, with its layout.
        unsafe { System.dealloc(pointer, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract for `pointer`, `layout`, `new_size`.
        let next = unsafe { System.realloc(pointer, layout, new_size) };
        if !next.is_null() {
            allocated(new_size);
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                shrank(layout.size() - new_size);
            }
        }
        next
    }
}

/// Everything this thread has allocated so far (zero without [`CountingAllocator`] installed).
#[must_use]
pub fn current() -> Allocations {
    THREAD.try_with(Cell::get).unwrap_or_default()
}

/// Run `work` and return what it allocated on this thread.
pub fn measure<T>(work: impl FnOnce() -> T) -> (T, Allocations) {
    let before = current();
    let value = work();
    (value, current().since(before))
}

/// Whether [`CountingAllocator`] is the global allocator, so counts mean something.
#[must_use]
pub fn installed() -> bool {
    let ((), allocations) = measure(|| drop(std::hint::black_box(Box::new(0_u64))));
    allocations.count > 0
}

/// Bytes allocated and not yet freed, process-wide.
#[must_use]
pub fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// Most bytes live at once since the last [`reset_peak`], process-wide.
#[must_use]
pub fn peak_bytes() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Start tracking the peak again from what is live now.
pub fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}
