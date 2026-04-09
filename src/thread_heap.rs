use crate::freelist::ThreadFreelist;
use crate::heap::GlobalHeap;
use crate::size_class::NUM_SIZE_CLASSES;

/// Maximum objects cached per size class in a per-thread freelist before
/// draining excess to the per-node heap.
pub const MAX_THREAD_CACHE: usize = 64;

/// Number of objects to attempt to refill from the per-node heap when the
/// per-thread freelist is empty.
pub const REFILL_BATCH: usize = 32;

/// Per-thread heap: one [`ThreadFreelist`] per size class, plus the owning
/// node id.  Accessed exclusively by a single thread (no synchronisation).
pub struct PerThreadHeap {
    pub node_id: usize,
    /// Pointer to the owning [`GlobalHeap`], used during thread-exit cleanup
    /// to drain cached blocks back to per-node freelists.
    pub global_heap: *const GlobalHeap,
    freelists: [ThreadFreelist; NUM_SIZE_CLASSES],
}

impl PerThreadHeap {
    pub fn new(node_id: usize, global_heap: *const GlobalHeap) -> Self {
        Self {
            node_id,
            global_heap,
            freelists: std::array::from_fn(|_| ThreadFreelist::new()),
        }
    }

    #[inline]
    pub fn freelist_mut(&mut self, class_index: usize) -> &mut ThreadFreelist {
        &mut self.freelists[class_index]
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
    }
}
