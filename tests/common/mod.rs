//! Shared allocator state-machine model used by the deterministic integration
//! tests and (via `#[path]` inclusion) by the AFL harnesses in `fuzz/afl`.
//!
//! The model keeps a table of *slots*.  Each slot either is empty or owns one
//! live allocation together with its requested size, alignment and a
//! generation counter.  Every byte written into a live allocation is derived
//! from `(slot, generation, offset)`, so the model can detect any corruption
//! caused by unrelated allocator operations.  Invalid API usage (double free,
//! freeing an empty slot, zero-sized layouts) is rejected at the model level
//! and never reaches the allocator.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout};
use std::ptr::NonNull;

// ---------------------------------------------------------------------------
// Allocator-specific hooks
// ---------------------------------------------------------------------------

/// Extra, optional introspection the model uses when available.
pub trait Harness: GlobalAlloc {
    /// Check internal invariants; must only be called while quiescent.
    ///
    /// # Safety
    /// See `NumaAlloc::validate_internal_state`.
    unsafe fn validate(&self) {}
    /// Whether `ptr` came from the pre-mapped region (`None` = unknown).
    fn owns(&self, _ptr: *mut u8) -> Option<bool> {
        None
    }
}

impl Harness for std::alloc::System {}

impl Harness for numalloc::NumaAlloc {
    unsafe fn validate(&self) {
        // SAFETY: forwarded contract.
        unsafe { self.validate_internal_state() }
    }
    fn owns(&self, ptr: *mut u8) -> Option<bool> {
        Some(numalloc::NumaAlloc::owns(self, ptr))
    }
}

// ---------------------------------------------------------------------------
// Boundary generators
// ---------------------------------------------------------------------------

pub mod boundaries {
    /// The 16 power-of-two size classes of NUMAlloc.
    pub const CLASS_SIZES: [usize; 16] = [
        8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144,
    ];
    /// Largest request served from bags; anything above is mmap-backed.
    pub const SMALL_LIMIT: usize = 262144;
    /// Bag size for classes up to 16 KiB.
    pub const BAG_SIZE: usize = 32768;
    pub const PAGE: usize = 4096;
    /// Large-cache "close size" tolerance (extra bytes accepted on reuse).
    pub const CLOSE_SIZE_TOLERANCE: usize = 8192;
    /// Threshold above which cached large mappings are `madvise`d.
    pub const MADVISE_THRESHOLD: usize = 512 * 1024;

    /// Generic allocator-relevant sizes (the task's baseline list).
    pub const GENERIC_SIZES: &[usize] = &[
        0, 1, 2, 3, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128, 255, 256, 511, 512, 1023, 1024, 4095,
        4096, 4097,
    ];

    /// Alignments from the minimum through large powers of two.  4 MiB is
    /// the largest alignment the large path is exercised with; it forces a
    /// mapping with `align - 1` bytes of slack.
    pub const ALIGNS: &[usize] = &[
        1,
        2,
        4,
        8,
        16,
        32,
        64,
        128,
        256,
        4096,
        8192,
        65536,
        262144,
        524288,
        1 << 20,
        4 << 20,
    ];

