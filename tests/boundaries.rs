//! Boundary-value tests: every size-class / bag / page / large-object edge,
//! all supported alignments, and `usize` extremes at the public API.

mod common;

use common::boundaries::{ALIGNS, CLASS_SIZES, PAGE, SMALL_LIMIT};
use common::{FillMode, Model};
use numalloc::NumaAlloc;
use std::alloc::{GlobalAlloc, Layout};

/// Under Miri keep the matrix small: it is ~1000x slower than native.
const MIRI: bool = cfg!(miri);

fn fresh() -> NumaAlloc {
    NumaAlloc::with_config(2, 4 << 20)
}

#[test]
fn every_size_with_every_alignment() {
    let alloc = fresh();
    let sizes = common::boundaries::nonzero_sizes();
    let mut small = Model::new(&alloc, 8, FillMode::Full);
    let mut big = Model::new(&alloc, 8, FillMode::Sparse);
    for &size in &sizes {
        if MIRI && size > 4096 {
            continue;
        }
        for &align in ALIGNS {
            if MIRI && align > 64 {
                continue;
            }
            if Layout::from_size_align(size, align).is_err() {
                continue;
            }
            let m = if size <= 65536 { &mut small } else { &mut big };
            assert!(m.alloc(0, size, align, false), "size {size} align {align}");
            assert!(
                m.alloc(1, size, align, true),
                "zeroed size {size} align {align}"
            );
            m.check_all();
            m.free(0);
            m.free(1);
        }
    }
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn class_edges_live_simultaneously() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, 3 * CLASS_SIZES.len(), FillMode::Sparse);
    for (i, &b) in CLASS_SIZES.iter().enumerate() {
        if MIRI && b > 65536 {
            break;
        }
        m.alloc(3 * i, b - 1, 8, false);
        m.alloc(3 * i + 1, b, 8, false);
        m.alloc(3 * i + 2, b + 1, 8, false);
    }
    m.check_all();
    // 256 KiB + 1 must leave the bag region; everything at or below stays.
    if !MIRI {
        let last = CLASS_SIZES.len() - 1;
        assert!(alloc.owns(m.ptr(3 * last + 1).unwrap()));
        assert!(!alloc.owns(m.ptr(3 * last + 2).unwrap()));
    }
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
    m.free_all(common::FreeOrder::EverySecond);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn zero_size_layouts_are_tolerated() {
    // `GlobalAlloc` forbids zero-sized layouts, but the allocator must not
    // corrupt itself if one slips through: it rounds up to the alignment.
    let alloc = fresh();
    for &align in ALIGNS {
        if MIRI && align > PAGE {
            continue;
        }
        let layout = Layout::from_size_align(0, align).unwrap();
        // SAFETY: robustness check; the pointer is never dereferenced.
        unsafe {
            let p = alloc.alloc(layout);
            assert!(!p.is_null(), "align {align}");
            assert_eq!(p as usize % align, 0);
            alloc.dealloc(p, layout);
        }
    }
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
#[cfg_attr(miri, ignore = "Miri cannot emulate address-space exhaustion")]
fn usize_extremes_fail_cleanly() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, 4, FillMode::Full);
    m.set_must_succeed_below(Some(1 << 20));
    m.alloc(0, 100, 8, false);
    let huge = [
        isize::MAX as usize,
        isize::MAX as usize - PAGE + 1,
        isize::MAX as usize - PAGE,
        usize::MAX - PAGE,
        1 << 50,
        1 << 47,
    ];
    for &size in &huge {
        for &align in ALIGNS {
            // Requests that pass Layout validation must return null, not
            // panic, overflow, or hand out a mapping we cannot honour.
            m.huge(size, align);
            // A realloc to a huge size must fail and keep the old block.
            assert!(!m.realloc(0, size), "realloc to {size}/{align} succeeded");
            m.check_all();
        }
    }
    assert!(m.stats.failed_huge > 0);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn realloc_walks_every_boundary_up_and_down() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, 1, FillMode::Sparse);
    let sizes: Vec<usize> = common::boundaries::nonzero_sizes()
        .into_iter()
        .filter(|&s| !MIRI || s <= 65536)
        .collect();
    m.alloc(0, 1, 8, false);
    for &s in &sizes {
        assert!(m.realloc(0, s), "grow to {s}");
    }
    for &s in sizes.iter().rev() {
        assert!(m.realloc(0, s), "shrink to {s}");
    }
    m.free(0);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn alignment_larger_than_size() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, 4, FillMode::Full);
    for &align in ALIGNS {
        if MIRI && align > PAGE {
            continue;
        }
        for &size in &[1usize, 7, 8, 9] {
            m.alloc(0, size, align, false);
            m.alloc(1, size, align, true);
            m.check_all();
        }
    }
    m.free_all(common::FreeOrder::Lifo);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn small_limit_edge_switches_paths() {
    let alloc = fresh();
    let mut m = Model::new(&alloc, 3, FillMode::Sparse);
    m.alloc(0, SMALL_LIMIT, 8, false);
    m.alloc(1, SMALL_LIMIT + 1, 8, false);
    // size below the limit but alignment above it must take the large path.
    m.alloc(2, 8, SMALL_LIMIT * 2, false);
    if !MIRI {
        assert!(alloc.owns(m.ptr(0).unwrap()));
        assert!(!alloc.owns(m.ptr(1).unwrap()));
        assert!(!alloc.owns(m.ptr(2).unwrap()));
    }
    m.check_all();
    m.free_all(common::FreeOrder::Fifo);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}

#[test]
fn large_close_size_reuse_is_fully_usable() {
    // A cached mapping that is up to 8 KiB larger than the request may be
    // reused.  The payload must be usable for the full new size and the
    // mapping must keep its real extent for later reuse/unmapping.
    let alloc = fresh();
    let big = SMALL_LIMIT + 4 * PAGE;
    let small = SMALL_LIMIT + PAGE;
    let mut m = Model::new(&alloc, 2, FillMode::Full);
    m.alloc(0, big, 8, false);
    let first = m.ptr(0).unwrap();
    m.free(0);
    m.alloc(0, small, 8, false); // close-size hit on the cached mapping
    m.check_all();
    m.free(0);
    m.alloc(1, big, 8, false); // exact hit again only if the extent survived
    assert_eq!(
        m.ptr(1).unwrap(),
        first,
        "cached mapping lost its full extent"
    );
    m.check_all();
    m.free(1);
    // SAFETY: single-threaded test.
    unsafe { alloc.validate_internal_state() };
}
