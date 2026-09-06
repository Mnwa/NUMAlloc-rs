use std::alloc::{GlobalAlloc, Layout};
use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::freelist::FreeBlock;
use crate::heap::{GlobalHeap, HeapHandle};
use crate::platform;
use crate::size_class::{self, SMALL_LIMIT};
use crate::sys_box::SysBox;
use crate::thread_heap::{MADVISE_THRESHOLD, PerThreadHeap, REFILL_BATCH, max_thread_cache};

// ---------------------------------------------------------------------------
// Per-thread heap guard (cleanup on thread exit)
// ---------------------------------------------------------------------------

/// Thin wrapper around an owned [`SysBox<PerThreadHeap>`] stored in
/// thread-local storage.
///
/// When the owning thread exits the TLS destructor drops this slot, which
/// drops the [`SysBox`], which runs [`PerThreadHeap::drop`] (draining cached
/// freelist blocks back to per-node Treiber stacks and flushing the large
/// object cache), then frees the allocation.  No manual cleanup needed.
struct ThreadHeapSlot {
    inner: UnsafeCell<Option<SysBox<PerThreadHeap>>>,
}

impl ThreadHeapSlot {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(None),
        }
    }

    /// Return a raw pointer to the contained [`PerThreadHeap`], if present.
    ///
    /// The pointer remains valid as long as this slot (and hence the owning
    /// thread's TLS) is alive.
    #[inline]
    fn get(&self) -> Option<NonNull<PerThreadHeap>> {
        // SAFETY: thread-local — only the owning thread accesses this cell.
        let opt = unsafe { &*self.inner.get() };
        opt.as_ref().map(|b| b.as_non_null())
    }

    /// Store an owned [`SysBox<PerThreadHeap>`] into this slot.
    #[inline]
    fn set(&self, val: Option<SysBox<PerThreadHeap>>) {
        // SAFETY: thread-local — only the owning thread accesses this cell.
        // Assigning drops any previous `SysBox`, whose `PerThreadHeap::drop`
        // drains its caches back to the heap it was created for.
        unsafe {
            *self.inner.get() = val;
        }
    }
}

// ---------------------------------------------------------------------------
// Per-instance thread-local storage
// ---------------------------------------------------------------------------