    /// `B - 1, B, B + 1` around every implementation boundary, deduplicated
    /// and sorted; zero is included.
    pub fn sizes() -> Vec<usize> {
        let mut v: Vec<usize> = GENERIC_SIZES.to_vec();
        let mut around = |b: usize| {
            if b > 0 {
                v.push(b - 1);
            }
            v.push(b);
            v.push(b + 1);
        };
        for &c in &CLASS_SIZES {
            around(c);
        }
        around(BAG_SIZE);
        around(2 * BAG_SIZE);
        for k in 1..=4 {
            around(k * PAGE);
        }
        around(SMALL_LIMIT);
        around(SMALL_LIMIT + PAGE);
        around(MADVISE_THRESHOLD);
        around(MADVISE_THRESHOLD + CLOSE_SIZE_TOLERANCE);
        around(1 << 20);
        around(4 << 20);
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Sizes without zero (for paths where zero-sized layouts are invalid).
    pub fn nonzero_sizes() -> Vec<usize> {
        sizes().into_iter().filter(|&s| s > 0).collect()
    }

    /// 256-entry lookup table used by the fuzz decoder so that a single
    /// input byte lands on a boundary value most of the time.
    pub fn fuzz_size_table() -> [usize; 256] {
        let base = nonzero_sizes();
        let mut t = [0usize; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            *slot = base[i % base.len()];
        }
        t
    }
}

// ---------------------------------------------------------------------------
// Byte patterns
// ---------------------------------------------------------------------------

/// Deterministic byte for `(slot, generation, offset)`.
#[inline]
pub fn pattern_byte(slot: usize, generation: u32, offset: usize) -> u8 {
    let x = (slot as u32)
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add(generation.wrapping_mul(0x85EB_CA6B))
        ^ (offset as u32).wrapping_mul(0xC2B2_AE35);
    let x = x ^ (x >> 15);
    (x.wrapping_mul(0x2C1B_3C6D) >> 24) as u8
}

/// How much of each allocation is written and verified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FillMode {
    /// Every byte.  Best oracle, O(size) per operation.
    Full,
    /// Head, tail and one byte per page.  Keeps fuzz iterations fast for
    /// multi-megabyte objects while still catching overlap/corruption.
    Sparse,
}

/// Offsets that are written/verified for an object of `size` bytes.
pub fn probe_offsets(size: usize, mode: FillMode) -> Vec<usize> {
    match mode {
        FillMode::Full => (0..size).collect(),
        FillMode::Sparse => {
            let mut v = Vec::new();
            let head = size.min(64);
            v.extend(0..head);
            v.extend(size.saturating_sub(64).max(head)..size);
            v.extend((head..size.saturating_sub(64)).step_by(boundaries::PAGE));
            v
        }
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Order used by bulk-free operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeOrder {
    Fifo,
    Lifo,
    EverySecond,
}

/// One allocator-level operation.  All slot indices are taken modulo the
/// model's slot count; operations that are invalid for the current state
/// (free of an empty slot, realloc of an empty slot, ...) are no-ops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Alloc {
        slot: usize,
        size: usize,
        align: usize,
        zeroed: bool,
    },
    Free {
        slot: usize,
    },
    Realloc {
        slot: usize,
        new_size: usize,
    },
    /// Free `slot` (if live) and allocate into it with the new parameters.
    Replace {
        slot: usize,
        size: usize,
        align: usize,
    },
    /// Allocate `size`/`align` into every empty slot.
    FillEmpty {
        size: usize,
        align: usize,
    },
    /// Free live slots in the given order.
    FreeAll {
        order: FreeOrder,
    },
    /// `cycles` × (alloc, write, check, free) of the same layout in `slot`.
    Churn {
        slot: usize,
        size: usize,
        align: usize,
        cycles: usize,
    },
    /// Rewrite `slot` with a fresh generation pattern.
    Touch {
        slot: usize,
    },
    /// Verify the contents of every live slot.
    CheckAll,
    /// Run the allocator's internal invariant checker.
    Validate,
    /// A request that must fail (or, if it succeeds, be freed untouched):
    /// sizes at or beyond the address-space limit.
    Huge {
        size: usize,
        align: usize,
    },
    /// Hand `slot` to another thread (`to` is taken modulo the thread
    /// count).  Single-threaded runners treat it as [`Op::Touch`].
    Transfer {
        slot: usize,
        to: usize,
    },
}

// ---------------------------------------------------------------------------
// Input decoder (fuzz bytes -> ops)
// ---------------------------------------------------------------------------

/// Hard cap on a normal request; larger values are clamped.  Keeps mmap
/// fallbacks and page-touching bounded per iteration.
pub const MAX_OP_SIZE: usize = 8 << 20;
/// Largest alignment used by the decoder (2^22 = 4 MiB).
pub const MAX_ALIGN_SHIFT: u32 = 22;
/// Maximum number of ops decoded from one input.
pub const MAX_OPS: usize = 4096;
/// Maximum churn cycles per `Churn` op.
pub const MAX_CHURN: usize = 64;

