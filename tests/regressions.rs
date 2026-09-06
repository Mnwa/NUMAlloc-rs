use numalloc::NumaAlloc;
use std::alloc::{GlobalAlloc, Layout};

#[test]
fn cached_large_zeroed() {
    let alloc = NumaAlloc::new();
    let layout = Layout::from_size_align(300_000, 8).unwrap();
    // SAFETY: every allocation is checked and accessed/freed with its layout.
    unsafe {
        let ptr = alloc.alloc(layout);
        assert!(!ptr.is_null());
        ptr.write_bytes(0xa5, layout.size());
        alloc.dealloc(ptr, layout);
        let ptr = alloc.alloc_zeroed(layout);
        assert!(!ptr.is_null());
        assert!(
            std::slice::from_raw_parts(ptr, layout.size())
                .iter()
                .all(|&b| b == 0)
        );
        alloc.dealloc(ptr, layout);
    }
}

#[test]
fn allocator_drop_move_and_recreate() {
    std::thread::spawn(|| {
        let layout = Layout::from_size_align(64, 8).unwrap();
        for _ in 0..16 {
            let alloc = NumaAlloc::new();
            // SAFETY: allocations are live and accessed within their layouts.
            unsafe {
                let ptr = alloc.alloc(layout);
                assert!(!ptr.is_null());
                ptr.write_bytes(0x55, layout.size());
                let moved = Box::new(alloc);
                moved.dealloc(ptr, layout);
            }
        }
    })
    .join()
    .unwrap();
}

#[test]
fn interleaved_allocator_instances() {
    std::thread::spawn(|| {
        let a = NumaAlloc::new();
        let b = NumaAlloc::new();
        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: each pointer is checked and returned to its original allocator.
        unsafe {
            let pa = a.alloc(layout);
            let pb = b.alloc(layout);
            assert!(!pa.is_null() && !pb.is_null());
            pa.write_bytes(0xa5, 64);
            pb.write_bytes(0x5a, 64);
            a.dealloc(pa, layout);
            b.dealloc(pb, layout);
        }
    })
    .join()
    .unwrap();
}

/// Reading the resident set size from /proc (Linux only).
#[cfg(all(target_os = "linux", not(miri)))]
fn rss_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let resident: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    resident * 4096
}

/// `alloc_zeroed` on the large path must not touch freshly mapped pages:
/// the kernel already zero-fills them lazily.  An eager memset would make
/// `vec![0u8; n]` fault in every page up front (regression guard).
#[test]
#[cfg(all(target_os = "linux", not(miri)))]
fn large_alloc_zeroed_fresh_mapping_stays_lazy() {
    let alloc = NumaAlloc::new();
    let size = 256 << 20;
    let layout = Layout::from_size_align(size, 4096).unwrap();
    let before = rss_bytes();
    // SAFETY: valid layout; the mapping is released below.
    unsafe {
        let ptr = alloc.alloc_zeroed(layout);
        assert!(!ptr.is_null());
        let after = rss_bytes();
        assert!(
            after.saturating_sub(before) < 8 << 20,
            "alloc_zeroed touched {} bytes of a fresh mapping",
            after - before
        );
        // Spot-check that the content really is zero.
        for off in [0usize, 4096, size / 2, size - 1] {
            assert_eq!(ptr.add(off).read(), 0);
        }
        alloc.dealloc(ptr, layout);
    }
}

/// Cached (reused) large mappings *are* zeroed by `alloc_zeroed`, whatever
/// their size relative to the madvise threshold.
#[test]
fn large_alloc_zeroed_after_dirty_reuse_below_madvise_threshold() {
    let alloc = NumaAlloc::new();
    // 300 KiB: above SMALL_LIMIT, below MADVISE_THRESHOLD (512 KiB), so the
    // cached pages keep their contents unless zeroed explicitly.
    let layout = Layout::from_size_align(300 * 1024, 8).unwrap();
    // SAFETY: matching alloc/dealloc pairs.
    unsafe {
        let ptr = alloc.alloc(layout);
        assert!(!ptr.is_null());
        ptr.write_bytes(0xa5, layout.size());
        alloc.dealloc(ptr, layout);
        let ptr = alloc.alloc_zeroed(layout);
        assert!(!ptr.is_null());
        assert!(
            std::slice::from_raw_parts(ptr, layout.size())
                .iter()
                .all(|&b| b == 0)
        );
        alloc.dealloc(ptr, layout);
    }
}
