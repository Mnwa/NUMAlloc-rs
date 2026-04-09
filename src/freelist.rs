use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// FreeBlock — intrusive node stored inside freed memory
// ---------------------------------------------------------------------------

/// Intrusive linked-list node stored directly in freed memory blocks.
///
/// The `next` field is wrapped in [`UnsafeCell`] because it is mutated through
/// shared pointers in the lock-free [`TreiberStack`]: a pushing thread writes
/// `next` while other threads may concurrently read different blocks' `next`
/// fields during a pop.
///
/// The block must be at least `size_of::<FreeBlock>()` bytes (8 on 64-bit).
#[repr(C)]
pub struct FreeBlock {
    next: UnsafeCell<Option<NonNull<FreeBlock>>>,
}

impl FreeBlock {
    /// Read the `next` pointer.
    ///
    /// # Safety
    /// Caller must ensure no data race on `self.next` (i.e. proper
    /// synchronisation via the Treiber stack's CAS or single-thread access).
    #[inline]
    pub unsafe fn read_next(&self) -> Option<NonNull<FreeBlock>> {
        unsafe { *self.next.get() }
    }

    /// Write the `next` pointer.
    ///
    /// # Safety
    /// Caller must ensure exclusive logical ownership of the block (the CAS
    /// loop in the Treiber stack provides this guarantee).
    #[inline]
    pub unsafe fn write_next(&self, val: Option<NonNull<FreeBlock>>) {
        unsafe { self.next.get().write(val) }
    }
}

// FreeBlock is Send: ownership is transferred between threads via atomics.
// It is intentionally !Sync (due to UnsafeCell) — shared &FreeBlock across
// threads without synchronisation would be unsound.
unsafe impl Send for FreeBlock {}

// ---------------------------------------------------------------------------
// TreiberStack — lock-free MPSC/MPMC stack with ABA protection
// ---------------------------------------------------------------------------

/// A lock-free Treiber stack using a 48-bit pointer + 16-bit generation tag
/// packed into a single `AtomicU64` to prevent the ABA problem.
///
/// On 64-bit platforms user-space addresses fit in 48 bits, leaving the upper
/// 16 bits for a monotonically-increasing tag that makes each CAS unique.
pub struct TreiberStack {
    head: AtomicU64,
}

impl TreiberStack {
    pub const fn new() -> Self {
        Self {
            head: AtomicU64::new(0),
        }
    }

    #[inline]
    fn pack(ptr: Option<NonNull<FreeBlock>>, tag: u16) -> u64 {
        let raw = match ptr {
            Some(p) => p.as_ptr() as u64,
            None => 0,
        };
        ((tag as u64) << 48) | (raw & 0x0000_FFFF_FFFF_FFFF)
    }

    #[inline]
    fn unpack(val: u64) -> (Option<NonNull<FreeBlock>>, u16) {
        let raw = (val & 0x0000_FFFF_FFFF_FFFF) as *mut FreeBlock;
        let ptr = NonNull::new(raw);
        let tag = (val >> 48) as u16;
        (ptr, tag)
    }

