use crate::freelist::ThreadFreelist;
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
    freelists: [ThreadFreelist; NUM_SIZE_CLASSES],
}

impl PerThreadHeap {
    pub fn new(node_id: usize) -> Self {
        Self {
            node_id,
            freelists: std::array::from_fn(|_| ThreadFreelist::new()),
        }
    }

    #[inline]
    pub fn freelist_mut(&mut self, class_index: usize) -> &mut ThreadFreelist {
        &mut self.freelists[class_index]
    }
}
