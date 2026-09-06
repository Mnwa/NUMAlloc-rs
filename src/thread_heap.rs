use std::mem::MaybeUninit;
use std::ptr::NonNull;

use crate::freelist::ThreadFreelist;
use crate::heap::HeapHandle;
use crate::platform;
use crate::size_class::{self, NUM_SIZE_CLASSES};
use crate::sys_box::SysBox;

// ---------------------------------------------------------------------------
// Large object cache — per-thread, avoids mmap/munmap on hot paths
// ---------------------------------------------------------------------------

/// Maximum number of cached large-object mappings per thread.
const LARGE_CACHE_SLOTS: usize = 1024;

/// Maximum total bytes held in the large cache before we start evicting.
const MAX_LARGE_CACHE_BYTES: usize = 512 * 1024 * 1024; // 512 MiB

/// Only call madvise for cached regions larger than this threshold.
/// Below this, the syscall overhead exceeds the memory savings.
pub const MADVISE_THRESHOLD: usize = 512 * 1024; // 512 KiB

/// A cached large-object mmap region ready for reuse.
#[derive(Clone, Copy)]
struct LargeCacheEntry {
    /// Original pointer returned by `mmap`.
    original_ptr: NonNull<u8>,
    /// Total size of the mmap region.
    alloc_size: usize,
}

/// Heap-allocated large object cache.  Kept separate from [`PerThreadHeap`]
/// so that the hot small-object freelists stay in a compact, cache-friendly
/// struct while the large cache can be up to 16 KiB.
///
/// Owned via [`SysBox`] — the [`Drop`] impl flushes all cached regions.
struct LargeCache {
    entries: [MaybeUninit<LargeCacheEntry>; LARGE_CACHE_SLOTS],
    count: usize,
    bytes: usize,
}

impl LargeCache {
    /// Allocate a new, empty large cache via the system allocator.
    ///
    /// Uses `alloc_zeroed` so the 16 KiB `entries` array is never
    /// constructed on the stack.  All-zeros is valid: `count = 0`,
    /// `bytes = 0`, and `MaybeUninit` accepts any bit pattern.
    fn new_boxed() -> SysBox<Self> {
        // SAFETY: all-zeros is a valid `LargeCache`: count and bytes are 0,
        // entries are MaybeUninit (any bit pattern is valid).
        unsafe { SysBox::new_zeroed() }
    }

    /// Maximum extra bytes we tolerate when reusing a cached region.
    const CLOSE_SIZE_TOLERANCE: usize = 8 * 1024; // 8 KiB

    #[inline]
    fn take(&mut self, alloc_size: usize) -> Option<(NonNull<u8>, usize)> {
        let count = self.count;
        let mut best_idx: usize = usize::MAX;
        let mut best_waste: usize = usize::MAX;
        for i in 0..count {
            // SAFETY: entries[0..count] are always initialised.
            let entry = unsafe { self.entries[i].assume_init_read() };
            if entry.alloc_size == alloc_size {
                // Exact match — take immediately.
                self.count -= 1;
                self.bytes -= entry.alloc_size;
                self.entries[i] = self.entries[self.count];
                return Some((entry.original_ptr, entry.alloc_size));
            }
            let waste = entry.alloc_size.wrapping_sub(alloc_size);
            if entry.alloc_size >= alloc_size
                && waste <= Self::CLOSE_SIZE_TOLERANCE
                && waste < best_waste
            {
                best_waste = waste;
                best_idx = i;
            }
        }
        if best_idx < count {
            // SAFETY: entries[0..count] are always initialised.
            let entry = unsafe { self.entries[best_idx].assume_init_read() };
            self.count -= 1;
            self.bytes -= entry.alloc_size;
            self.entries[best_idx] = self.entries[self.count];
            return Some((entry.original_ptr, entry.alloc_size));
        }
        None
    }

    #[inline]
    fn put(&mut self, original_ptr: NonNull<u8>, alloc_size: usize) -> bool {
        // Evict stale entries when the cache is full or byte limit reached.
        while self.count > 0
            && (self.count >= LARGE_CACHE_SLOTS
                || self.bytes.saturating_add(alloc_size) > MAX_LARGE_CACHE_BYTES)
        {
            self.count -= 1;
            // SAFETY: entries[0..count] are always initialised.
            let evicted = unsafe { self.entries[self.count].assume_init_read() };
            self.bytes -= evicted.alloc_size;
            // SAFETY: evicted entry was stored from a valid mmap region.
            unsafe {
                platform::munmap(evicted.original_ptr, evicted.alloc_size);
            }
        }
        if self.bytes.saturating_add(alloc_size) > MAX_LARGE_CACHE_BYTES {
            return false;
        }
        let idx = self.count;
        self.entries[idx] = MaybeUninit::new(LargeCacheEntry {
            original_ptr,
            alloc_size,
        });
        self.count += 1;
        self.bytes += alloc_size;
        true
    }

