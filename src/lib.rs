mod allocator;
mod freelist;
mod heap;
mod node_heap;
mod platform;
mod size_class;
mod thread_heap;

pub use allocator::NumaAlloc;

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout};

    use crate::NumaAlloc;

    static ALLOC: NumaAlloc = NumaAlloc::new();

    // -- Basic allocation / deallocation ------------------------------------

    #[test]
    fn small_alloc_dealloc() {
        unsafe {
            let layout = Layout::from_size_align(64, 8).unwrap();
            let ptr = ALLOC.alloc(layout);
            assert!(!ptr.is_null());
            std::ptr::write_bytes(ptr, 0xAB, 64);
            ALLOC.dealloc(ptr, layout);
        }
    }

    #[test]
    fn all_size_classes() {
        unsafe {
            for &size in &[8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384] {
                let layout = Layout::from_size_align(size, 8).unwrap();
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null(), "failed for size {size}");
                std::ptr::write_bytes(ptr, 0xCD, size);
                ALLOC.dealloc(ptr, layout);
            }
        }
    }

    #[test]
    fn large_alloc_dealloc() {
        unsafe {
            let size = 1024 * 1024; // 1 MiB
            let layout = Layout::from_size_align(size, 4096).unwrap();
            let ptr = ALLOC.alloc(layout);
            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % 4096, 0);
            std::ptr::write_bytes(ptr, 0xEF, size);
            ALLOC.dealloc(ptr, layout);
        }
    }

    // -- Alignment ----------------------------------------------------------

    #[test]
    fn alignment_power_of_two() {
        unsafe {
            for align_shift in 3..=12 {
                let align = 1usize << align_shift;
                let layout = Layout::from_size_align(align, align).unwrap();
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                assert_eq!(
                    ptr as usize % align,
                    0,
                    "misaligned for align={align}"
                );
                ALLOC.dealloc(ptr, layout);
            }
        }
    }

    // -- Reuse after free ---------------------------------------------------

    #[test]
    fn reuse_after_free() {
        unsafe {
            let layout = Layout::from_size_align(128, 8).unwrap();
            let mut seen = std::collections::HashSet::new();

            for _ in 0..100 {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                ALLOC.dealloc(ptr, layout);
                seen.insert(ptr as usize);
            }
            // With a freelist the same slot is reused quickly.
            assert!(seen.len() < 100, "expected reuse, got {} unique ptrs", seen.len());
        }
    }

    // -- Bulk allocations (exercises bag allocation + drain) -----------------

    #[test]
    fn many_allocs() {
        unsafe {
            let layout = Layout::from_size_align(64, 8).unwrap();
            let mut ptrs: Vec<*mut u8> = Vec::new();
            for _ in 0..10_000 {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                std::ptr::write_bytes(ptr, 0x42, 64);
                ptrs.push(ptr);
            }
            // All pointers must be unique.
            let mut sorted = ptrs.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), ptrs.len(), "duplicate pointers detected");

            for ptr in ptrs {
                ALLOC.dealloc(ptr, layout);
            }
        }
    }

    // -- Multi-threaded allocation ------------------------------------------

    #[test]
    fn multithreaded_alloc_dealloc() {
        use std::thread;

        let handles: Vec<_> = (0..8)
            .map(|_| {
                thread::spawn(|| unsafe {
                    let layout = Layout::from_size_align(64, 8).unwrap();
                    let mut ptrs = Vec::new();
                    for _ in 0..2_000 {
                        let ptr = ALLOC.alloc(layout);
                        assert!(!ptr.is_null());
                        std::ptr::write_bytes(ptr, 0x55, 64);
                        ptrs.push(ptr);
                    }
                    for ptr in ptrs {
                        ALLOC.dealloc(ptr, layout);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // -- Cross-thread (remote) deallocation ---------------------------------

    #[test]
    fn cross_thread_dealloc() {
        use std::sync::mpsc;
        use std::thread;

        let (tx, rx) = mpsc::channel();

        // Producer thread: allocate objects.
        let producer = thread::spawn(move || unsafe {
            let layout = Layout::from_size_align(256, 8).unwrap();
            let mut addrs = Vec::new();
            for _ in 0..200 {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                std::ptr::write_bytes(ptr, 0xAA, 256);
                addrs.push(ptr as usize);
            }
            tx.send(addrs).unwrap();
        });
        producer.join().unwrap();

        let addrs = rx.recv().unwrap();

        // Consumer thread: free them (different thread → origin-aware path).
        let consumer = thread::spawn(move || unsafe {
            let layout = Layout::from_size_align(256, 8).unwrap();
            for addr in addrs {
                ALLOC.dealloc(addr as *mut u8, layout);
            }
        });
        consumer.join().unwrap();
    }

    // -- Mixed sizes --------------------------------------------------------

    #[test]
    fn mixed_sizes() {
        unsafe {
            let sizes: &[usize] = &[8, 17, 33, 100, 500, 1024, 4000, 8192, 16384, 32768, 100_000];
            let mut ptrs = Vec::new();

            for &size in sizes {
                let layout = Layout::from_size_align(size, 8).unwrap();
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null(), "failed for size {size}");
                std::ptr::write_bytes(ptr, 0xBB, size);
                ptrs.push((ptr, layout));
            }

            for (ptr, layout) in ptrs {
                ALLOC.dealloc(ptr, layout);
            }
        }
    }

    // -- alloc_zeroed -------------------------------------------------------

    #[test]
    fn alloc_zeroed_small() {
        unsafe {
            let layout = Layout::from_size_align(256, 8).unwrap();
            // Allocate, scribble, free, then alloc_zeroed — must be all zeros
            // even if the freelist hands back the same block.
            let ptr = ALLOC.alloc(layout);
            assert!(!ptr.is_null());
            std::ptr::write_bytes(ptr, 0xFF, 256);
            ALLOC.dealloc(ptr, layout);

            let ptr2 = ALLOC.alloc_zeroed(layout);
            assert!(!ptr2.is_null());
            let slice = std::slice::from_raw_parts(ptr2, 256);
            assert!(slice.iter().all(|&b| b == 0), "alloc_zeroed returned non-zero memory");
            ALLOC.dealloc(ptr2, layout);
        }
    }

    #[test]
    fn alloc_zeroed_large() {
        unsafe {
            let size = 128 * 1024; // 128 KiB — large path
            let layout = Layout::from_size_align(size, 8).unwrap();
            let ptr = ALLOC.alloc_zeroed(layout);
            assert!(!ptr.is_null());
            let slice = std::slice::from_raw_parts(ptr, size);
            assert!(slice.iter().all(|&b| b == 0), "large alloc_zeroed not zeroed");
            ALLOC.dealloc(ptr, layout);
        }
    }

    // -- realloc ------------------------------------------------------------

    #[test]
    fn realloc_same_size_class() {
        unsafe {
            let layout = Layout::from_size_align(60, 8).unwrap();
            let ptr = ALLOC.alloc(layout);
            assert!(!ptr.is_null());
            std::ptr::write_bytes(ptr, 0xAA, 60);

            // 60 and 63 both round up to size class 64 — pointer unchanged.
            let ptr2 = ALLOC.realloc(ptr, layout, 63);
            assert_eq!(ptr, ptr2, "same size class should return same pointer");

            ALLOC.dealloc(ptr2, Layout::from_size_align(63, 8).unwrap());
        }
    }

    #[test]
    fn realloc_grow() {
        unsafe {
            let old_layout = Layout::from_size_align(32, 8).unwrap();
            let ptr = ALLOC.alloc(old_layout);
            assert!(!ptr.is_null());
            // Write a pattern to verify data is preserved after realloc.
            for i in 0..32u8 {
                *ptr.add(i as usize) = i;
            }

            let new_ptr = ALLOC.realloc(ptr, old_layout, 256);
            assert!(!new_ptr.is_null());
            let slice = std::slice::from_raw_parts(new_ptr, 32);
            for (i, &b) in slice.iter().enumerate() {
                assert_eq!(b, i as u8, "data not preserved at byte {i}");
            }

            ALLOC.dealloc(new_ptr, Layout::from_size_align(256, 8).unwrap());
        }
    }

    #[test]
    fn realloc_shrink() {
        unsafe {
            let old_layout = Layout::from_size_align(4096, 8).unwrap();
            let ptr = ALLOC.alloc(old_layout);
            assert!(!ptr.is_null());
            for i in 0..64u8 {
                *ptr.add(i as usize) = i;
            }

            let new_ptr = ALLOC.realloc(ptr, old_layout, 64);
            assert!(!new_ptr.is_null());
            let slice = std::slice::from_raw_parts(new_ptr, 64);
            for (i, &b) in slice.iter().enumerate() {
                assert_eq!(b, i as u8, "data not preserved at byte {i}");
            }

            ALLOC.dealloc(new_ptr, Layout::from_size_align(64, 8).unwrap());
        }
    }

    #[test]
    fn realloc_small_to_large() {
        unsafe {
            let old_layout = Layout::from_size_align(128, 8).unwrap();
            let ptr = ALLOC.alloc(old_layout);
            assert!(!ptr.is_null());
            std::ptr::write_bytes(ptr, 0xBB, 128);

            let big = 256 * 1024;
            let new_ptr = ALLOC.realloc(ptr, old_layout, big);
            assert!(!new_ptr.is_null());
            // First 128 bytes should be preserved.
            let slice = std::slice::from_raw_parts(new_ptr, 128);
            assert!(slice.iter().all(|&b| b == 0xBB));

            ALLOC.dealloc(new_ptr, Layout::from_size_align(big, 8).unwrap());
        }
    }

    #[test]
    fn realloc_large_to_small() {
        unsafe {
            let big = 128 * 1024;
            let old_layout = Layout::from_size_align(big, 8).unwrap();
            let ptr = ALLOC.alloc(old_layout);
            assert!(!ptr.is_null());
            std::ptr::write_bytes(ptr, 0xCC, 64);

            let new_ptr = ALLOC.realloc(ptr, old_layout, 64);
            assert!(!new_ptr.is_null());
            let slice = std::slice::from_raw_parts(new_ptr, 64);
            assert!(slice.iter().all(|&b| b == 0xCC));

            ALLOC.dealloc(new_ptr, Layout::from_size_align(64, 8).unwrap());
        }
    }

    // -- Stress test --------------------------------------------------------

    #[test]
    fn stress_concurrent_mixed() {
        use std::thread;

        let handles: Vec<_> = (0..4)
            .map(|tid| {
                thread::spawn(move || unsafe {
                    let mut ptrs: Vec<(*mut u8, Layout)> = Vec::new();
                    for i in 0..5_000 {
                        let size = match (tid + i) % 5 {
                            0 => 16,
                            1 => 128,
                            2 => 1024,
                            3 => 8192,
                            _ => 64,
                        };
                        let layout = Layout::from_size_align(size, 8).unwrap();
                        let ptr = ALLOC.alloc(layout);
                        assert!(!ptr.is_null());
                        std::ptr::write_bytes(ptr, 0xDD, size);
                        ptrs.push((ptr, layout));

                        // Free ~half to exercise reuse while allocating.
                        if ptrs.len() > 20 && i % 3 == 0 {
                            let (p, l) = ptrs.swap_remove(0);
                            ALLOC.dealloc(p, l);
                        }
                    }
                    for (p, l) in ptrs {
                        ALLOC.dealloc(p, l);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }
}