/// Byte-stream cursor with boundary-biased value decoding.
pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    size_table: [usize; 256],
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            size_table: boundaries::fuzz_size_table(),
        }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn byte(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    /// Two-byte size: the high two bits of the first byte select the mode.
    ///   0: exact boundary table lookup
    ///   1: boundary table lookup with a small signed jitter (-32..=31)
    ///   2: small product (1..=16384)
    ///   3: power of two up to 4 MiB, optionally +/-1
    pub fn size(&mut self) -> Option<usize> {
        let a = self.byte()?;
        let b = self.byte()?;
        let v = match a >> 6 {
            0 => self.size_table[b as usize],
            1 => {
                let jitter = (a & 0x3f) as isize - 32;
                (self.size_table[b as usize] as isize + jitter).max(1) as usize
            }
            2 => (b as usize + 1) * ((a & 0x3f) as usize + 1),
            _ => {
                let p = 1usize << (b % 23);
                match a & 3 {
                    0 => p,
                    1 => p + 1,
                    2 => p.saturating_sub(1).max(1),
                    _ => p + p / 2,
                }
            }
        };
        Some(v.clamp(1, MAX_OP_SIZE))
    }

    /// One-byte alignment: values below 128 give 1..=128, the rest span the
    /// full range up to 4 MiB.
    pub fn align(&mut self) -> Option<usize> {
        let b = self.byte()?;
        Some(if b < 128 {
            1 << (b % 8)
        } else {
            1 << ((b - 128) as u32 % (MAX_ALIGN_SHIFT + 1))
        })
    }

    pub fn slot(&mut self, nslots: usize) -> Option<usize> {
        Some(self.byte()? as usize % nslots)
    }

    /// Decode one operation, or `None` when the input is exhausted.
    pub fn op(&mut self, nslots: usize) -> Option<Op> {
        let kind = self.byte()?;
        Some(match kind % 21 {
            0..=4 => Op::Alloc {
                slot: self.slot(nslots)?,
                size: self.size()?,
                align: self.align()?,
                zeroed: false,
            },
            5 => Op::Alloc {
                slot: self.slot(nslots)?,
                size: self.size()?,
                align: self.align()?,
                zeroed: true,
            },
            6..=8 => Op::Free {
                slot: self.slot(nslots)?,
            },
            9 | 10 => Op::Realloc {
                slot: self.slot(nslots)?,
                new_size: self.size()?,
            },
            11 => Op::Replace {
                slot: self.slot(nslots)?,
                size: self.size()?,
                align: self.align()?,
            },
            12 => Op::FillEmpty {
                size: self.size()?,
                align: self.align()?,
            },
            13 => Op::FreeAll {
                order: match self.byte()? % 3 {
                    0 => FreeOrder::Fifo,
                    1 => FreeOrder::Lifo,
                    _ => FreeOrder::EverySecond,
                },
            },
            14 => Op::Churn {
                slot: self.slot(nslots)?,
                size: self.size()?,
                align: self.align()?,
                cycles: (self.byte()? as usize % MAX_CHURN) + 1,
            },
            15 => Op::Touch {
                slot: self.slot(nslots)?,
            },
            16 => Op::CheckAll,
            17 => Op::Validate,
            18 => Op::Huge {
                size: match self.byte()? % 4 {
                    0 => isize::MAX as usize,
                    1 => isize::MAX as usize - boundaries::PAGE,
                    2 => 1 << 50,
                    _ => usize::MAX - boundaries::PAGE,
                },
                align: self.align()?,
            },
            20 => Op::Transfer {
                slot: self.slot(nslots)?,
                to: self.byte()? as usize,
            },
            _ => {
                // Realloc to the same size class edge: size +/- 1.
                Op::Realloc {
                    slot: self.slot(nslots)?,
                    new_size: {
                        let s = self.size()?;
                        if kind & 0x20 != 0 {
                            s + 1
                        } else {
                            s.max(2) - 1
                        }
                    },
                }
            }
        })
    }

    pub fn ops(&mut self, nslots: usize) -> Vec<Op> {
        let mut v = Vec::new();
        while v.len() < MAX_OPS {
            match self.op(nslots) {
                Some(op) => v.push(op),
                None => break,
            }
        }
        v
    }
}