    /// Push a single block onto the stack (lock-free).
    pub fn push(&self, block: NonNull<FreeBlock>) {
        loop {
            let current = self.head.load(Ordering::Acquire);
            let (head, tag) = Self::unpack(current);
            unsafe { block.as_ref().write_next(head) };
            let new = Self::pack(Some(block), tag.wrapping_add(1));
            if self
                .head
                .compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Pop a single block from the stack (lock-free).
    pub fn pop(&self) -> Option<NonNull<FreeBlock>> {
        loop {
            let current = self.head.load(Ordering::Acquire);
            let (head, tag) = Self::unpack(current);
            let head = head?;
            let next = unsafe { head.as_ref().read_next() };
            let new = Self::pack(next, tag.wrapping_add(1));
            if self
                .head
                .compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(head);
            }
        }
    }

    /// Push a chain of blocks from `first` through `last` in one CAS.
    /// `last.next` is overwritten to point to the previous stack top.
    ///
    /// # Safety
    /// `last` must be reachable by following `next` pointers from `first`.
    pub fn push_chain(&self, first: NonNull<FreeBlock>, last: NonNull<FreeBlock>) {
        loop {
            let current = self.head.load(Ordering::Acquire);
            let (head, tag) = Self::unpack(current);
            unsafe { last.as_ref().write_next(head) };
            let new = Self::pack(Some(first), tag.wrapping_add(1));
            if self
                .head
                .compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Returns `true` when the stack appears empty.
    /// (Another thread may push concurrently, so this is advisory.)
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        let (head, _) = Self::unpack(self.head.load(Ordering::Relaxed));
        head.is_none()
    }
}

// Safety: The stack is designed for concurrent access from multiple threads.
// All shared state is behind AtomicU64; FreeBlock fields use UnsafeCell.
unsafe impl Send for TreiberStack {}
unsafe impl Sync for TreiberStack {}

// ---------------------------------------------------------------------------
// ThreadFreelist — single-threaded intrusive list with bulk drain
// ---------------------------------------------------------------------------

/// Per-thread singly-linked freelist.  Not `Send`/`Sync` — used exclusively
/// by a single thread through thread-local storage.
pub struct ThreadFreelist {
    head: Option<NonNull<FreeBlock>>,
    tail: Option<NonNull<FreeBlock>>,
    count: usize,
}

impl ThreadFreelist {
    pub const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            count: 0,
        }
    }

    /// Push a freed block to the head (most-recently-freed end).
    #[inline]
    pub fn push(&mut self, block: NonNull<FreeBlock>) {
        unsafe { block.as_ref().write_next(self.head) };
        if self.head.is_none() {
            self.tail = Some(block);
        }
        self.head = Some(block);
        self.count += 1;
    }

    /// Pop one block from the head.
    #[inline]
    pub fn pop(&mut self) -> Option<NonNull<FreeBlock>> {
        let block = self.head?;
        self.head = unsafe { block.as_ref().read_next() };
        self.count -= 1;
        if self.head.is_none() {
            self.tail = None;
        }
        Some(block)
    }

    /// Push a chain of `count` blocks (from `first` through `last`) to the
    /// head of the freelist in O(1).  `last.next` must be `None`.
    #[inline]
    pub fn push_chain(
        &mut self,
        first: NonNull<FreeBlock>,
        last: NonNull<FreeBlock>,
        chain_count: usize,
    ) {
        unsafe { last.as_ref().write_next(self.head) };
        if self.head.is_none() {
            self.tail = Some(last);
        }
        self.head = Some(first);
        self.count += chain_count;
    }

    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    #[cfg(test)]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    /// Drain **all** items, returning the full chain.  Used on thread exit
    /// to return cached blocks to the per-node heap.
    pub fn drain_all(&mut self) -> Option<(NonNull<FreeBlock>, NonNull<FreeBlock>, usize)> {
        if self.count == 0 {
            return None;
        }
        let head = self.head.unwrap();
        let tail = self.tail.unwrap();
        let count = self.count;
        self.head = None;
        self.tail = None;
        self.count = 0;
        Some((head, tail, count))
    }

