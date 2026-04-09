use crate::freelist::TreiberStack;
use crate::size_class::NUM_SIZE_CLASSES;

/// Shared per-node heap.  Each size class has its own lock-free Treiber stack
/// so that remote deallocations and per-thread refills are wait-free.
pub struct PerNodeHeap {
    freelists: [TreiberStack; NUM_SIZE_CLASSES],
}

impl PerNodeHeap {
    pub fn new() -> Self {
        Self {
            freelists: std::array::from_fn(|_| TreiberStack::new()),
        }
    }

    #[inline]
    pub fn freelist(&self, class_index: usize) -> &TreiberStack {
        &self.freelists[class_index]
    }
}
