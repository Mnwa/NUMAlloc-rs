use std::alloc::{GlobalAlloc, Layout, System};
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::node_heap::PerNodeHeap;
use crate::platform;

/// Maximum number of NUMA nodes supported.
pub const MAX_NODES: usize = 8;

/// Default virtual-address reservation per node (128 MiB).
/// Only virtual address space is consumed upfront; physical pages are
/// demand-faulted by the kernel.
#[cfg(not(miri))]
pub const DEFAULT_REGION_SIZE: usize = 128 * 1024 * 1024;

/// Under Miri the region is backed by a real (interpreted) allocation, so
/// keep it small; exhaustion simply falls through to the large-object path.
#[cfg(miri)]
pub const DEFAULT_REGION_SIZE: usize = 4 * 1024 * 1024;

/// Every per-node region must hold at least one bag of the largest class.
pub const MIN_REGION_SIZE: usize = crate::size_class::SMALL_LIMIT;

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
        debug_assert!(bag_size <= self.size || self.size == 0);
        let align_mask = bag_size - 1;
        loop {
            let offset = self.bump.load(Ordering::Relaxed);
            // mmap guarantees page alignment, not bag alignment. Align the
            // absolute address so every carved object meets its layout.
            let base = self.base.as_ptr() as usize;
            let address = base.checked_add(offset)?.checked_add(align_mask)? & !align_mask;
            let aligned = address.checked_sub(base)?;
            let new_offset = aligned.checked_add(bag_size)?;
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

    /// Bytes carved from this region so far (advisory snapshot).
    #[cfg(any(test, feature = "internal-testing"))]
    pub fn bump(&self) -> usize {
        self.bump.load(Ordering::Acquire)
    }

    #[cfg(any(test, feature = "internal-testing"))]
    pub fn base(&self) -> NonNull<u8> {
        self.base
    }

    #[cfg(any(test, feature = "internal-testing"))]
    pub fn size(&self) -> usize {
        self.size
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
    /// First byte of node 0's region; aligned to [`MIN_REGION_SIZE`] so that
    /// bag placement (and therefore capacity) does not depend on where the
    /// kernel put the mapping.
    base: NonNull<u8>,
    total_size: usize,
    region_size: usize,
    num_nodes: usize,
    nodes: [NodeRegion; MAX_NODES],
    /// The raw mapping that backs `base` (needed for `munmap`).
    map_base: NonNull<u8>,
    map_size: usize,
}

impl GlobalHeap {
    /// Allocate and initialise the global heap.
    ///
    /// `region_size` is rounded up to a multiple of [`MIN_REGION_SIZE`] and
    /// clamped so that the whole reservation fits in `isize::MAX`.
    pub fn new(num_nodes: usize, region_size: usize) -> Option<Self> {
        let num_nodes = num_nodes.clamp(1, MAX_NODES);
        let region_size = region_size
            .max(MIN_REGION_SIZE)
            .checked_add(MIN_REGION_SIZE - 1)?
            & !(MIN_REGION_SIZE - 1);
        let total_size = region_size.checked_mul(num_nodes)?;
        // Over-map by one alignment unit so the base can be aligned up.
        let map_size = total_size.checked_add(MIN_REGION_SIZE)?;
        if map_size > isize::MAX as usize {
            return None;
        }

        // SAFETY: map_size is non-zero (region_size >= MIN_REGION_SIZE).
        let map_base = unsafe { platform::mmap_anonymous(map_size)? };
        let raw = map_base.as_ptr() as usize;
        let aligned = (raw + MIN_REGION_SIZE - 1) & !(MIN_REGION_SIZE - 1);
        debug_assert!(aligned + total_size <= raw + map_size);
        // SAFETY: `aligned - raw < MIN_REGION_SIZE`, inside the mapping.
        let base = unsafe { NonNull::new_unchecked(map_base.as_ptr().add(aligned - raw)) };

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
            region_size,
            num_nodes,
            nodes,
            map_base,
            map_size,
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
        Some(offset / self.region_size)
    }

    /// Check whether `ptr` belongs to this heap's region.
    #[inline]
    pub fn is_owned(&self, ptr: NonNull<u8>) -> bool {
        self.node_for_ptr(ptr).is_some()
    }

    #[inline]
    pub fn node_region(&self, node: usize) -> &NodeRegion {
        &self.nodes[node]
    }

    #[inline]
    pub fn num_nodes(&self) -> usize {
        self.num_nodes
    }

    #[cfg(any(test, feature = "internal-testing"))]
    pub fn region_size(&self) -> usize {
        self.region_size
    }

    /// Walk every per-node Treiber stack and check the metadata invariants
    /// described in [`crate::validate`].
    ///
    /// # Safety
    /// No other thread may push to or take from the node stacks while the
    /// walk is in progress (the intrusive `next` links are read unsynchronised).
    #[cfg(any(test, feature = "internal-testing"))]
    pub unsafe fn validate(&self, marks: &mut crate::validate::Marks) {
        use crate::size_class::{self, NUM_SIZE_CLASSES};
        assert_eq!(self.total_size, self.region_size * self.num_nodes);
        assert_eq!(
            self.base.as_ptr() as usize % MIN_REGION_SIZE,
            0,
            "heap base is not bag aligned"
        );
        let raw = self.map_base.as_ptr() as usize;
        assert!(
            (raw..=raw + self.map_size - self.total_size).contains(&(self.base.as_ptr() as usize)),
            "heap base outside its mapping"
        );
        for node in 0..self.num_nodes {
            let region = &self.nodes[node];
            let bump = region.bump();
            assert!(
                bump <= region.size,
                "node {node}: bump {bump} > size {}",
                region.size
            );
            assert_eq!(
                region.base.as_ptr() as usize,
                self.base.as_ptr() as usize + node * self.region_size,
                "node {node}: region base mismatch"
            );
            for class in 0..NUM_SIZE_CLASSES {
                let obj = size_class::size_for_class(class);
                let max_len = region.size / obj + 1;
                let mut cursor = region.node_heap.freelist(class).peek_head();
                let mut len = 0usize;
                while let Some(block) = cursor {
                    assert!(len < max_len, "node {node} class {class}: freelist cycle");
                    marks.mark_block(self, node, class, block.cast(), "node stack");
                    // SAFETY: caller guarantees the stack is quiescent.
                    cursor = unsafe { block.as_ref().read_next() };
                    len += 1;
                }
            }
        }
        for node in self.num_nodes..MAX_NODES {
            assert_eq!(self.nodes[node].size, 0, "unused node {node} has a region");
        }
    }
}

// Safety: see NodeRegion reasoning.  The `base` pointer is immutable after
// construction; all mutation goes through per-node atomics.
unsafe impl Send for GlobalHeap {}
unsafe impl Sync for GlobalHeap {}

impl Drop for GlobalHeap {
    fn drop(&mut self) {
        // SAFETY: map_base/map_size are exactly what mmap_anonymous returned.
        unsafe {
            platform::munmap(self.map_base, self.map_size);
        }
    }
}

/// Stable, system-allocated heap ownership shared by an allocator and its TLS
/// caches. Cloning happens only when creating a cache, never per allocation.
pub struct HeapHandle(NonNull<SharedHeap>);

struct SharedHeap {
    references: AtomicUsize,
    heap: GlobalHeap,
}

impl HeapHandle {
    pub fn new(num_nodes: usize, region_size: usize) -> Option<Self> {
        let heap = GlobalHeap::new(num_nodes, region_size)?;
        let layout = Layout::new::<SharedHeap>();
        // SAFETY: layout is nonzero and System avoids allocator recursion.
        let ptr = NonNull::new(unsafe { System.alloc(layout) })?.cast::<SharedHeap>();
        // SAFETY: ptr is an aligned, exclusive allocation for SharedHeap.
        unsafe {
            ptr.write(SharedHeap {
                references: AtomicUsize::new(1),
                heap,
            })
        };
        Some(Self(ptr))
    }
}

impl Deref for HeapHandle {
    type Target = GlobalHeap;
    fn deref(&self) -> &GlobalHeap {
        // SAFETY: this handle keeps the initialized allocation alive.
        unsafe { &self.0.as_ref().heap }
    }
}

impl Clone for HeapHandle {
    fn clone(&self) -> Self {
        // SAFETY: this handle keeps the reference count alive.
        let previous = unsafe { self.0.as_ref() }
            .references
            .fetch_add(1, Ordering::AcqRel);
        if previous >= isize::MAX as usize {
            std::process::abort();
        }
        Self(self.0)
    }
}

impl Drop for HeapHandle {
    fn drop(&mut self) {
        // SAFETY: this handle owns one reference to SharedHeap.
        if unsafe { self.0.as_ref() }
            .references
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            // SAFETY: the last reference exclusively owns the System allocation.
            unsafe {
                self.0.drop_in_place();
                System.dealloc(self.0.cast().as_ptr(), Layout::new::<SharedHeap>());
            }
        }
    }
}

// SAFETY: the reference count is atomic and GlobalHeap is Send + Sync.
unsafe impl Send for HeapHandle {}
// SAFETY: shared access exposes only the synchronized GlobalHeap.
unsafe impl Sync for HeapHandle {}