    /// Check slot/byte accounting and entry sanity.  See [`crate::validate`].
    #[cfg(any(test, feature = "internal-testing"))]
    fn validate(&self, heap: &crate::heap::GlobalHeap) {
        assert!(
            self.count <= LARGE_CACHE_SLOTS,
            "large cache count overflow"
        );
        assert!(
            self.bytes <= MAX_LARGE_CACHE_BYTES,
            "large cache byte overflow"
        );
        let page = platform::page_size();
        let mut total = 0usize;
        for i in 0..self.count {
            // SAFETY: entries[0..count] are always initialised.
            let e = unsafe { self.entries[i].assume_init_read() };
            let addr = e.original_ptr.as_ptr() as usize;
            assert_eq!(addr % page, 0, "large cache entry {i} not page aligned");
            assert_eq!(
                e.alloc_size % page,
                0,
                "large cache entry {i} size not page multiple"
            );
            assert!(e.alloc_size > 0, "large cache entry {i} has zero size");
            assert!(
                !heap.is_owned(e.original_ptr),
                "large cache entry {i} points into the region"
            );
            for j in 0..i {
                // SAFETY: entries[0..count] are always initialised.
                let other = unsafe { self.entries[j].assume_init_read() };
                let o = other.original_ptr.as_ptr() as usize;
                assert!(
                    addr + e.alloc_size <= o || o + other.alloc_size <= addr,
                    "large cache entries {i} and {j} overlap"
                );
            }
            total += e.alloc_size;
        }
        assert_eq!(total, self.bytes, "large cache byte accounting drifted");
    }

    fn flush(&mut self) {
        for i in 0..self.count {
            // SAFETY: entries[0..count] are always initialised.
            let entry = unsafe { self.entries[i].assume_init_read() };
            // SAFETY: `original_ptr` and `alloc_size` were saved from a
            // valid `mmap` region in `put`.
            unsafe {
                platform::munmap(entry.original_ptr, entry.alloc_size);
            }
        }
        self.count = 0;
        self.bytes = 0;
    }
}

impl Drop for LargeCache {
    fn drop(&mut self) {
        self.flush();
    }
}

// ---------------------------------------------------------------------------
// PerThreadHeap
// ---------------------------------------------------------------------------

/// Number of blocks moved from a detached node chain into the counted
/// per-thread freelist per refill; the rest is parked as the spare chain.
/// Bounds refill cost to O(REFILL_BATCH) regardless of node stack length.
pub const REFILL_BATCH: usize = 64;

/// Maximum objects cached per size class in a per-thread freelist before
/// draining excess to the per-node heap.
///
/// Scales with object size: small objects (8–64 B) cache up to 2048 items,
/// while large objects (≥ 16 KB) cache only 64.  This keeps memory overhead
/// bounded while avoiding excessive drain/refill for bulk allocation patterns.
const MAX_CACHE_TABLE: [usize; NUM_SIZE_CLASSES] = build_max_cache_table();

const fn build_max_cache_table() -> [usize; NUM_SIZE_CLASSES] {
    let mut table = [0usize; NUM_SIZE_CLASSES];
    let mut i = 0;
    while i < NUM_SIZE_CLASSES {
        let obj = size_class::SIZE_CLASSES[i];
        let bag = size_class::bag_size_for_class(i);
        let per_bag = bag / obj;
        // Two bags' worth, clamped to [64, 2048].
        let raw = per_bag * 2;
        table[i] = if raw < 64 {
            64
        } else if raw > 2048 {
            2048
        } else {
            raw
        };
        i += 1;
    }
    table
}

/// Returns the maximum per-thread cache size for a given size class.
#[inline]
pub fn max_thread_cache(class_idx: usize) -> usize {
    MAX_CACHE_TABLE[class_idx]
}

/// Per-thread heap: one [`ThreadFreelist`] per size class, plus the owning
/// node id.  Accessed exclusively by a single thread (no synchronisation).
///
/// The large object cache is heap-allocated separately via [`SysBox`] to keep
/// this struct compact and cache-friendly for the hot small-object path.
///
/// [`Drop`] automatically drains all cached blocks back to the per-node
/// Treiber stacks, and the owned [`SysBox<LargeCache>`] flushes cached
/// mmap regions.
pub struct PerThreadHeap {
    pub node_id: usize,
    /// Handle to the owning [`GlobalHeap`], used during drop to drain
    /// cached blocks back to per-node freelists.  The handle keeps the heap
    /// alive for as long as this thread heap exists.
    pub global_heap: HeapHandle,
    freelists: [ThreadFreelist; NUM_SIZE_CLASSES],
    /// Owned large object cache (via System allocator).
    large_cache: SysBox<LargeCache>,
}

