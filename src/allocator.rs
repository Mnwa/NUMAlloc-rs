use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::freelist::FreeBlock;
use crate::heap::GlobalHeap;
use crate::platform;
use crate::size_class::{self, SMALL_LIMIT};
use crate::thread_heap::{PerThreadHeap, REFILL_BATCH, max_thread_cache};

// ---------------------------------------------------------------------------
// Per-thread heap guard (cleanup on thread exit)
// ---------------------------------------------------------------------------

/// Head of the calling thread's list of [`PerThreadHeap`]s (one per
/// [`NumaAlloc`] instance the thread has touched). Drains cached freelist
/// blocks back to the per-node Treiber stacks when the owning thread exits.
/// This prevents progressive region exhaustion caused by short-lived threads
/// stranding blocks in their thread-local caches.
struct ThreadHeapSlot {
    inner: Cell<Option<NonNull<PerThreadHeap>>>,
}

impl ThreadHeapSlot {
    const fn new() -> Self {
        Self {
            inner: Cell::new(None),
        }
    }

    #[inline]
    fn get(&self) -> Option<NonNull<PerThreadHeap>> {
        self.inner.get()
    }

    #[inline]
    fn set(&self, val: Option<NonNull<PerThreadHeap>>) {
        self.inner.set(val);
    }
}

