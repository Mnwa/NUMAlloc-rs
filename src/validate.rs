//! Debug-only metadata invariant checks.
//!
//! Nothing in this module is compiled into a normal build; it is enabled by
//! `cfg(test)` or the `internal-testing` cargo feature and is driven by
//! [`crate::NumaAlloc::validate_internal_state`].
//!
//! The allocator keeps no per-block or per-bag headers, so the checks are
//! derived purely from address arithmetic:
//!
//! * every free block lies inside the region of the node whose stack (or
//!   thread cache) holds it, below that region's bump pointer;
//! * every free block is aligned to its size class;
//! * a block appears at most once across all node stacks and the calling
//!   thread's caches, and blocks of different classes never overlap;
//! * every 32 KiB bag granule is claimed by at most one size class;
//! * freelist counts, tails and chain terminators agree;
//! * the large-object cache's slot and byte accounting is exact and its
//!   entries are disjoint, page aligned and outside the region.
//!
//! Bookkeeping memory comes straight from [`crate::platform::mmap_anonymous`]
//! so validation never re-enters the allocator being inspected.

use std::ptr::NonNull;

use crate::heap::GlobalHeap;
use crate::platform;
use crate::size_class::{self, BAG_SIZE};

/// Smallest block granule tracked by the occupancy bitmap.
const GRANULE: usize = size_class::SIZE_CLASSES[0];

/// Marker for a bag granule not yet claimed by any size class.
const UNCLAIMED: u8 = u8::MAX;

/// Occupancy bitmap plus per-bag class map for one heap.
pub struct Marks {
    buf: NonNull<u8>,
    buf_len: usize,
    /// Bytes of bitmap per node.
    bitmap_bytes: usize,
    /// Bag-class entries per node (one byte each), placed after all bitmaps.
    bag_entries: usize,
    num_nodes: usize,
}

impl Marks {
    pub fn new(heap: &GlobalHeap) -> Self {
        let region = heap.region_size();
        let num_nodes = heap.num_nodes();
        let bitmap_bytes = region.div_ceil(GRANULE * 8);
        let bag_entries = region.div_ceil(BAG_SIZE);
        let buf_len = (bitmap_bytes + bag_entries) * num_nodes;
        // SAFETY: buf_len is non-zero for any valid region.
        let buf = unsafe { platform::mmap_anonymous(buf_len) }.expect("validate: bookkeeping mmap");
        let marks = Self {
            buf,
            buf_len,
            bitmap_bytes,
            bag_entries,
            num_nodes,
        };
        // SAFETY: the buffer is exclusively owned and buf_len bytes long.
        unsafe {
            std::ptr::write_bytes(
                buf.as_ptr().add(bitmap_bytes * num_nodes),
                UNCLAIMED,
                bag_entries * num_nodes,
            );
        }
        marks
    }

    fn bitmap(&mut self, node: usize) -> &mut [u8] {
        // SAFETY: node < num_nodes and the slice lies within the buffer.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.buf.as_ptr().add(node * self.bitmap_bytes),
                self.bitmap_bytes,
            )
        }
    }

    fn bag_map(&mut self, node: usize) -> &mut [u8] {
        let start = self.bitmap_bytes * self.num_nodes + node * self.bag_entries;
        // SAFETY: the slice lies within the buffer (see `new`).
        unsafe { std::slice::from_raw_parts_mut(self.buf.as_ptr().add(start), self.bag_entries) }
    }

    /// Record a free block of `class` found in `node`'s structures, checking
    /// ownership, alignment, uniqueness and bag consistency.
    pub fn mark_block(
        &mut self,
        heap: &GlobalHeap,
        node: usize,
        class: usize,
        block: NonNull<u8>,
        origin: &str,
    ) {
        let region = heap.node_region(node);
        let obj = size_class::size_for_class(class);
        let bag = size_class::bag_size_for_class(class);
        let addr = block.as_ptr() as usize;
        let base = region.base().as_ptr() as usize;
        assert_eq!(
            heap.node_for_ptr(block),
            Some(node),
            "{origin}: block {addr:#x} (class {class}) is not inside node {node}'s region"
        );
        let off = addr - base;
        assert!(
            off + obj <= region.bump(),
            "{origin}: block {addr:#x} (class {class}) lies beyond node {node}'s bump pointer"
        );
        assert_eq!(
            addr % obj,
            0,
            "{origin}: block {addr:#x} is not aligned to its class size {obj}"
        );

        // Bag granule ownership: every 32 KiB granule covered by this block's
        // bag must be unclaimed or already claimed by the same class.
        let bag_base = (addr & !(bag - 1)) - base;
        let first_granule = bag_base / BAG_SIZE;
        let granules = bag / BAG_SIZE;
        let bag_map = self.bag_map(node);
        for (i, entry) in bag_map[first_granule..first_granule + granules]
            .iter_mut()
            .enumerate()
        {
            assert!(
                *entry == UNCLAIMED || *entry as usize == class,
                "{origin}: bag granule {} of node {node} is used by classes {} and {class}",
                first_granule + i,
                *entry
            );
            *entry = class as u8;
        }

        // Occupancy: no granule of this block may already be marked.
        let bitmap = self.bitmap(node);
        for g in (off / GRANULE)..((off + obj) / GRANULE) {
            let byte = &mut bitmap[g / 8];
            let bit = 1u8 << (g % 8);
            assert_eq!(
                *byte & bit,
                0,
                "{origin}: block {addr:#x} (class {class}) overlaps another free block (duplicate or double free)"
            );
            *byte |= bit;
        }
    }
}

impl Drop for Marks {
    fn drop(&mut self) {
        // SAFETY: buf/buf_len came from mmap_anonymous in `new`.
        unsafe { platform::munmap(self.buf, self.buf_len) };
    }
}