/// Encode ops back into the byte format understood by [`Decoder`] (used to
/// write seed corpora).  Sizes are emitted in "exact table" or "small
/// product" form when representable, otherwise in power-of-two form; any
/// value not representable exactly is emitted as the nearest table entry.
pub fn encode(ops: &[Op], nslots: usize) -> Vec<u8> {
    let table = boundaries::fuzz_size_table();
    let mut out = Vec::new();
    fn push_size(out: &mut Vec<u8>, table: &[usize; 256], size: usize) {
        if let Some(i) = table.iter().position(|&s| s == size) {
            out.push(0);
            out.push(i as u8);
            return;
        }
        for a in 1..=64usize {
            if size.is_multiple_of(a) && (1..=256).contains(&(size / a)) {
                out.push(0x80 | (a as u8 - 1));
                out.push((size / a - 1) as u8);
                return;
            }
        }
        if size.is_power_of_two() && size <= 1 << 22 {
            out.push(0xC0);
            out.push(size.trailing_zeros() as u8);
            return;
        }
        for (i, &t) in table.iter().enumerate() {
            let jitter = size as isize - t as isize;
            if (-32..=31).contains(&jitter) {
                out.push(0x40 | ((jitter + 32) as u8));
                out.push(i as u8);
                return;
            }
        }
        // Fallback: nearest table entry.
        let i = table
            .iter()
            .enumerate()
            .min_by_key(|&(_, &t)| t.abs_diff(size))
            .map(|(i, _)| i)
            .unwrap();
        out.push(0);
        out.push(i as u8);
    }
    fn push_align(out: &mut Vec<u8>, align: usize) {
        let shift = align.trailing_zeros();
        if shift < 8 {
            out.push(shift as u8);
        } else {
            out.push(128 + shift as u8);
        }
    }
    let slot = |s: usize| (s % nslots) as u8;
    for op in ops {
        match *op {
            Op::Alloc {
                slot: s,
                size,
                align,
                zeroed,
            } => {
                out.push(if zeroed { 5 } else { 0 });
                out.push(slot(s));
                push_size(&mut out, &table, size);
                push_align(&mut out, align);
            }
            Op::Free { slot: s } => {
                out.push(6);
                out.push(slot(s));
            }
            Op::Transfer { slot: s, to } => {
                out.push(20);
                out.push(slot(s));
                out.push(to as u8);
            }
            Op::Realloc { slot: s, new_size } => {
                out.push(9);
                out.push(slot(s));
                push_size(&mut out, &table, new_size);
            }
            Op::Replace {
                slot: s,
                size,
                align,
            } => {
                out.push(11);
                out.push(slot(s));
                push_size(&mut out, &table, size);
                push_align(&mut out, align);
            }
            Op::FillEmpty { size, align } => {
                out.push(12);
                push_size(&mut out, &table, size);
                push_align(&mut out, align);
            }
            Op::FreeAll { order } => {
                out.push(13);
                out.push(match order {
                    FreeOrder::Fifo => 0,
                    FreeOrder::Lifo => 1,
                    FreeOrder::EverySecond => 2,
                });
            }
            Op::Churn {
                slot: s,
                size,
                align,
                cycles,
            } => {
                out.push(14);
                out.push(slot(s));
                push_size(&mut out, &table, size);
                push_align(&mut out, align);
                out.push((cycles.clamp(1, MAX_CHURN) - 1) as u8);
            }
            Op::Touch { slot: s } => {
                out.push(15);
                out.push(slot(s));
            }
            Op::CheckAll => out.push(16),
            Op::Validate => out.push(17),
            Op::Huge { size, align } => {
                out.push(18);
                out.push(if size == isize::MAX as usize {
                    0
                } else if size == isize::MAX as usize - boundaries::PAGE {
                    1
                } else if size == 1 << 50 {
                    2
                } else {
                    3
                });
                push_align(&mut out, align);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// A live allocation detached from a model (e.g. for cross-thread transfer).
#[derive(Debug)]
pub struct Owned {
    ptr: NonNull<u8>,
    size: usize,
    align: usize,
    generation: u32,
    /// Slot id the pattern was derived from (kept so the receiver can verify
    /// the bytes exactly as the sender wrote them).
    pattern_slot: usize,
}

// SAFETY: the allocation is exclusively owned by whoever holds the `Owned`;
// `GlobalAlloc` permits deallocating from any thread.
unsafe impl Send for Owned {}

impl Owned {
    pub fn size(&self) -> usize {
        self.size
    }
    pub fn align(&self) -> usize {
        self.align
    }
    pub fn addr(&self) -> usize {
        self.ptr.as_ptr() as usize
    }
    /// Give up model tracking: `(address, size, align)`.  The caller becomes
    /// responsible for deallocating with exactly this layout.
    pub fn into_raw(self) -> (usize, usize, usize) {
        (self.addr(), self.size, self.align)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub allocs: usize,
    pub frees: usize,
    pub reallocs: usize,
    pub in_region: usize,
    pub mapped: usize,
    pub failed_huge: usize,
}

struct Slot {
    ptr: Option<NonNull<u8>>,
    size: usize,
    align: usize,
    generation: u32,
    pattern_slot: usize,
}

pub struct Model<'a, A: Harness> {
    alloc: &'a A,
    slots: Vec<Slot>,
    next_generation: u32,
    mode: FillMode,
    /// `Some` = requests up to this size must succeed.
    must_succeed_below: Option<usize>,
    pub stats: Stats,
}

impl<'a, A: Harness> Model<'a, A> {
    pub fn new(alloc: &'a A, nslots: usize, mode: FillMode) -> Self {
        assert!(nslots > 0);
        Self {
            alloc,
            slots: (0..nslots)
                .map(|i| Slot {
                    ptr: None,
                    size: 0,
                    align: 1,
                    generation: 0,
                    pattern_slot: i,
                })
                .collect(),
            next_generation: 1,
            mode,
            must_succeed_below: Some(MAX_OP_SIZE),
            stats: Stats::default(),
        }
    }

    /// Requests below `limit` must succeed; larger requests may return null.
    pub fn set_must_succeed_below(&mut self, limit: Option<usize>) {
        self.must_succeed_below = limit;
    }

    pub fn allocator(&self) -> &A {
        self.alloc
    }

    pub fn nslots(&self) -> usize {
        self.slots.len()
    }

    pub fn is_live(&self, slot: usize) -> bool {
        self.slots[slot % self.slots.len()].ptr.is_some()
    }

    pub fn live_count(&self) -> usize {
        self.slots.iter().filter(|s| s.ptr.is_some()).count()
    }

    pub fn ptr(&self, slot: usize) -> Option<*mut u8> {
        self.slots[slot % self.slots.len()].ptr.map(NonNull::as_ptr)
    }

    fn bump_generation(&mut self) -> u32 {
        let g = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        g
    }

    /// Verify that `[ptr, ptr+size)` overlaps no live slot.
    fn assert_disjoint(&self, ptr: NonNull<u8>, size: usize, except: usize) {
        let a0 = ptr.as_ptr() as usize;
        let a1 = a0 + size.max(1);
        for (i, s) in self.slots.iter().enumerate() {
            if i == except {
                continue;
            }
            let Some(p) = s.ptr else { continue };
            let b0 = p.as_ptr() as usize;
            let b1 = b0 + s.size.max(1);
            assert!(
                a1 <= b0 || b1 <= a0,
                "allocation {a0:#x}+{size} overlaps live slot {i} at {b0:#x}+{}",
                s.size
            );
        }
    }

    fn write_pattern(ptr: NonNull<u8>, size: usize, pslot: usize, generation: u32, mode: FillMode) {
        for off in probe_offsets(size, mode) {
            // SAFETY: off < size and the allocation is live and exclusively owned.
            unsafe {
                ptr.as_ptr()
                    .add(off)
                    .write(pattern_byte(pslot, generation, off))
            };
        }
    }

    fn check_pattern(
        ptr: NonNull<u8>,
        size: usize,
        pslot: usize,
        generation: u32,
        mode: FillMode,
        what: &str,
        limit: usize,
    ) {
        for off in probe_offsets(size, mode) {
            if off >= limit {
                continue;
            }
            // SAFETY: off < size and the allocation is live.
            let got = unsafe { ptr.as_ptr().add(off).read() };
            let want = pattern_byte(pslot, generation, off);
            assert_eq!(
                got,
                want,
                "{what}: byte {off} of {:#x} (size {size}, gen {generation}) corrupted",
                ptr.as_ptr() as usize
            );
        }
    }

    /// Allocate into `slot` (freeing any previous occupant).  Returns whether
    /// the allocation succeeded.
    pub fn alloc(&mut self, slot: usize, size: usize, align: usize, zeroed: bool) -> bool {
        let slot = slot % self.slots.len();
        if self.slots[slot].ptr.is_some() {
            self.free(slot);
        }
        if size == 0 {
            // Zero-sized layouts are a caller contract violation for
            // GlobalAlloc; the model never issues them.
            return false;
        }
        let Ok(layout) = Layout::from_size_align(size, align) else {
            return false;
        };
        // SAFETY: valid non-zero layout.
        let raw = unsafe {
            if zeroed {
                self.alloc.alloc_zeroed(layout)
            } else {
                self.alloc.alloc(layout)
            }
        };
        let Some(ptr) = NonNull::new(raw) else {
            if let Some(limit) = self.must_succeed_below
                && size <= limit
            {
                panic!("allocation of {size} bytes / align {align} unexpectedly failed");
            }
            return false;
        };
        assert_eq!(
            ptr.as_ptr() as usize % align,
            0,
            "pointer {:#x} violates alignment {align} (size {size})",
            ptr.as_ptr() as usize
        );
        self.assert_disjoint(ptr, size, slot);
        if zeroed {
            for off in probe_offsets(size, self.mode) {
                // SAFETY: off < size.
                let b = unsafe { ptr.as_ptr().add(off).read() };
                assert_eq!(b, 0, "alloc_zeroed returned non-zero byte at offset {off}");
            }
        }
        let generation = self.bump_generation();
        let pslot = self.slots[slot].pattern_slot;
        Self::write_pattern(ptr, size, pslot, generation, self.mode);
        match self.alloc.owns(ptr.as_ptr()) {
            Some(true) => self.stats.in_region += 1,
            Some(false) => self.stats.mapped += 1,
            None => {}
        }
        self.stats.allocs += 1;
        self.slots[slot] = Slot {
            ptr: Some(ptr),
            size,
            align,
            generation,
            pattern_slot: pslot,
        };
        true
    }

    /// Verify and free `slot`; a no-op for empty slots.
    pub fn free(&mut self, slot: usize) {
        let slot = slot % self.slots.len();
        let Some(ptr) = self.slots[slot].ptr else {
            return;
        };
        self.check_slot(slot);
        let s = &mut self.slots[slot];
        let layout = Layout::from_size_align(s.size, s.align).unwrap();
        // SAFETY: ptr/layout are exactly what was allocated.
        unsafe { self.alloc.dealloc(ptr.as_ptr(), layout) };
        s.ptr = None;
        self.stats.frees += 1;
    }

    /// Realloc `slot` to `new_size`, verifying the preserved prefix.
    pub fn realloc(&mut self, slot: usize, new_size: usize) -> bool {
        let slot = slot % self.slots.len();
        let Some(ptr) = self.slots[slot].ptr else {
            return false;
        };
        if new_size == 0 {
            return false;
        }
        let (old_size, align, old_gen, pslot) = {
            let s = &self.slots[slot];
            (s.size, s.align, s.generation, s.pattern_slot)
        };
        if Layout::from_size_align(new_size, align).is_err() {
            return false;
        }
        self.check_slot(slot);
        let old_layout = Layout::from_size_align(old_size, align).unwrap();
        // SAFETY: ptr/old_layout match the live allocation; new_size is valid.
        let raw = unsafe { self.alloc.realloc(ptr.as_ptr(), old_layout, new_size) };
        let Some(new_ptr) = NonNull::new(raw) else {
            // Failure leaves the old allocation intact.
            if let Some(limit) = self.must_succeed_below
                && new_size <= limit
            {
                panic!("realloc {old_size} -> {new_size} (align {align}) unexpectedly failed");
            }
            self.check_slot(slot);
            return false;
        };
        assert_eq!(
            new_ptr.as_ptr() as usize % align,
            0,
            "realloc result {:#x} violates alignment {align}",
            new_ptr.as_ptr() as usize
        );
        self.slots[slot].ptr = None; // exclude the old range from the overlap scan
        self.assert_disjoint(new_ptr, new_size, slot);
        let copy = old_size.min(new_size);
        Self::check_pattern(
            new_ptr,
            old_size,
            pslot,
            old_gen,
            self.mode,
            "realloc prefix",
            copy,
        );
        let generation = self.bump_generation();
        Self::write_pattern(new_ptr, new_size, pslot, generation, self.mode);
        self.stats.reallocs += 1;
        self.slots[slot] = Slot {
            ptr: Some(new_ptr),
            size: new_size,
            align,
            generation,
            pattern_slot: pslot,
        };
        true
    }

    /// Rewrite `slot` with a new generation pattern.
    pub fn touch(&mut self, slot: usize) {
        let slot = slot % self.slots.len();
        let Some(ptr) = self.slots[slot].ptr else {
            return;
        };
        self.check_slot(slot);
        let generation = self.bump_generation();
        let s = &mut self.slots[slot];
        s.generation = generation;
        Self::write_pattern(ptr, s.size, s.pattern_slot, generation, self.mode);
    }

    pub fn check_slot(&self, slot: usize) {
        let s = &self.slots[slot % self.slots.len()];
        if let Some(ptr) = s.ptr {
            Self::check_pattern(
                ptr,
                s.size,
                s.pattern_slot,
                s.generation,
                self.mode,
                &format!("slot {slot}"),
                usize::MAX,
            );
        }
    }

    pub fn check_all(&self) {
        for i in 0..self.slots.len() {
            self.check_slot(i);
        }
    }

    pub fn free_all(&mut self, order: FreeOrder) {
        let live: Vec<usize> = (0..self.slots.len())
            .filter(|&i| self.slots[i].ptr.is_some())
            .collect();
        match order {
            FreeOrder::Fifo => live.iter().for_each(|&i| self.free(i)),
            FreeOrder::Lifo => live.iter().rev().for_each(|&i| self.free(i)),
            FreeOrder::EverySecond => {
                live.iter().step_by(2).for_each(|&i| self.free(i));
                live.iter().skip(1).step_by(2).for_each(|&i| self.free(i));
            }
        }
    }

    /// Issue a request that is expected to fail; if the allocator does hand
    /// out memory it is released immediately without being touched.
    pub fn huge(&mut self, size: usize, align: usize) {
        // Miri treats an unsatisfiable allocation as a hard "resource
        // exhaustion" error rather than returning null, so the address-space
        // limit cannot be probed under the interpreter.
        if cfg!(miri) {
            return;
        }
        let Ok(layout) = Layout::from_size_align(size, align) else {
            return;
        };
        // SAFETY: valid layout.
        let raw = unsafe { self.alloc.alloc(layout) };
        if raw.is_null() {
            self.stats.failed_huge += 1;
        } else {
            // SAFETY: allocated just above with this layout.
            unsafe { self.alloc.dealloc(raw, layout) };
        }
    }

    /// # Safety
    /// The allocator must be quiescent (no other threads using it).
    pub unsafe fn validate(&self) {
        // SAFETY: forwarded contract.
        unsafe { self.alloc.validate() };
    }

    /// Detach the allocation in `slot` (verified first) for transfer.
    pub fn take(&mut self, slot: usize) -> Option<Owned> {
        let slot = slot % self.slots.len();
        let ptr = self.slots[slot].ptr?;
        self.check_slot(slot);
        let s = &mut self.slots[slot];
        s.ptr = None;
        Some(Owned {
            ptr,
            size: s.size,
            align: s.align,
            generation: s.generation,
            pattern_slot: s.pattern_slot,
        })
    }

    /// Adopt a transferred allocation into `slot` (freeing any occupant),
    /// verifying its contents as written by the sender.  The bytes are then
    /// rewritten with this model's own pattern.
    pub fn adopt(&mut self, slot: usize, owned: Owned) {
        let slot = slot % self.slots.len();
        if self.slots[slot].ptr.is_some() {
            self.free(slot);
        }
        Self::check_pattern(
            owned.ptr,
            owned.size,
            owned.pattern_slot,
            owned.generation,
            self.mode,
            "adopted allocation",
            usize::MAX,
        );
        self.assert_disjoint(owned.ptr, owned.size, slot);
        let generation = self.bump_generation();
        let pslot = self.slots[slot].pattern_slot;
        Self::write_pattern(owned.ptr, owned.size, pslot, generation, self.mode);
        self.slots[slot] = Slot {
            ptr: Some(owned.ptr),
            size: owned.size,
            align: owned.align,
            generation,
            pattern_slot: pslot,
        };
    }

    /// Free a transferred allocation directly (verifying it first).
    pub fn release(&mut self, owned: Owned) {
        Self::check_pattern(
            owned.ptr,
            owned.size,
            owned.pattern_slot,
            owned.generation,
            self.mode,
            "released allocation",
            usize::MAX,
        );
        let layout = Layout::from_size_align(owned.size, owned.align).unwrap();
        // SAFETY: the allocation is live and exclusively owned.
        unsafe { self.alloc.dealloc(owned.ptr.as_ptr(), layout) };
        self.stats.frees += 1;
    }

    /// Apply one decoded operation.
    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Alloc {
                slot,
                size,
                align,
                zeroed,
            } => {
                self.alloc(slot, size, align, zeroed);
            }
            Op::Free { slot } => self.free(slot),
            Op::Realloc { slot, new_size } => {
                self.realloc(slot, new_size);
            }
            Op::Replace { slot, size, align } => {
                self.free(slot);
                self.alloc(slot, size, align, false);
            }
            Op::FillEmpty { size, align } => {
                for i in 0..self.slots.len() {
                    if self.slots[i].ptr.is_none() {
                        self.alloc(i, size, align, false);
                    }
                }
            }
            Op::FreeAll { order } => self.free_all(order),
            Op::Churn {
                slot,
                size,
                align,
                cycles,
            } => {
                for _ in 0..cycles.min(MAX_CHURN) {
                    self.alloc(slot, size, align, false);
                    self.free(slot);
                }
            }
            Op::Touch { slot } => self.touch(slot),
            Op::CheckAll => self.check_all(),
            // SAFETY: the model is single-threaded; callers that share the
            // allocator across threads must strip `Validate` ops themselves.
            Op::Validate => unsafe { self.validate() },
            Op::Huge { size, align } => self.huge(size, align),
            Op::Transfer { slot, .. } => self.touch(slot),
        }
    }

    pub fn run(&mut self, ops: &[Op]) {
        for &op in ops {
            self.apply(op);
        }
    }
}

impl<A: Harness> Drop for Model<'_, A> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        for i in 0..self.slots.len() {
            if self.slots[i].ptr.is_some() {
                self.free(i);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tiny deterministic PRNG for the non-AFL tests
// ---------------------------------------------------------------------------

/// xorshift64* — deterministic, dependency-free.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
    /// Feed random bytes through the boundary-biased [`Decoder`].
    pub fn ops(&mut self, nslots: usize, nbytes: usize) -> Vec<Op> {
        let bytes: Vec<u8> = (0..nbytes).map(|_| self.next_u64() as u8).collect();
        Decoder::new(&bytes).ops(nslots)
    }
}
