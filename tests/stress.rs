//! Deterministic stress patterns that run without AFL and guard CI against
//! regressions in reuse, drain/refill, fragmentation and teardown paths.

mod common;

use common::boundaries::{CLASS_SIZES, SMALL_LIMIT};
use common::{FillMode, FreeOrder, Model};
use numalloc::NumaAlloc;

const N: usize = if cfg!(miri) { 120 } else { 6000 };

fn fresh() -> NumaAlloc {
    NumaAlloc::with_config(2, 4 << 20)
}

fn pattern(order: FreeOrder, size: usize, align: usize) {
    let alloc = fresh();
    let mut m = Model::new(&alloc, N, FillMode::Sparse);
    for _ in 0..2 {
        for i in 0..N {
            m.alloc(i, size, align, i % 3 == 0);
        }
        m.check_all();
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
        m.free_all(order);
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
    }
}

#[test]
fn fifo_free_order() {
    for &s in &[8usize, 64, 4096, 32768] {
        pattern(FreeOrder::Fifo, s, 8);
    }
}

#[test]
fn lifo_free_order() {
    for &s in &[8usize, 64, 4096, 32768] {
        pattern(FreeOrder::Lifo, s, 8);
    }
}

#[test]
fn free_every_second_then_refill_holes() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, N, FillMode::Sparse);
    for i in 0..N {
        m.alloc(i, 128, 8, false);
    }
    for i in (0..N).step_by(2) {
        m.free(i);
    }
    // Refill the holes with a different class and then the same class.
    for i in (0..N).step_by(2) {
        m.alloc(i, 24, 8, false);
    }
    m.check_all();
    for i in (0..N).step_by(2) {
        m.free(i);
        m.alloc(i, 128, 16, false);
    }
    m.check_all();
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
    m.free_all(FreeOrder::EverySecond);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn alternating_small_and_large() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, 64, FillMode::Sparse);
    let rounds = if cfg!(miri) { 4 } else { 64 };
    for r in 0..rounds {
        for i in 0..64 {
            let size = if i % 2 == 0 {
                48
            } else {
                SMALL_LIMIT + 1 + (r * 4096)
            };
            m.alloc(i, size, 8, false);
        }
        m.check_all();
        m.free_all(if r % 2 == 0 {
            FreeOrder::Fifo
        } else {
            FreeOrder::Lifo
        });
    }
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn alternating_alignment_requirements() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, 128, FillMode::Full);
    let aligns = [1usize, 4096, 8, 65536, 16, 262144, 64, 1 << 20];
    let rounds = if cfg!(miri) { 2 } else { 16 };
    for _ in 0..rounds {
        for i in 0..128 {
            let align = aligns[i % aligns.len()];
            if cfg!(miri) && align > 4096 {
                continue;
            }
            m.alloc(i, 40 + i, align, false);
        }
        m.check_all();
        m.free_all(FreeOrder::EverySecond);
    }
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn repeated_same_size_reuse_hits_freelist() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, 1, FillMode::Full);
    for &size in &CLASS_SIZES {
        if cfg!(miri) && size > 4096 {
            break;
        }
        let mut distinct = std::collections::BTreeSet::new();
        for _ in 0..(if cfg!(miri) { 50 } else { 2000 }) {
            m.alloc(0, size, 8, false);
            distinct.insert(m.ptr(0).unwrap() as usize);
            m.free(0);
        }
        // LIFO thread cache: the same block must come back every time.
        assert_eq!(distinct.len(), 1, "size {size} did not reuse its block");
    }
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn allocation_storm_then_full_teardown() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, N, FillMode::Sparse);
    let mut rng = common::Rng::new(0xC0FFEE);
    for i in 0..N {
        let size = *rng.pick(&CLASS_SIZES) + rng.below(3);
        let size = if cfg!(miri) { size.min(8192) } else { size };
        m.alloc(i, size, 8, false);
    }
    m.check_all();
    m.free_all(FreeOrder::EverySecond);
    assert_eq!(m.live_count(), 0);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
    // The cache must serve the next storm without new bags where possible.
    for i in 0..N {
        m.alloc(i, 64, 8, false);
    }
    m.free_all(FreeOrder::Lifo);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn fragmentation_followed_by_large_allocations() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, N + 64, FillMode::Sparse);
    for i in 0..N {
        m.alloc(i, 64, 8, false);
    }
    for i in (0..N).step_by(2) {
        m.free(i);
    }
    for i in 0..64 {
        let size = if i % 2 == 0 { 4096 } else { 1 << 20 };
        if cfg!(miri) && size > 65536 {
            continue;
        }
        m.alloc(N + i, size, 4096, false);
    }
    m.check_all();
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
    m.free_all(FreeOrder::Fifo);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn repeated_allocator_initialisation_and_destruction() {
    // Each instance reserves its own region; dropping it while the thread
    // cache still references the heap must keep the region alive until the
    // cache is replaced, and the next instance must start empty.
    for round in 0..(if cfg!(miri) { 3 } else { 40 }) {
        let alloc = NumaAlloc::with_config(1 + round % 3, 1 << 20);
        let mut m = Model::new(&alloc, 32, FillMode::Sparse);
        for i in 0..32 {
            m.alloc(i, CLASS_SIZES[(round + i) % CLASS_SIZES.len()], 8, false);
        }
        m.check_all();
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
        drop(m);
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
    }
}

#[test]
fn region_exhaustion_falls_back_and_recovers() {
    // A 256 KiB region holds exactly one 256 KiB bag (or eight 32 KiB ones).
    let alloc = NumaAlloc::with_config(1, SMALL_LIMIT);
    let mut m = Model::new(&alloc, 64, FillMode::Sparse);
    for i in 0..64 {
        assert!(m.alloc(i, 32768, 8, false));
    }
    let in_region = (0..64).filter(|&i| alloc.owns(m.ptr(i).unwrap())).count();
    // The region base is bag aligned, so exactly eight 32 KiB bags fit; the
    // rest must be served by dedicated mappings rather than fail.
    assert_eq!(in_region, 8, "in_region = {in_region}");
    m.check_all();
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
    m.free_all(FreeOrder::Lifo);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
    // Region blocks are recycled before any new mapping is created.
    for i in 0..in_region {
        assert!(m.alloc(i, 32768, 8, false));
        assert!(alloc.owns(m.ptr(i).unwrap()), "block {i} not recycled");
    }
    m.free_all(FreeOrder::Fifo);
    // Mixed classes on an exhausted region.
    for i in 0..64 {
        m.alloc(i, CLASS_SIZES[i % CLASS_SIZES.len()], 8, false);
    }
    m.check_all();
    m.free_all(FreeOrder::EverySecond);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn thread_cache_drain_and_refill_cycles() {
    // Push far past the per-class cache limit so drain (dealloc) and refill
    // (alloc) paths run repeatedly on one thread.
    let alloc = fresh();
    let mut m = Model::new(&alloc, N, FillMode::Sparse);
    for round in 0..(if cfg!(miri) { 2 } else { 6 }) {
        for i in 0..N {
            m.alloc(i, 8 << (round % 4), 8, false);
        }
        m.free_all(if round % 2 == 0 {
            FreeOrder::Fifo
        } else {
            FreeOrder::Lifo
        });
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
    }
}
