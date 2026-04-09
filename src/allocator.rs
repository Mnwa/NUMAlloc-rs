use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use crate::freelist::FreeBlock;
use crate::heap::GlobalHeap;
use crate::platform;
use crate::size_class::{self, BAG_SIZE, SMALL_LIMIT};
use crate::thread_heap::{PerThreadHeap, MAX_THREAD_CACHE, REFILL_BATCH};

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static GLOBAL_HEAP: OnceLock<GlobalHeap> = OnceLock::new();

/// Round-robin counter for assigning threads to NUMA nodes.
static NEXT_NODE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Pointer to the current thread's [`PerThreadHeap`].
    /// Allocated via the **system** allocator to avoid bootstrap recursion.
    static TH_PTR: Cell<Option<NonNull<PerThreadHeap>>> = const { Cell::new(None) };
}

// ---------------------------------------------------------------------------
// Large-object header (for mmap'd allocations)
// ---------------------------------------------------------------------------

#[repr(C)]
struct LargeHeader {
    original_ptr: NonNull<u8>,
    alloc_size: usize,
}

// ---------------------------------------------------------------------------
// NumaAlloc
// ---------------------------------------------------------------------------

/// NUMA-aware memory allocator.
///
/// Use as `#[global_allocator]` or call [`GlobalAlloc`] methods directly.
///
/// ```rust,ignore
/// #[global_allocator]
/// static ALLOC: numalloc::NumaAlloc = numalloc::NumaAlloc::new();
/// ```
pub struct NumaAlloc;

unsafe impl Send for NumaAlloc {}
unsafe impl Sync for NumaAlloc {}

impl Default for NumaAlloc {
    fn default() -> Self {
        Self::new()
    }
}

impl NumaAlloc {
    pub const fn new() -> Self {
        Self
    }

    fn heap() -> &'static GlobalHeap {
        GLOBAL_HEAP.get_or_init(|| {
            let topo = platform::detect_topology();
            GlobalHeap::new(topo.num_nodes).expect("numalloc: failed to mmap heap region")
        })
    }

    /// Obtain (or lazily create) the calling thread's [`PerThreadHeap`].
    ///
    /// The heap struct is allocated from the **system** allocator so that the
    /// very first allocation of a new thread doesn't recurse into NUMAlloc.
    fn thread_heap() -> NonNull<PerThreadHeap> {
        // Fast path: try_with avoids panicking when TLS is being destroyed.
        if let Ok(Some(ptr)) = TH_PTR.try_with(Cell::get) {
            return ptr;
        }

        // Slow path — first allocation on this thread.
        let heap = Self::heap();
        let node = NEXT_NODE.fetch_add(1, Ordering::Relaxed) % heap.num_nodes();

        // Bind thread to its NUMA node (no-op on non-Linux).
        platform::bind_thread_to_node(node);

        // Allocate PerThreadHeap from the system allocator.
        let layout = Layout::new::<PerThreadHeap>();
        let raw = unsafe { System.alloc(layout) } as *mut PerThreadHeap;
        let Some(nn) = NonNull::new(raw) else {
            std::alloc::handle_alloc_error(layout);
        };
        unsafe {
            nn.as_ptr().write(PerThreadHeap::new(node));
        }

        let _ = TH_PTR.try_with(|c| c.set(Some(nn)));
        nn
    }
}

