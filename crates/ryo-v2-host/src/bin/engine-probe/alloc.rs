//! The counting allocator of `engine-probe`: the system allocator behind two relaxed atomic
//! counters, the live bytes and the most bytes live at once.
//!
//! The one module of the repository that may use `unsafe`, and the one row of `UNSAFE.tsv`
//! (`crates/ryo-v2-core/tests/unsafe_ceiling.rs` keeps the two equal). It lives in a binary of its
//! own: no library crate links it, so every other binary runs on the system allocator.
#![allow(unsafe_code)]

use ryo_v2_host::engine_probe::HeapGauge;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// The system allocator, counting the bytes of every allocation it hands out and takes back.
pub struct Counting {
    live: AtomicU64,
    peak: AtomicU64,
}

impl Counting {
    /// A counter at zero.
    pub const fn new() -> Counting {
        Counting {
            live: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }

    fn add(&self, bytes: usize) {
        let bytes = bytes as u64;
        let now = self.live.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    fn sub(&self, bytes: usize) {
        self.live.fetch_sub(bytes as u64, Ordering::Relaxed);
    }
}

impl HeapGauge for Counting {
    fn live_bytes(&self) -> u64 {
        self.live.load(Ordering::Relaxed)
    }

    fn peak_bytes(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    fn reset_peak(&self) {
        // An allocation racing the store can raise the peak after it; the peak stays at least
        // the live bytes of the moment it was reset.
        self.peak
            .store(self.live.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

// SAFETY: every method forwards its call to `System` unchanged, so `Counting` meets the
// `GlobalAlloc` contract exactly as `System` does; the counters never touch the memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the layout is passed to `System` as received, under the caller's contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            self.add(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the layout is passed to `System` as received, under the caller's contract.
        // Forwarded rather than left to the default (`alloc`, then zeroing the block here), so
        // that a large zeroed block stays untouched fresh pages, as it is on `System`, and the
        // RSS the probe reads is the one the system allocator gives.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            self.add(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout are passed to `System` as received: the caller's
        // contract makes them a block that `alloc` or `realloc` above took from `System`.
        unsafe { System.dealloc(ptr, layout) };
        self.sub(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the pointer, layout and new size are passed to `System` as received, under the
        // caller's contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            self.sub(layout.size());
            self.add(new_size);
        }
        new
    }
}

#[cfg(test)]
mod tests {
    use crate::COUNTING;
    use ryo_v2_host::engine_probe::HeapGauge;

    #[test]
    fn counting_allocator_follows_allocations() {
        const SIZE: u64 = 64 << 20;
        // Other threads of the test harness allocate and free too, far less than this.
        const SLACK: u64 = 1 << 20;
        let before = COUNTING.live_bytes();
        let block: Vec<u8> = Vec::with_capacity(SIZE as usize);
        let during = COUNTING.live_bytes();
        assert!(during + SLACK >= before + SIZE, "{before} -> {during}");
        assert!(COUNTING.peak_bytes() >= during);
        // A realloc moves the count to the new size.
        let mut grown = block;
        grown.reserve_exact(2 * SIZE as usize);
        let grown_to = COUNTING.live_bytes();
        assert!(
            grown_to + SLACK >= before + 2 * SIZE,
            "{before} -> {grown_to}"
        );
        assert!(
            grown_to < before + 2 * SIZE + SLACK,
            "{before} -> {grown_to}"
        );
        // The free gives the bytes back.
        drop(grown);
        let after = COUNTING.live_bytes();
        assert!(after < before + SLACK, "{before} -> {during} -> {after}");
        assert!(COUNTING.peak_bytes() + SLACK >= before + 2 * SIZE);
        // A zeroed allocation (`vec![0; n]` takes `alloc_zeroed`) is counted and given back too.
        let before = COUNTING.live_bytes();
        let zeroed = vec![0u8; SIZE as usize];
        let during = COUNTING.live_bytes();
        assert!(during + SLACK >= before + SIZE, "{before} -> {during}");
        assert_eq!((zeroed[0], zeroed[SIZE as usize - 1]), (0, 0));
        drop(zeroed);
        let after = COUNTING.live_bytes();
        assert!(after < before + SLACK, "{before} -> {during} -> {after}");
        // A reset starts the peak again from the live bytes: a block allocated and freed after
        // it is its peak, not the earlier larger ones.
        COUNTING.reset_peak();
        let reset_at = COUNTING.live_bytes();
        assert!(COUNTING.peak_bytes() < reset_at + SLACK);
        let small = vec![1u8; (SIZE / 4) as usize];
        drop(small);
        let peak = COUNTING.peak_bytes();
        assert!(peak + SLACK >= reset_at + SIZE / 4, "{reset_at} -> {peak}");
        assert!(peak < reset_at + SIZE / 4 + SLACK, "{reset_at} -> {peak}");
    }
}