thread_local! {
    /// Owned pointer to the current thread's [`PerThreadHeap`].
    /// Allocated via the **system** allocator to avoid bootstrap recursion.
    /// The [`SysBox`] ensures automatic cleanup on thread exit.
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
/// so multiple allocators do not compete for the same resources.
///
/// Use as `#[global_allocator]` or call [`GlobalAlloc`] methods directly.
///
/// ```rust,ignore
/// #[global_allocator]
/// static ALLOC: numalloc::NumaAlloc = numalloc::NumaAlloc::new();
/// ```
pub struct NumaAlloc {
    heap: OnceLock<HeapHandle>,
    /// Round-robin counter for assigning threads to NUMA nodes.
    next_node: AtomicUsize,
    /// Test-only topology override (`0` = auto-detect).  Only present when
    /// the `internal-testing` feature is on so production layout is unchanged.
    #[cfg(feature = "internal-testing")]
    config: (usize, usize),
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
            #[cfg(feature = "internal-testing")]
            config: (0, 0),
        }
    }

    /// Build an allocator with an explicit virtual topology.
    ///
    /// `num_nodes` is clamped to `1..=MAX_NODES`; `region_size` (bytes per
    /// node) is rounded up to a multiple of 256 KiB.  On hosts without the
    /// requested physical nodes `mbind` simply fails and memory is placed by
    /// the kernel's default policy, so this is safe to use anywhere.  Meant
    /// for tests and fuzz harnesses that need multi-node code paths or quick
    /// region exhaustion.
    #[cfg(feature = "internal-testing")]
    pub const fn with_config(num_nodes: usize, region_size: usize) -> Self {
        Self {
            heap: OnceLock::new(),
            next_node: AtomicUsize::new(0),
            config: (num_nodes, region_size),
        }
    }

    fn heap(&self) -> &HeapHandle {
        self.heap.get_or_init(|| {
            #[cfg(feature = "internal-testing")]
            let (num_nodes, region_size) = if self.config.0 != 0 {
                self.config
            } else {
                (
                    platform::detect_topology().num_nodes,
                    crate::heap::DEFAULT_REGION_SIZE,
                )
            };
            #[cfg(not(feature = "internal-testing"))]
            let (num_nodes, region_size) = (
                platform::detect_topology().num_nodes,
                crate::heap::DEFAULT_REGION_SIZE,
            );
            // GlobalAlloc must not unwind on initialization failure.
            HeapHandle::new(num_nodes, region_size).unwrap_or_else(|| std::process::abort())
        })
    }

    /// Number of NUMA nodes this allocator distributes threads across.
    #[cfg(feature = "internal-testing")]
    pub fn num_nodes(&self) -> usize {
        self.heap().num_nodes()
    }

    /// Whether `ptr` lies inside this allocator's pre-mapped region (as
    /// opposed to a dedicated large-object mapping).
    #[cfg(feature = "internal-testing")]
    pub fn owns(&self, ptr: *mut u8) -> bool {
        NonNull::new(ptr).is_some_and(|p| self.heap().is_owned(p))
    }

    /// Check every allocator-internal invariant that can be verified from
    /// metadata alone, panicking with a description on the first violation.
    /// See [`crate::validate`] for the full list.  Behaviour of the allocator
    /// is not affected; the check only reads metadata and free blocks.
    ///
    /// Covers all per-node Treiber stacks and the *calling* thread's caches.
    /// Other threads' caches are not reachable and are validated when they
    /// drain on thread exit (their blocks then appear on the node stacks).
    ///
    /// # Safety
    /// The allocator must be quiescent: no other thread may allocate,
    /// deallocate or exit (draining its cache) through this allocator while
    /// the walk runs, because the intrusive `next` links of blocks on the
    /// shared stacks are read without synchronisation.
    #[cfg(any(test, feature = "internal-testing"))]
    pub unsafe fn validate_internal_state(&self) {
        let heap = self.heap();
        let mut marks = crate::validate::Marks::new(heap);
        // SAFETY: quiescence is guaranteed by the caller.
        unsafe { heap.validate(&mut marks) };
        if let Ok(Some(th)) = TH_PTR.try_with(ThreadHeapSlot::get) {
            // SAFETY: the TLS slot owns an initialised heap for this thread.
            let th = unsafe { th.as_ref() };
            if std::ptr::eq::<GlobalHeap>(&*th.global_heap, &**heap) {
                th.validate(&mut marks);
            }
        }
    }

    /// A destroyed TLS slot cannot be recreated. Its callers use uncached
    /// mmap allocations or return freed blocks directly to their origin node.
    fn thread_heap(&self) -> Option<NonNull<PerThreadHeap>> {
        TH_PTR
            .try_with(|slot| {
                let heap = self.heap();
                if let Some(ptr) = slot.get() {
                    // SAFETY: the TLS slot owns the initialized thread heap.
                    if std::ptr::eq::<GlobalHeap>(&*unsafe { ptr.as_ref() }.global_heap, &**heap) {
                        return ptr;
                    }
                    // Switching allocators relinquishes this thread's cache;
                    // dropping the SysBox drains it to its original heap.
                    slot.set(None);
                }
                // Advisory round-robin assignment has no publication dependency.
                let node = self.next_node.fetch_add(1, Ordering::Relaxed) % heap.num_nodes();
                // Allocated via SysBox (system allocator) so that the very
                // first allocation of a new thread doesn't recurse into NUMAlloc.
                let boxed = SysBox::new(PerThreadHeap::new(node, heap.clone()));
                let ptr = boxed.as_non_null();
                // Register in TLS BEFORE bind_thread_to_node so any allocation
                // it triggers sees the cache instead of re-entering this path.
                slot.set(Some(boxed));
                platform::bind_thread_to_node(node);
                ptr
            })
            .ok()
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
            // SAFETY: valid caller layout; late TLS allocations bypass caches.
            return unsafe { self.alloc_large(layout) };
        };
        // SAFETY: this thread exclusively owns its cache; bootstrap does not recurse.
        let th = unsafe { th.as_mut() };
        let node = th.node_id;
        let fl = th.freelist_mut(class_idx);

        // 1. Try per-thread freelist.
        if let Some(block) = fl.pop() {
            return block.as_ptr().cast();
        }

        // 2. Refill.  Use this thread's parked spare chain if it has one;
        //    otherwise detach the whole shared chain in one CAS.  Intrusive
        //    links are only ever read on a chain this thread owns outright
        //    (a per-block pop would read `next` of a block another thread
        //    may already have handed to user code).  Only REFILL_BATCH
        //    blocks are walked; the remainder is parked as spare, so refill
        //    cost is bounded whatever the node stack length.
        let heap = self.heap();
        let chain = fl.take_spare().or_else(|| {
            heap.node_region(node)
                .node_heap
                .freelist(class_idx)
                .take_all()
        });
        if let Some(first) = chain {
            let mut tail = first;
            let mut count = 1;
            loop {
                // SAFETY: the chain is exclusively owned by this thread.
                let next = unsafe { tail.as_ref().read_next() };
                if count >= REFILL_BATCH {
                    fl.set_spare(next);
                    // SAFETY: as above; terminates the counted chain.
                    unsafe { tail.as_ref().write_next(None) };
                    break;
                }
                match next {
                    Some(n) => {
                        tail = n;
                        count += 1;
                    }
                    None => break,
                }
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

        let heap = self.heap();

        if !heap.is_owned(ptr) {
            // Pointer not from our region — treat as large (mmap'd).
            unsafe { dealloc_large(ptr, Some(self)) };
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
        // A block handed out from a bag is always aligned to its class size
        // and lies below the bump pointer; anything else is a caller passing
        // a wrong layout/pointer or an internal carving bug.
        debug_assert_eq!(
            ptr.as_ptr() as usize % size_class::size_for_class(class_idx),
            0,
            "dealloc: pointer misaligned for its size class"
        );

        let Some(mut th) = self.thread_heap() else {
            heap.node_region(origin_node)
                .node_heap
                .freelist(class_idx)
                .push(ptr.cast());
            return;
        };
        // SAFETY: the thread owns the initialized cache exclusively.
        let th = unsafe { th.as_mut() };
        let current_node = th.node_id;
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
            // A fresh anonymous mapping is zero-filled lazily by the kernel;
            // touching it here would defeat that.  A mapping reused from the
            // per-thread large cache may hold stale bytes (MADV_DONTNEED is
            // only applied above MADVISE_THRESHOLD, and macOS MADV_FREE keeps
            // contents), so only those are zeroed explicitly.
            // SAFETY: layout is the caller's valid allocation request.
            let (ptr, reused) = unsafe { self.alloc_large_tracked(layout) };
            if !ptr.is_null() && reused {
                // SAFETY: ptr is valid for layout.size() bytes.
                unsafe { std::ptr::write_bytes(ptr, 0, layout.size()) };
            }
            return ptr;
        }
        // Small path: memory may come from a freelist (stale data), so zero it.
        // (If the region is exhausted this falls back to a fresh mapping and
        // the memset is merely redundant, never wrong.)
        // SAFETY: layout is the caller's valid allocation request.
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
            && NonNull::new(ptr).is_some_and(|p| self.heap().is_owned(p))
        {
            return ptr;
        }

        // General case: allocate → copy → deallocate.  The caller contract
        // already forbids `new_size` overflowing `isize::MAX` after rounding
        // to the alignment, but a single compare is cheap insurance against
        // silently building an invalid layout.
        let Ok(new_layout) = Layout::from_size_align(new_size, old_layout.align()) else {
            return std::ptr::null_mut();
        };
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

impl NumaAlloc {
    /// Compute the total mmap size needed for a large allocation.
    #[inline]
    fn large_alloc_size(layout: &Layout) -> Option<usize> {
        let page_size = platform::page_size();
        let header_size = std::mem::size_of::<LargeHeader>();
        let align = layout.align().max(std::mem::align_of::<LargeHeader>());
        let alloc_size = header_size
            .checked_add(align - 1)?
            .checked_add(layout.size())?;
        let rounded = alloc_size.checked_add(page_size - 1)? & !(page_size - 1);
        (rounded <= isize::MAX as usize).then_some(rounded)
    }

    /// Place the [`LargeHeader`] and return the payload pointer for a given
    /// raw mmap base, alloc size, and requested layout.
    #[inline]
    unsafe fn prepare_large_payload(
        raw: NonNull<u8>,
        alloc_size: usize,
        layout: &Layout,
    ) -> *mut u8 {
        let header_size = std::mem::size_of::<LargeHeader>();
        let align = layout.align().max(std::mem::align_of::<LargeHeader>());
        let payload_addr = (raw.as_ptr() as usize + header_size + align - 1) & !(align - 1);
        debug_assert!(
            payload_addr - (raw.as_ptr() as usize) + layout.size() <= alloc_size,
            "large payload does not fit its mapping"
        );

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
    unsafe fn alloc_large(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded contract.
        unsafe { self.alloc_large_tracked(layout) }.0
    }

    /// [`Self::alloc_large`] that also reports whether the mapping was reused
    /// from the per-thread cache (`true`) or freshly mapped (`false`).
    ///
    /// # Safety
    /// Same as [`GlobalAlloc::alloc`].
    unsafe fn alloc_large_tracked(&self, layout: Layout) -> (*mut u8, bool) {
        let Some(alloc_size) = Self::large_alloc_size(&layout) else {
            return (std::ptr::null_mut(), false);
        };
        let mut thread = self.thread_heap();
        // SAFETY: a present TLS pointer is initialized and exclusively owned.
        let th = thread.as_mut().map(|ptr| unsafe { ptr.as_mut() });
        let node = th.as_ref().map_or(0, |th| th.node_id);

        // Fast path: check per-thread large cache.
        if let Some((raw, mapped_size)) = th.and_then(|th| th.large_cache_take(alloc_size)) {
            // SAFETY: retain the full mapping extent, including close-size slack.
            return (
                unsafe { Self::prepare_large_payload(raw, mapped_size, &layout) },
                true,
            );
        }

        // Slow path: mmap a fresh region.
        let Some(raw) = (unsafe { platform::mmap_anonymous(alloc_size) }) else {
            return (std::ptr::null_mut(), false);
        };

        // Bind to the current thread's NUMA node.
        unsafe {
            platform::bind_to_node(raw, alloc_size, node);
        }

        (
            unsafe { Self::prepare_large_payload(raw, alloc_size, &layout) },
            false,
        )
    }

    /// Try to cache a freed large mapping; returns `false` if the cache is
    /// full (caller should `munmap`).
    #[inline]
    fn try_cache_large(&self, original: NonNull<u8>, alloc_size: usize) -> bool {
        if let Ok(Some(mut th)) = TH_PTR.try_with(ThreadHeapSlot::get) {
            // SAFETY: initialized cache owned by this thread.
            let th = unsafe { th.as_mut() };
            if !std::ptr::eq::<GlobalHeap>(&*th.global_heap, &**self.heap()) {
                return false;
            }
            if th.large_cache_put(original, alloc_size) {
                // Only release physical pages for large regions; for smaller
                // ones the madvise syscall overhead exceeds the savings.
                if alloc_size >= MADVISE_THRESHOLD {
                    // SAFETY: original/alloc_size describe a valid mmap region.
                    unsafe {
                        platform::madvise_dontneed(original, alloc_size);
                    }
                }
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
