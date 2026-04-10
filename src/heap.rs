use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::node_heap::PerNodeHeap;
use crate::platform;

/// Maximum number of NUMA nodes supported.
pub const MAX_NODES: usize = 8;

/// Default virtual-address reservation per node (128 MiB).
/// Only virtual address space is consumed upfront; physical pages are
/// demand-faulted by the kernel.
pub const DEFAULT_REGION_SIZE: usize = 128 * 1024 * 1024;

// ---------------------------------------------------------------------------
// NodeRegion — per-node slice of the contiguous heap
// ---------------------------------------------------------------------------

/// A contiguous virtual-memory region bound to one NUMA node.
pub struct NodeRegion {
    /// Base address of this region.  `NonNull::dangling()` for unused slots.
    base: NonNull<u8>,
    size: usize,
    /// Atomic bump pointer (byte offset from `base`).  Incremented by
    /// `BAG_SIZE` for each new bag — lock-free allocation.
    bump: AtomicUsize,
    pub node_heap: PerNodeHeap,
}

impl NodeRegion {
    fn new(base: NonNull<u8>, size: usize) -> Self {
        Self {
            base,
            size,
            bump: AtomicUsize::new(0),
            node_heap: PerNodeHeap::new(),
        }
    }

    fn empty() -> Self {
        Self::new(NonNull::dangling(), 0)
    }

    /// Bump-allocate a bag of the given `bag_size`.  The returned pointer is
    /// aligned to `bag_size` (which must be a power of two) so that objects
    /// carved from it inherit the alignment.  Returns `None` on exhaustion.
    /// This is lock-free (atomic CAS retry loop).
    pub fn allocate_bag(&self, bag_size: usize) -> Option<NonNull<u8>> {
        debug_assert!(bag_size.is_power_of_two());
        let align_mask = bag_size - 1;
        loop {
            let offset = self.bump.load(Ordering::Relaxed);
            // Align up to bag_size boundary.
            let aligned = (offset + align_mask) & !align_mask;
            let new_offset = aligned + bag_size;
            if new_offset > self.size {
                return None;
            }
            if self
                .bump
                .compare_exchange_weak(offset, new_offset, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                // SAFETY: `aligned` is within `[0, self.size)` and `base` is
                // valid for `self.size` bytes.
                return Some(unsafe { NonNull::new_unchecked(self.base.as_ptr().add(aligned)) });
            }
        }
    }
}

// Safety: `base` is only accessed through atomic bump pointers and the
// lock-free per-node freelist.  All concurrent paths use proper synchronisation.
unsafe impl Send for NodeRegion {}
unsafe impl Sync for NodeRegion {}

// ---------------------------------------------------------------------------
// GlobalHeap — the top-level heap owning the mmap'd region
// ---------------------------------------------------------------------------

/// Global heap: one contiguous `mmap` region split into per-node sub-regions.
pub struct GlobalHeap {
    base: NonNull<u8>,
    total_size: usize,
    /// log2(region_size) — used for O(1) node lookup via bit-shift instead of
    /// expensive integer division.
    region_shift: u32,
    num_nodes: usize,
    nodes: [NodeRegion; MAX_NODES],
}

impl GlobalHeap {
    /// Allocate and initialise the global heap.
    pub fn new(num_nodes: usize) -> Option<Self> {
        let num_nodes = num_nodes.clamp(1, MAX_NODES);
        let region_size = DEFAULT_REGION_SIZE;
        debug_assert!(region_size.is_power_of_two());
        let region_shift = region_size.trailing_zeros();
        let total_size = region_size * num_nodes;

        let base = unsafe { platform::mmap_anonymous(total_size)? };

        let nodes: [NodeRegion; MAX_NODES] = std::array::from_fn(|i| {
            if i < num_nodes {
                let node_base =
                    unsafe { NonNull::new_unchecked(base.as_ptr().add(i * region_size)) };
                unsafe {
                    platform::bind_to_node(node_base, region_size, i);
                }
                NodeRegion::new(node_base, region_size)
            } else {
                NodeRegion::empty()
            }
        });

        Some(Self {
            base,
            total_size,
            region_shift,
            num_nodes,
            nodes,
        })
    }

    /// Determine which NUMA node owns `ptr`.  Returns `None` if the pointer
    /// falls outside the heap.
    #[inline]
    pub fn node_for_ptr(&self, ptr: NonNull<u8>) -> Option<usize> {
        let offset = (ptr.as_ptr() as usize).wrapping_sub(self.base.as_ptr() as usize);
        if offset >= self.total_size {
            return None;
        }
        Some(offset >> self.region_shift)
    }

    #[inline]
    pub fn node_region(&self, node: usize) -> &NodeRegion {
        &self.nodes[node]
    }

    #[inline]
    pub fn num_nodes(&self) -> usize {
        self.num_nodes
    }
}

// Safety: see NodeRegion reasoning.  The `base` pointer is immutable after
// construction; all mutation goes through per-node atomics.
unsafe impl Send for GlobalHeap {}
unsafe impl Sync for GlobalHeap {}

impl Drop for GlobalHeap {
    fn drop(&mut self) {
        unsafe {
            platform::munmap(self.base, self.total_size);
        }
    }
}