    /// Drain the **coldest** (tail-side) items, keeping `keep` hot items at
    /// the head.  Returns `(chain_head, chain_tail, drained_count)`.
    ///
    /// The returned chain can be pushed to a [`TreiberStack`] via
    /// [`TreiberStack::push_chain`].
    pub fn drain(
        &mut self,
        keep: usize,
    ) -> Option<(NonNull<FreeBlock>, NonNull<FreeBlock>, usize)> {
        if self.count <= keep {
            return None;
        }
        let drain_count = self.count - keep;

        // Walk `keep - 1` hops from `head` to reach the split point.
        let mut split = self.head.unwrap();
        for _ in 1..keep {
            split = unsafe { split.as_ref().read_next().unwrap() };
        }

        let chain_head = unsafe { split.as_ref().read_next().unwrap() };
        let chain_tail = self.tail.unwrap();

        unsafe { split.as_ref().write_next(None) };
        self.tail = Some(split);
        self.count = keep;

        Some((chain_head, chain_tail, drain_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allocate a `FreeBlock` on the heap (for testing only).
    fn alloc_block() -> NonNull<FreeBlock> {
        let block = Box::new(FreeBlock {
            next: UnsafeCell::new(None),
        });
        NonNull::from(Box::leak(block))
    }

    unsafe fn free_block(b: NonNull<FreeBlock>) {
        drop(unsafe { Box::from_raw(b.as_ptr()) });
    }

    // -- ThreadFreelist tests ------------------------------------------------

    #[test]
    fn thread_freelist_push_pop() {
        let mut fl = ThreadFreelist::new();
        assert!(fl.is_empty());

        let b1 = alloc_block();
        let b2 = alloc_block();
        fl.push(b1);
        fl.push(b2);
        assert_eq!(fl.count(), 2);

        let p1 = fl.pop().unwrap();
        assert_eq!(p1, b2); // LIFO
        let p2 = fl.pop().unwrap();
        assert_eq!(p2, b1);
        assert!(fl.is_empty());

        unsafe {
            free_block(b1);
            free_block(b2);
        }
    }

    #[test]
    fn thread_freelist_drain() {
        let mut fl = ThreadFreelist::new();
        let mut blocks = Vec::new();
        for _ in 0..10 {
            let b = alloc_block();
            fl.push(b);
            blocks.push(b);
        }
        assert_eq!(fl.count(), 10);

        let (chain_head, chain_tail, drained) = fl.drain(4).unwrap();
        assert_eq!(drained, 6);
        assert_eq!(fl.count(), 4);

        // Verify chain length.
        let mut n = chain_head;
        let mut chain_len = 1usize;
        while let Some(next) = unsafe { n.as_ref().read_next() } {
            n = next;
            chain_len += 1;
        }
        assert_eq!(chain_len, 6);
        assert_eq!(n, chain_tail);

        for b in blocks {
            unsafe { free_block(b) };
        }
    }

    // -- TreiberStack tests --------------------------------------------------

    #[test]
    fn treiber_push_pop() {
        let stack = TreiberStack::new();
        assert!(stack.is_empty());

        let b1 = alloc_block();
        let b2 = alloc_block();
        stack.push(b1);
        stack.push(b2);

        assert_eq!(stack.pop().unwrap(), b2);
        assert_eq!(stack.pop().unwrap(), b1);
        assert!(stack.pop().is_none());

        unsafe {
            free_block(b1);
            free_block(b2);
        }
    }

    #[test]
    fn treiber_push_chain() {
        let stack = TreiberStack::new();
        let b1 = alloc_block();
        let b2 = alloc_block();
        let b3 = alloc_block();

        // Build chain: b1 -> b2 -> b3
        unsafe {
            b1.as_ref().write_next(Some(b2));
            b2.as_ref().write_next(Some(b3));
            b3.as_ref().write_next(None);
        }

        stack.push_chain(b1, b3);

        assert_eq!(stack.pop().unwrap(), b1);
        assert_eq!(stack.pop().unwrap(), b2);
        assert_eq!(stack.pop().unwrap(), b3);
        assert!(stack.pop().is_none());

        unsafe {
            free_block(b1);
            free_block(b2);
            free_block(b3);
        }
    }

    #[test]
    fn treiber_concurrent_push_pop() {
        use std::sync::Arc;
        use std::thread;

        let stack = Arc::new(TreiberStack::new());
        let num_threads = 8;
        let ops_per_thread = 1000;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let s = Arc::clone(&stack);
                thread::spawn(move || {
                    for _ in 0..ops_per_thread {
                        let b = alloc_block();
                        s.push(b);
                    }
                    for _ in 0..ops_per_thread {
                        let p = s.pop();
                        assert!(p.is_some());
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Stack should be empty.
        let mut remaining = 0;
        while stack.pop().is_some() {
            remaining += 1;
        }
        assert_eq!(remaining, 0);
    }
}