impl Drop for ThreadHeapSlot {
    fn drop(&mut self) {
        let mut cur = self.inner.take();
        while let Some(mut th_ptr) = cur {
            // SAFETY: every node in the list was allocated via `System.alloc`
            // in `NumaAlloc::thread_heap_slow` and points to a valid
            // `PerThreadHeap`.  The owning `GlobalHeap` lives inside a
            // `NumaAlloc` that (per the type's contract) outlives every
            // thread that used it.
            unsafe {
                let th = th_ptr.as_mut();
                cur = th.next;
                th.drain_to_node_heap();
                System.dealloc(th_ptr.as_ptr() as *mut u8, Layout::new::<PerThreadHeap>());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-local storage
// ---------------------------------------------------------------------------

thread_local! {
    /// Head of the current thread's [`PerThreadHeap`] list.
    /// Allocated via the **system** allocator to avoid bootstrap recursion.
    /// The [`ThreadHeapSlot`] wrapper ensures cleanup on thread exit.
    static TH_PTR: ThreadHeapSlot = const { ThreadHeapSlot::new() };
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
/// Each instance owns an independent [`GlobalHeap`] and round-robin counter,
/// so multiple allocators do not compete for the same resources. Per-thread
/// caches are kept per instance as well, so a block allocated through one
/// instance is always returned to that instance's heap.
///
/// Use as `#[global_allocator]` or call [`GlobalAlloc`] methods directly.
/// An instance must outlive every thread that allocates through it — in
/// practice, store it in a `static`.
///
/// ```rust,ignore
/// #[global_allocator]
/// static ALLOC: numalloc::NumaAlloc = numalloc::NumaAlloc::new();
/// ```
pub struct NumaAlloc {
    heap: OnceLock<GlobalHeap>,
    /// Round-robin counter for assigning threads to NUMA nodes.
    next_node: AtomicUsize,
}

// Safety: `OnceLock` and `AtomicUsize` are both `Send + Sync`.  The
// `GlobalHeap` stored inside the `OnceLock` is also `Send + Sync` (see
// heap.rs).
unsafe impl Send for NumaAlloc {}
unsafe impl Sync for NumaAlloc {}

impl Default for NumaAlloc {
    fn default() -> Self {
        Self::new()
    }
}

impl NumaAlloc {
    pub const fn new() -> Self {
        Self {
            heap: OnceLock::new(),
            next_node: AtomicUsize::new(0),
        }
    }

    /// Lazily initialise and return the global heap.
    ///
    /// Initialisation must not allocate through the global allocator: if this
    /// instance *is* the global allocator, a re-entrant `alloc` would
    /// deadlock on the `OnceLock`.  `detect_topology` and `GlobalHeap::new`
    /// therefore only use raw `libc` calls.
    fn heap(&self) -> &GlobalHeap {
        self.heap.get_or_init(|| {
            let topo = platform::detect_topology();
            GlobalHeap::new(topo.num_nodes).expect("numalloc: failed to mmap heap region")
        })
    }

    /// Obtain (or lazily create) the calling thread's [`PerThreadHeap`] for
    /// **this** allocator instance.
    ///
    /// Returns `None` when thread-local storage is no longer available (the
    /// thread is running its TLS destructors).  Callers must then fall back
    /// to the heap-less paths, which go straight to the per-node stacks.
    ///
    /// The heap struct is allocated from the **system** allocator so that the
    /// very first allocation of a new thread doesn't recurse into NUMAlloc.
    #[inline]
    fn thread_heap(&self) -> Option<NonNull<PerThreadHeap>> {
        // Fast path: the head of the list belongs to this instance.
        // `try_with` avoids panicking when TLS is being destroyed.
        let head = TH_PTR.try_with(ThreadHeapSlot::get).ok()?;
        if let Some(th) = head {
            // SAFETY: list nodes are valid until the slot is dropped.
            let owner = unsafe { th.as_ref() }.global_heap;
            if self
                .heap
                .get()
                .is_some_and(|h| std::ptr::eq(h, owner.as_ptr()))
            {
                return Some(th);
            }
        }
        self.thread_heap_slow(head)
    }

    #[cold]
    fn thread_heap_slow(
        &self,
        head: Option<NonNull<PerThreadHeap>>,
    ) -> Option<NonNull<PerThreadHeap>> {
        let heap = self.heap();
        let heap_ptr = NonNull::from(heap);

        // Another instance owns the head; search the rest of the list.
        let mut cur = head;
        while let Some(th) = cur {
            // SAFETY: list nodes are valid until the slot is dropped.
            let th_ref = unsafe { th.as_ref() };
            if th_ref.global_heap == heap_ptr {
                return Some(th);
            }
            cur = th_ref.next;
        }

        // First allocation on this thread through this instance.
        let node = self.next_node.fetch_add(1, Ordering::Relaxed) % heap.num_nodes();

        // Allocate PerThreadHeap from the system allocator.
        let layout = Layout::new::<PerThreadHeap>();
        let raw = unsafe { System.alloc(layout) } as *mut PerThreadHeap;
        let Some(nn) = NonNull::new(raw) else {
            std::alloc::handle_alloc_error(layout);
        };
        unsafe {
            nn.as_ptr().write(PerThreadHeap::new(node, heap_ptr, head));
        }

        // Register in TLS BEFORE bind_thread_to_node, which allocates
        // (std::fs::read_to_string).  Without this, the recursive alloc call
        // would see TH_PTR as empty and re-enter this slow path infinitely.
        if TH_PTR.try_with(|slot| slot.set(Some(nn))).is_err() {
            // TLS already destroyed: nothing can own this heap, so undo and
            // report "no thread heap".  (Cannot happen when `head` was
            // `Some`, because reading it above succeeded.)
            unsafe {
                nn.as_ptr().drop_in_place();
                System.dealloc(raw as *mut u8, layout);
            }
            return None;
        }

        // Bind thread to its NUMA node (no-op on non-Linux).
        platform::bind_thread_to_node(node);

        Some(nn)
    }
}

unsafe impl GlobalAlloc for NumaAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let effective_size = layout.size().max(layout.align());

        // --- large object path ---
        if effective_size > SMALL_LIMIT {
            return unsafe { self.alloc_large(layout) };
        }

        let class_idx = match size_class::size_class_index(effective_size) {
            Some(i) => i,
            None => return std::ptr::null_mut(),
        };

        let Some(mut th) = self.thread_heap() else {
            return unsafe { self.alloc_orphan(class_idx, layout) };
        };
        let th = unsafe { th.as_mut() };
        let node = th.node_id;
        // SAFETY: global_heap was set in PerThreadHeap::new and points into
        // `self.heap`, which outlives all threads using this instance.
        let heap = unsafe { th.global_heap.as_ref() };
        let fl = th.freelist_mut(class_idx);

        // 1. Try per-thread freelist — hottest path, no atomics, no heap lookup.
        if let Some(block) = fl.pop() {
            return block.as_ptr().cast();
        }

        // 2. Refill from per-node freelist.
        //    Pop individually (each is one CAS) but batch-insert into the
        //    thread freelist via push_chain for O(1) insertion.
        let node_fl = heap.node_region(node).node_heap.freelist(class_idx);
        if let Some(first) = node_fl.pop() {
            unsafe { first.as_ref().write_next(None) };
            let mut tail = first;
            let mut count = 1usize;
            while count < REFILL_BATCH {
                let Some(b) = node_fl.pop() else { break };
                unsafe { b.as_ref().write_next(None) };
                unsafe { tail.as_ref().write_next(Some(b)) };
                tail = b;
                count += 1;
            }
            fl.push_chain(first, tail, count);
            return fl.pop().unwrap().as_ptr().cast();
        }

        // 3. Allocate a new bag and carve it into objects.
        let region = heap.node_region(node);
        let bag_size = size_class::bag_size_for_class(class_idx);
        let Some(bag) = region.allocate_bag(bag_size) else {
            // Region exhausted — fall back to mmap for this allocation.
            return unsafe { self.alloc_large(layout) };
        };

        let obj_size = size_class::size_for_class(class_idx);
        let count = bag_size / obj_size;
        for i in 0..count {
            let obj =
                unsafe { NonNull::new_unchecked(bag.as_ptr().add(i * obj_size) as *mut FreeBlock) };
            fl.push(obj);
        }

        fl.pop().unwrap().as_ptr().cast()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(ptr) = NonNull::new(ptr) else { return };
        let effective_size = layout.size().max(layout.align());

        // --- large object path ---
        if effective_size > SMALL_LIMIT {
            unsafe { dealloc_large(ptr, Some(self)) };
            return;
        }

        let class_idx = match size_class::size_class_index(effective_size) {
            Some(i) => i,
            None => return,
        };

        let Some(mut th) = self.thread_heap() else {
            unsafe { self.dealloc_orphan(ptr, class_idx) };
            return;
        };
        let th = unsafe { th.as_mut() };
        // SAFETY: see `alloc`.
        let heap = unsafe { th.global_heap.as_ref() };
        let current_node = th.node_id;

        // node_for_ptr does bounds check + node lookup in one shot.  A small
        // request that fell back to mmap on region exhaustion lives outside
        // the region and carries a `LargeHeader`.
        let origin_node = match heap.node_for_ptr(ptr) {
            Some(n) => n,
            None => {
                unsafe { dealloc_large(ptr, Some(self)) };
                return;
            }
        };

        let block = ptr.cast::<FreeBlock>();

        if origin_node == current_node {
            // Local deallocation — push to per-thread freelist (no sync).
            let fl = th.freelist_mut(class_idx);
            fl.push(block);

            // Drain excess to per-node heap.
            let cache_limit = max_thread_cache(class_idx);
            if fl.count() > cache_limit
                && let Some((head, tail, _)) = fl.drain(cache_limit / 2)
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
            // Large path: a fresh mmap is already zeroed, but a mapping
            // reused from the per-thread large cache still holds the previous
            // owner's data and must be cleared.
            let (ptr, from_cache) = unsafe { self.alloc_large_inner(layout) };
            if from_cache && !ptr.is_null() {
                unsafe { std::ptr::write_bytes(ptr, 0, layout.size()) };
            }
            return ptr;
        }

        // Small path: memory may come from a freelist (stale data), so zero it.
        // (A small request that fell back to mmap is zeroed too — cheap, and
        // it may have come from the large cache as well.)
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
        // block already has room for the whole class — return it as-is.
        // This only holds for blocks carved from a bag inside the region: a
        // small request that fell back to mmap is exactly `old_layout.size()`
        // bytes, so it must go through the general path.
        if old_effective <= SMALL_LIMIT
            && new_effective <= SMALL_LIMIT
            && let (Some(old_cls), Some(new_cls)) = (
                size_class::size_class_index(old_effective),
                size_class::size_class_index(new_effective),
            )
            && old_cls == new_cls
            && let Some(nn) = NonNull::new(ptr)
            && self.heap().node_for_ptr(nn).is_some()
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
// Fuzzing seam
// ---------------------------------------------------------------------------

#[cfg(feature = "fuzz-hooks")]
impl NumaAlloc {
    /// Return the allocator to a pristine state so that fuzz iterations are
    /// independent and every finding replays from a single input.
    ///
    /// Rewinds every node's bump pointer, empties the per-node stacks, clears
    /// the calling thread's freelists and large-object cache, and restarts
    /// round-robin node assignment. The virtual region stays mapped.
    ///
    /// # Safety
    /// No allocation obtained from this instance may still be in use, on any
    /// thread, and no other thread may be allocating or freeing through it
    /// concurrently. Per-thread heaps of *other* live threads are not
    /// touched and would afterwards hold blocks that overlap fresh bags —
    /// so all other threads that used this instance must have exited.
    pub unsafe fn fuzz_reset(&self) {
        if let Some(heap) = self.heap.get() {
            heap.reset();
        }
        if let Some(mut th) = self.thread_heap() {
            // SAFETY: exclusively owned by the calling thread.
            unsafe { th.as_mut() }.clear_caches();
        }
        self.next_node.store(0, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Heap-less fallback (TLS destroyed)
// ---------------------------------------------------------------------------

/// Number of allocations served by [`NumaAlloc::alloc_orphan`] (tests only).
#[cfg(test)]
pub(crate) static ORPHAN_ALLOCS: AtomicUsize = AtomicUsize::new(0);

impl NumaAlloc {
    /// Small allocation for a thread whose TLS has already been torn down
    /// (e.g. another thread-local destructor allocating after ours ran).
    /// Goes straight to the per-node Treiber stack, then the bump allocator.
    #[cold]
    unsafe fn alloc_orphan(&self, class_idx: usize, layout: Layout) -> *mut u8 {
        #[cfg(test)]
        ORPHAN_ALLOCS.fetch_add(1, Ordering::Relaxed);
        let heap = self.heap();
        let node = self.next_node.load(Ordering::Relaxed) % heap.num_nodes();
        let region = heap.node_region(node);
        let node_fl = region.node_heap.freelist(class_idx);

        if let Some(block) = node_fl.pop() {
            return block.as_ptr().cast();
        }

        let bag_size = size_class::bag_size_for_class(class_idx);
        let Some(bag) = region.allocate_bag(bag_size) else {
            return unsafe { self.alloc_large(layout) };
        };

        // Return object 0; chain objects 1..count and push them to the node
        // stack in a single CAS.
        let obj_size = size_class::size_for_class(class_idx);
        let count = bag_size / obj_size;
        if count > 1 {
            // SAFETY: `i < count`, so every object lies inside the bag.
            let at = |i: usize| unsafe {
                NonNull::new_unchecked(bag.as_ptr().add(i * obj_size) as *mut FreeBlock)
            };
            for i in 1..count - 1 {
                unsafe { at(i).as_ref().write_next(Some(at(i + 1))) };
            }
            unsafe { at(count - 1).as_ref().write_next(None) };
            node_fl.push_chain(at(1), at(count - 1));
        }
        bag.as_ptr()
    }

    /// Small deallocation for a thread whose TLS has already been torn down:
    /// push directly to the origin node's Treiber stack.
    #[cold]
    unsafe fn dealloc_orphan(&self, ptr: NonNull<u8>, class_idx: usize) {
        let heap = self.heap();
        match heap.node_for_ptr(ptr) {
            Some(node) => heap
                .node_region(node)
                .node_heap
                .freelist(class_idx)
                .push(ptr.cast::<FreeBlock>()),
            None => unsafe { dealloc_large(ptr, None) },
        }
    }
}

// ---------------------------------------------------------------------------
// Large-object helpers (mmap/munmap)
// ---------------------------------------------------------------------------

impl NumaAlloc {
    /// Compute the total mmap size needed for a large allocation.
    #[inline]
    fn large_alloc_size(layout: &Layout) -> usize {
        let page_size = platform::page_size();
        let header_size = std::mem::size_of::<LargeHeader>();
        let align = layout.align().max(std::mem::align_of::<LargeHeader>());
        let alloc_size = header_size + (align - 1) + layout.size();
        (alloc_size + page_size - 1) & !(page_size - 1)
    }

    /// Place the [`LargeHeader`] and return the payload pointer for a given
    /// raw mmap base, its **actual** mapping size, and the requested layout.
    #[inline]
    unsafe fn prepare_large_payload(
        raw: NonNull<u8>,
        alloc_size: usize,
        layout: &Layout,
    ) -> *mut u8 {
        let header_size = std::mem::size_of::<LargeHeader>();
        let align = layout.align().max(std::mem::align_of::<LargeHeader>());
        let payload_addr = (raw.as_ptr() as usize + header_size + align - 1) & !(align - 1);
        debug_assert!(payload_addr + layout.size() <= raw.as_ptr() as usize + alloc_size);

        // SAFETY: payload_addr - header_size is within the mmap region and
        // correctly aligned for LargeHeader.
        let header_ptr =
            unsafe { NonNull::new_unchecked((payload_addr - header_size) as *mut LargeHeader) };
        unsafe {
            header_ptr.as_ptr().write(LargeHeader {
                original_ptr: raw,
                alloc_size,
            });
        }
        payload_addr as *mut u8
    }

    /// Allocate a large object backed by its own `mmap` region.
    ///
    /// A [`LargeHeader`] is placed just before the returned pointer so that
    /// [`dealloc_large`] can recover the original mmap address and size.
    #[inline]
    unsafe fn alloc_large(&self, layout: Layout) -> *mut u8 {
        unsafe { self.alloc_large_inner(layout) }.0
    }

    /// Like [`Self::alloc_large`], also reporting whether the mapping was
    /// reused from the per-thread cache (and therefore may hold stale data).
    unsafe fn alloc_large_inner(&self, layout: Layout) -> (*mut u8, bool) {
        let alloc_size = Self::large_alloc_size(&layout);
        let th = self.thread_heap();

        // Fast path: check per-thread large cache.  The cached mapping may be
        // slightly larger than requested; its real size goes in the header
        // so that the eventual munmap releases the whole mapping.
        if let Some(mut th) = th
            && let Some((raw, actual_size)) = unsafe { th.as_mut() }.large_cache_take(alloc_size)
        {
            debug_assert!(actual_size >= alloc_size);
            return (
                unsafe { Self::prepare_large_payload(raw, actual_size, &layout) },
                true,
            );
        }

        // Slow path: mmap a fresh region.
        let Some(raw) = (unsafe { platform::mmap_anonymous(alloc_size) }) else {
            return (std::ptr::null_mut(), false);
        };

        // Bind to the current thread's NUMA node.
        if let Some(th) = th {
            let node = unsafe { th.as_ref() }.node_id;
            unsafe {
                platform::bind_to_node(raw, alloc_size, node);
            }
        }

        (
            unsafe { Self::prepare_large_payload(raw, alloc_size, &layout) },
            false,
        )
    }

    /// Try to cache a freed large mapping; returns `false` if the cache is
    /// full (caller should `munmap`).
    ///
    /// Physical pages are **not** released here — the cached region retains its
    /// pages so that a subsequent reuse avoids costly zero-fault page-ins.
    /// Pages are released only when an entry is evicted from the cache (via
    /// `munmap`).
    #[inline]
    fn try_cache_large(&self, original: NonNull<u8>, alloc_size: usize) -> bool {
        if let Some(mut th) = self.thread_heap() {
            let th = unsafe { th.as_mut() };
            if th.large_cache_put(original, alloc_size) {
                return true;
            }
        }
        false
    }
}

/// Free a large object previously returned by [`NumaAlloc::alloc_large`].
///
/// # Safety
/// `ptr` must have been returned by `alloc_large`.  `allocator` is used to
/// attempt caching; pass `None` to force immediate `munmap`.
unsafe fn dealloc_large(ptr: NonNull<u8>, allocator: Option<&NumaAlloc>) {
    let header_size = std::mem::size_of::<LargeHeader>();
    // SAFETY: ptr was returned by prepare_large_payload; subtracting
    // header_size yields the LargeHeader that was written at allocation time.
    let header_ptr =
        unsafe { NonNull::new_unchecked(ptr.as_ptr().sub(header_size) as *mut LargeHeader) };
    let header = unsafe { header_ptr.as_ref() };
    let original = header.original_ptr;
    let size = header.alloc_size;

    // Cheap sanity checks (debug/fuzz builds only): a corrupted header —
    // e.g. an overflow from the neighbouring mapping — shows up here instead
    // of as a silent bogus munmap.
    debug_assert!(
        size != 0 && size % platform::page_size() == 0,
        "numalloc: corrupt LargeHeader size {size:#x} for {ptr:p}"
    );
    debug_assert!(
        original <= header_ptr.cast::<u8>()
            && (ptr.as_ptr() as usize) < original.as_ptr() as usize + size,
        "numalloc: corrupt LargeHeader base {original:p} (size {size:#x}) for {ptr:p}"
    );

    // Try to cache for reuse.
    if let Some(alloc) = allocator
        && alloc.try_cache_large(original, size)
    {
        return;
    }

    unsafe {
        platform::munmap(original, size);
    }
}