impl PerThreadHeap {
    pub fn new(node_id: usize, global_heap: HeapHandle) -> Self {
        Self {
            node_id,
            global_heap,
            freelists: std::array::from_fn(|_| ThreadFreelist::new()),
            large_cache: LargeCache::new_boxed(),
        }
    }

    #[inline]
    pub fn freelist_mut(&mut self, class_index: usize) -> &mut ThreadFreelist {
        &mut self.freelists[class_index]
    }

    /// Try to find a cached large mapping with the exact `alloc_size`.
    /// Returns `(original_ptr, alloc_size)` on hit.
    #[inline]
    pub fn large_cache_take(&mut self, alloc_size: usize) -> Option<(NonNull<u8>, usize)> {
        self.large_cache.take(alloc_size)
    }

    /// Cache a freed large mapping for later reuse.  Returns `false` if the
    /// cache is full or the total byte limit would be exceeded (caller should
    /// `munmap` instead).
    #[inline]
    pub fn large_cache_put(&mut self, original_ptr: NonNull<u8>, alloc_size: usize) -> bool {
        self.large_cache.put(original_ptr, alloc_size)
    }
}

impl Drop for PerThreadHeap {
    fn drop(&mut self) {
        // `global_heap` is a refcounted handle, so the heap (and its region)
        // outlive this destructor even if the owning `NumaAlloc` has already
        // moved on to another heap.
        let heap = &self.global_heap;
        let node = self.node_id;
        for class_idx in 0..NUM_SIZE_CLASSES {
            let node_fl = heap.node_region(node).node_heap.freelist(class_idx);
            if let Some((head, tail, _)) = self.freelists[class_idx].drain_all() {
                node_fl.push_chain(head, tail);
            }
            // The spare chain's tail is unknown: walk it (cold path, thread
            // exit) so the whole chain goes back in one CAS.
            if let Some(head) = self.freelists[class_idx].take_spare() {
                let mut tail = head;
                // SAFETY: the spare chain is exclusively owned by this thread.
                while let Some(next) = unsafe { tail.as_ref().read_next() } {
                    tail = next;
                }
                node_fl.push_chain(head, tail);
            }
        }
        // `large_cache: SysBox<LargeCache>` is dropped automatically after
        // this method returns — `LargeCache::Drop` flushes cached mmap
        // regions, then `SysBox::Drop` frees the allocation.
    }
}

impl PerThreadHeap {
    /// Walk this thread's freelists and large cache, checking the invariants
    /// described in [`crate::validate`].  Must be called by the owning thread.
    #[cfg(any(test, feature = "internal-testing"))]
    pub fn validate(&self, marks: &mut crate::validate::Marks) {
        let heap = &*self.global_heap;
        assert!(
            self.node_id < heap.num_nodes(),
            "thread bound to unknown node"
        );
        for class in 0..NUM_SIZE_CLASSES {
            let fl = &self.freelists[class];
            let mut cursor = fl.head();
            let mut len = 0usize;
            let mut last = None;
            while let Some(block) = cursor {
                assert!(
                    len <= fl.count(),
                    "thread freelist class {class}: chain longer than count"
                );
                assert_eq!(
                    heap.node_for_ptr(block.cast()),
                    Some(self.node_id),
                    "thread freelist class {class}: block from another node"
                );
                marks.mark_block(heap, self.node_id, class, block.cast(), "thread cache");
                last = Some(block);
                // SAFETY: single-owner list; no concurrent mutation.
                cursor = unsafe { block.as_ref().read_next() };
                len += 1;
            }
            assert_eq!(
                len,
                fl.count(),
                "thread freelist class {class}: count mismatch"
            );
            assert_eq!(
                last,
                fl.tail(),
                "thread freelist class {class}: tail mismatch"
            );
            let max_len = heap.region_size() / size_class::size_for_class(class) + 1;
            let mut cursor = fl.spare();
            let mut len = 0usize;
            while let Some(block) = cursor {
                assert!(len < max_len, "thread spare chain class {class}: cycle");
                assert_eq!(
                    heap.node_for_ptr(block.cast()),
                    Some(self.node_id),
                    "thread spare chain class {class}: block from another node"
                );
                marks.mark_block(heap, self.node_id, class, block.cast(), "thread spare");
                // SAFETY: single-owner chain; no concurrent mutation.
                cursor = unsafe { block.as_ref().read_next() };
                len += 1;
            }
        }
        self.large_cache.validate(heap);
    }
}