unsafe impl GlobalAlloc for NumaAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let effective_size = layout.size().max(layout.align());

        // --- large object path ---
        if effective_size > SMALL_LIMIT {
            return unsafe { alloc_large(layout) };
        }

        let class_idx = match size_class::size_class_index(effective_size) {
            Some(i) => i,
            None => return std::ptr::null_mut(),
        };

        let th = unsafe { Self::thread_heap().as_mut() };
        let heap = Self::heap();
        let node = th.node_id;
        let fl = th.freelist_mut(class_idx);

        // 1. Try per-thread freelist.
        if let Some(block) = fl.pop() {
            return block.as_ptr().cast();
        }

        // 2. Refill from per-node freelist (lock-free pops).
        let node_fl = heap.node_region(node).node_heap.freelist(class_idx);
        let mut refilled = 0usize;
        while refilled < REFILL_BATCH {
            let Some(b) = node_fl.pop() else { break };
            fl.push(b);
            refilled += 1;
        }
        if refilled > 0 {
            return fl.pop().unwrap().as_ptr().cast();
        }

        // 3. Allocate a new bag and carve it into objects.
        let region = heap.node_region(node);
        let Some(bag) = region.allocate_bag() else {
            return std::ptr::null_mut();
        };

        let obj_size = size_class::size_for_class(class_idx);
        let count = BAG_SIZE / obj_size;
        for i in 0..count {
            let obj = unsafe {
                NonNull::new_unchecked(bag.as_ptr().add(i * obj_size) as *mut FreeBlock)
            };
            fl.push(obj);
        }

        fl.pop().unwrap().as_ptr().cast()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(ptr) = NonNull::new(ptr) else { return };
        let effective_size = layout.size().max(layout.align());

        // --- large object path ---
        if effective_size > SMALL_LIMIT {
            unsafe { dealloc_large(ptr) };
            return;
        }

        let heap = Self::heap();

        if !heap.is_owned(ptr) {
            // Pointer not from our region — treat as large (mmap'd).
            unsafe { dealloc_large(ptr) };
            return;
        }

        let class_idx = match size_class::size_class_index(effective_size) {
            Some(i) => i,
            None => return,
        };

        let origin_node = match heap.node_for_ptr(ptr) {
            Some(n) => n,
            None => return,
        };

        let th = unsafe { Self::thread_heap().as_mut() };
        let current_node = th.node_id;
        let block = ptr.cast::<FreeBlock>();

        if origin_node == current_node {
            // Local deallocation — push to per-thread freelist (no sync).
            let fl = th.freelist_mut(class_idx);
            fl.push(block);

            // Drain excess to per-node heap.
            if fl.count() > MAX_THREAD_CACHE
                && let Some((head, tail, _)) = fl.drain(MAX_THREAD_CACHE / 2)
            {
                heap.node_region(current_node)
                    .node_heap
                    .freelist(class_idx)
                    .push_chain(head, tail);
            }
        } else {
            // Remote deallocation — push directly to origin node's per-node
            // freelist (lock-free, one CAS).
            heap.node_region(origin_node)
                .node_heap
                .freelist(class_idx)
                .push(block);
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let effective_size = layout.size().max(layout.align());

        if effective_size > SMALL_LIMIT {
            // Large path: mmap already returns zeroed pages — the header is
            // written *before* the returned pointer, so the payload is clean.
            return unsafe { self.alloc(layout) };
        }

        // Small path: memory may come from a freelist (stale data), so zero it.
        let ptr = unsafe { self.alloc(layout) };
        if !ptr.is_null() {
            unsafe { std::ptr::write_bytes(ptr, 0, layout.size()) };
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
        let old_effective = old_layout.size().max(old_layout.align());
        let new_effective = new_size.max(old_layout.align());

        // If both old and new land in the same small size class the existing
        // allocation already has enough room — return the pointer as-is.
        if old_effective <= SMALL_LIMIT
            && new_effective <= SMALL_LIMIT
            && let (Some(old_cls), Some(new_cls)) = (
                size_class::size_class_index(old_effective),
                size_class::size_class_index(new_effective),
            )
            && old_cls == new_cls
        {
            return ptr;
        }

        // General case: allocate → copy → deallocate.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, old_layout.align()) };
        let new_ptr = unsafe { self.alloc(new_layout) };
        if new_ptr.is_null() {
            return new_ptr;
        }
        let copy_size = old_layout.size().min(new_size);
        unsafe {
            std::ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
            self.dealloc(ptr, old_layout);
        }
        new_ptr
    }
}

// ---------------------------------------------------------------------------
// Large-object helpers (mmap/munmap)
// ---------------------------------------------------------------------------

/// Allocate a large object backed by its own `mmap` region.
///
/// A [`LargeHeader`] is placed just before the returned pointer so that
/// [`dealloc_large`] can recover the original mmap address and size.
unsafe fn alloc_large(layout: Layout) -> *mut u8 {
    let page_size = platform::page_size();
    let header_size = std::mem::size_of::<LargeHeader>();
    let align = layout.align().max(std::mem::align_of::<LargeHeader>());

    // Over-allocate to guarantee alignment: header + padding + payload.
    let alloc_size = header_size + (align - 1) + layout.size();
    let alloc_size = (alloc_size + page_size - 1) & !(page_size - 1);

    let Some(raw) = (unsafe { platform::mmap_anonymous(alloc_size) }) else {
        return std::ptr::null_mut();
    };

    // Place payload at the first correctly-aligned address after the header.
    let payload_addr = (raw.as_ptr() as usize + header_size + align - 1) & !(align - 1);

    // Write the header immediately before payload.
    let header_ptr = (payload_addr - header_size) as *mut LargeHeader;
    unsafe {
        (*header_ptr).original_ptr = raw;
        (*header_ptr).alloc_size = alloc_size;
    }

    // Optionally bind to the current thread's NUMA node.
    if let Ok(Some(th)) = TH_PTR.try_with(Cell::get) {
        let node = unsafe { th.as_ref().node_id };
        unsafe {
            platform::bind_to_node(raw, alloc_size, node);
        }
    }

    payload_addr as *mut u8
}

/// Free a large object previously returned by [`alloc_large`].
///
/// # Safety
/// `ptr` must have been returned by `alloc_large`.
unsafe fn dealloc_large(ptr: NonNull<u8>) {
    let header_size = std::mem::size_of::<LargeHeader>();
    let header_ptr = unsafe { ptr.as_ptr().sub(header_size) as *mut LargeHeader };
    let original = unsafe { (*header_ptr).original_ptr };
    let size = unsafe { (*header_ptr).alloc_size };
    unsafe {
        platform::munmap(original, size);
    }
}
