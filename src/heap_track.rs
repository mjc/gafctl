use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static COUNTERS: Counters = Counters::new();

struct CountingAllocator;

struct Counters {
    allocations: AtomicU64,
    zeroed_allocations: AtomicU64,
    reallocations: AtomicU64,
    deallocations: AtomicU64,
    allocated_bytes: AtomicU64,
    reallocated_from_bytes: AtomicU64,
    reallocated_to_bytes: AtomicU64,
    deallocated_bytes: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
        Self {
            allocations: AtomicU64::new(0),
            zeroed_allocations: AtomicU64::new(0),
            reallocations: AtomicU64::new(0),
            deallocations: AtomicU64::new(0),
            allocated_bytes: AtomicU64::new(0),
            reallocated_from_bytes: AtomicU64::new(0),
            reallocated_to_bytes: AtomicU64::new(0),
            deallocated_bytes: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            allocations: self.allocations.load(Ordering::Relaxed),
            zeroed_allocations: self.zeroed_allocations.load(Ordering::Relaxed),
            reallocations: self.reallocations.load(Ordering::Relaxed),
            deallocations: self.deallocations.load(Ordering::Relaxed),
            allocated_bytes: self.allocated_bytes.load(Ordering::Relaxed),
            reallocated_from_bytes: self.reallocated_from_bytes.load(Ordering::Relaxed),
            reallocated_to_bytes: self.reallocated_to_bytes.load(Ordering::Relaxed),
            deallocated_bytes: self.deallocated_bytes.load(Ordering::Relaxed),
        }
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: this forwards the allocator contract and layout unchanged to System.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            COUNTERS.allocations.fetch_add(1, Ordering::Relaxed);
            COUNTERS
                .allocated_bytes
                .fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: this forwards the allocator contract and layout unchanged to System.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            COUNTERS.zeroed_allocations.fetch_add(1, Ordering::Relaxed);
            COUNTERS
                .allocated_bytes
                .fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer and layout are passed through unchanged from the caller.
        unsafe { System.dealloc(pointer, layout) };
        COUNTERS.deallocations.fetch_add(1, Ordering::Relaxed);
        COUNTERS
            .deallocated_bytes
            .fetch_add(layout.size() as u64, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: pointer, old layout, and new size are passed through unchanged to System.
        let resized = unsafe { System.realloc(pointer, layout, new_size) };
        if !resized.is_null() {
            COUNTERS.reallocations.fetch_add(1, Ordering::Relaxed);
            COUNTERS
                .reallocated_from_bytes
                .fetch_add(layout.size() as u64, Ordering::Relaxed);
            COUNTERS
                .reallocated_to_bytes
                .fetch_add(new_size as u64, Ordering::Relaxed);
        }
        resized
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Snapshot {
    allocations: u64,
    zeroed_allocations: u64,
    reallocations: u64,
    deallocations: u64,
    allocated_bytes: u64,
    reallocated_from_bytes: u64,
    reallocated_to_bytes: u64,
    deallocated_bytes: u64,
}

pub(crate) fn snapshot() -> Snapshot {
    COUNTERS.snapshot()
}

pub(crate) fn report_since(start: Snapshot) {
    let end = snapshot();
    let allocations = end.allocations - start.allocations;
    let zeroed_allocations = end.zeroed_allocations - start.zeroed_allocations;
    let reallocations = end.reallocations - start.reallocations;
    let allocation_events = allocations + zeroed_allocations + reallocations;
    let requested_bytes = end.allocated_bytes - start.allocated_bytes + end.reallocated_to_bytes
        - start.reallocated_to_bytes;
    let released_bytes = end.deallocated_bytes - start.deallocated_bytes
        + end.reallocated_from_bytes
        - start.reallocated_from_bytes;
    let deallocations = end.deallocations - start.deallocations;

    eprintln!(
        "heap track: {allocation_events} allocation events ({allocations} alloc, {zeroed_allocations} zeroed, {reallocations} realloc), {deallocations} dealloc; requested {requested_bytes} bytes, released {released_bytes} bytes"
    );
}
