use std::alloc::{Layout, System};
use std::ptr::NonNull;

use crate::freelist::ThreadFreelist;
use crate::heap::GlobalHeap;
use crate::platform;
use crate::size_class::NUM_SIZE_CLASSES;

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
/// struct (~320 bytes) while the large cache can be up to 16 KiB.
struct LargeCache {
    entries: [LargeCacheEntry; LARGE_CACHE_SLOTS],
    count: usize,
    bytes: usize,
}

impl LargeCache {
    fn new_boxed() -> NonNull<Self> {
        let layout = Layout::new::<Self>();
        // SAFETY: layout is non-zero size.
        let ptr = unsafe { std::alloc::GlobalAlloc::alloc(&System, layout) } as *mut Self;
        let Some(nn) = NonNull::new(ptr) else {
            std::alloc::handle_alloc_error(layout);
        };
        unsafe {
            std::ptr::write(
                &raw mut (*nn.as_ptr()).count,
                0,
            );
            std::ptr::write(
                &raw mut (*nn.as_ptr()).bytes,
                0,
            );
        }
        nn
    }

    #[inline]
    fn take(&mut self, alloc_size: usize) -> Option<(NonNull<u8>, usize)> {
        let count = self.count;
        for i in 0..count {
            if self.entries[i].alloc_size == alloc_size {
                let entry = self.entries[i];
                self.count -= 1;
                self.bytes -= entry.alloc_size;
                self.entries[i] = self.entries[self.count];
                return Some((entry.original_ptr, entry.alloc_size));
            }
        }
        None
    }

    #[inline]
    fn put(&mut self, original_ptr: NonNull<u8>, alloc_size: usize) -> bool {
        // Evict stale entries when the cache is full or byte limit reached.
        while self.count > 0
            && (self.count >= LARGE_CACHE_SLOTS
                || self.bytes + alloc_size > MAX_LARGE_CACHE_BYTES)
        {
            self.count -= 1;
            let evicted = self.entries[self.count];
            self.bytes -= evicted.alloc_size;
            // SAFETY: evicted entry was stored from a valid mmap region.
            unsafe {
                platform::munmap(evicted.original_ptr, evicted.alloc_size);
            }
        }
        if self.bytes + alloc_size > MAX_LARGE_CACHE_BYTES {
            return false;
        }
        let idx = self.count;
        self.entries[idx] = LargeCacheEntry {
            original_ptr,
            alloc_size,
        };
        self.count += 1;
        self.bytes += alloc_size;
        true
    }

    fn flush(&mut self) {
        for i in 0..self.count {
            let entry = self.entries[i];
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

// ---------------------------------------------------------------------------
// PerThreadHeap
// ---------------------------------------------------------------------------

/// Maximum objects cached per size class in a per-thread freelist before
/// draining excess to the per-node heap.
pub const MAX_THREAD_CACHE: usize = 256;

/// Number of objects to attempt to refill from the per-node heap when the
/// per-thread freelist is empty.
pub const REFILL_BATCH: usize = 64;

/// Per-thread heap: one [`ThreadFreelist`] per size class, plus the owning
/// node id.  Accessed exclusively by a single thread (no synchronisation).
///
/// The large object cache is heap-allocated separately to keep this struct
/// compact and cache-friendly for the hot small-object path.
pub struct PerThreadHeap {
    pub node_id: usize,
    /// Pointer to the owning [`GlobalHeap`], used during thread-exit cleanup
    /// to drain cached blocks back to per-node freelists.
    pub global_heap: *const GlobalHeap,
    freelists: [ThreadFreelist; NUM_SIZE_CLASSES],
    /// Heap-allocated large object cache (via System allocator).
    large_cache: NonNull<LargeCache>,
}

impl PerThreadHeap {
    pub fn new(node_id: usize, global_heap: *const GlobalHeap) -> Self {
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
        // SAFETY: large_cache was allocated in `new` and is exclusively owned.
        unsafe { self.large_cache.as_mut().take(alloc_size) }
    }

    /// Cache a freed large mapping for later reuse.  Returns `false` if the
    /// cache is full or the total byte limit would be exceeded (caller should
    /// `munmap` instead).
    #[inline]
    pub fn large_cache_put(
        &mut self,
        original_ptr: NonNull<u8>,
        alloc_size: usize,
    ) -> bool {
        // SAFETY: large_cache was allocated in `new` and is exclusively owned.
        unsafe { self.large_cache.as_mut().put(original_ptr, alloc_size) }
    }

    /// Drain all per-size-class freelists back to the per-node Treiber stacks.
    ///
    /// # Safety
    /// `self.global_heap` must point to a valid, live [`GlobalHeap`].
    pub unsafe fn drain_to_node_heap(&mut self) {
        // SAFETY: caller guarantees GlobalHeap is still alive.
        let heap = unsafe { &*self.global_heap };
        let node = self.node_id;
        for class_idx in 0..NUM_SIZE_CLASSES {
            if let Some((head, tail, _)) = self.freelists[class_idx].drain_all() {
                heap.node_region(node)
                    .node_heap
                    .freelist(class_idx)
                    .push_chain(head, tail);
            }
        }
        // Also release cached large mappings and free the cache struct.
        unsafe {
            self.large_cache.as_mut().flush();
            let layout = Layout::new::<LargeCache>();
            std::alloc::GlobalAlloc::dealloc(&System, self.large_cache.as_ptr() as *mut u8, layout);
        }
    }
}
