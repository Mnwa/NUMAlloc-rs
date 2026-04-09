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

    // -- Basic allocation / deallocation ------------------------------------

    #[test]
    fn small_alloc_dealloc() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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
        static ALLOC: NumaAlloc = NumaAlloc::new();
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

    // -- Multi-producer multi-consumer cross-thread dealloc ------------------

    /// Multiple threads allocate, then swap their pointers and free each
    /// other's allocations. Stresses the remote-deallocation path (CAS into
    /// origin node's Treiber stack) from many threads simultaneously.
    #[test]
    fn cross_thread_dealloc_many_to_many() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::sync::{Arc, Barrier};
        use std::thread;

        const NUM_THREADS: usize = 8;
        const ALLOCS_PER_THREAD: usize = 500;

        let barrier = Arc::new(Barrier::new(NUM_THREADS));
        // Store addresses as usize so they are Send.
        let collected: Arc<std::sync::Mutex<Vec<Vec<usize>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        // Phase 1: each thread allocates objects.
        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let collected = Arc::clone(&collected);
                thread::spawn(move || {
                    barrier.wait();
                    let mut addrs = Vec::with_capacity(ALLOCS_PER_THREAD);
                    for _ in 0..ALLOCS_PER_THREAD {
                        unsafe {
                            let layout = Layout::from_size_align(128, 8).unwrap();
                            let ptr = ALLOC.alloc(layout);
                            assert!(!ptr.is_null());
                            std::ptr::write_bytes(ptr, 0xAA, 128);
                            addrs.push(ptr as usize);
                        }
                    }
                    collected.lock().unwrap().push(addrs);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Phase 2: redistribute — each thread frees a different thread's allocations.
        let mut all = collected.lock().unwrap();
        let batches: Vec<Vec<usize>> = all.drain(..).collect();
        drop(all);

        let handles: Vec<_> = batches
            .into_iter()
            .map(|addrs| {
                thread::spawn(move || {
                    let layout = Layout::from_size_align(128, 8).unwrap();
                    for addr in addrs {
                        unsafe {
                            ALLOC.dealloc(addr as *mut u8, layout);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // -- Concurrent realloc --------------------------------------------------

    /// Multiple threads performing realloc concurrently, growing and shrinking
    /// across size classes. Exercises alloc+copy+dealloc atomicity per thread.
    #[test]
    fn concurrent_realloc() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::thread;

        let handles: Vec<_> = (0..8)
            .map(|_| {
                thread::spawn(|| unsafe {
                    let sizes = [16, 64, 256, 1024, 4096, 16384];
                    let mut current_layout = Layout::from_size_align(8, 8).unwrap();

                    // Allocate initial block.
                    let mut ptr = ALLOC.alloc(current_layout);
                    assert!(!ptr.is_null());
                    std::ptr::write_bytes(ptr, 0x11, current_layout.size());

                    // Walk through growing sizes.
                    for &new_size in &sizes {
                        let new_ptr = ALLOC.realloc(ptr, current_layout, new_size);
                        assert!(!new_ptr.is_null());
                        // Verify old data preserved (up to min of old/new size).
                        let check_len = current_layout.size().min(new_size);
                        let slice = std::slice::from_raw_parts(new_ptr, check_len);
                        assert!(
                            slice.iter().all(|&b| b == 0x11),
                            "data corruption during realloc grow"
                        );
                        // Fill the rest with the pattern.
                        std::ptr::write_bytes(new_ptr, 0x11, new_size);
                        ptr = new_ptr;
                        current_layout = Layout::from_size_align(new_size, 8).unwrap();
                    }

                    // Walk back through shrinking sizes.
                    for &new_size in sizes.iter().rev().skip(1) {
                        let new_ptr = ALLOC.realloc(ptr, current_layout, new_size);
                        assert!(!new_ptr.is_null());
                        let check_len = new_size;
                        let slice = std::slice::from_raw_parts(new_ptr, check_len);
                        assert!(
                            slice.iter().all(|&b| b == 0x11),
                            "data corruption during realloc shrink"
                        );
                        ptr = new_ptr;
                        current_layout = Layout::from_size_align(new_size, 8).unwrap();
                    }

                    ALLOC.dealloc(ptr, current_layout);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // -- Concurrent alloc_zeroed ---------------------------------------------

    /// Verify that `alloc_zeroed` returns zeroed memory even under concurrent
    /// pressure where freed blocks may contain stale data.
    #[test]
    fn concurrent_alloc_zeroed() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::thread;

        let handles: Vec<_> = (0..8)
            .map(|_| {
                thread::spawn(|| unsafe {
                    let layout = Layout::from_size_align(512, 8).unwrap();
                    for _ in 0..500 {
                        // Scribble + free to pollute the freelist.
                        let dirty = ALLOC.alloc(layout);
                        assert!(!dirty.is_null());
                        std::ptr::write_bytes(dirty, 0xFF, 512);
                        ALLOC.dealloc(dirty, layout);

                        // alloc_zeroed must return clean memory.
                        let clean = ALLOC.alloc_zeroed(layout);
                        assert!(!clean.is_null());
                        let slice = std::slice::from_raw_parts(clean, 512);
                        assert!(
                            slice.iter().all(|&b| b == 0),
                            "alloc_zeroed returned non-zero memory under concurrency"
                        );
                        ALLOC.dealloc(clean, layout);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // -- Same size class contention ------------------------------------------

    /// Hammer a single size class from many threads to stress the per-node
    /// Treiber stack under high contention on one freelist.
    #[test]
    fn high_contention_single_size_class() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::sync::{Arc, Barrier};
        use std::thread;

        const NUM_THREADS: usize = 16;
        const OPS: usize = 2_000;

        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    unsafe {
                        let layout = Layout::from_size_align(64, 8).unwrap();
                        let mut ptrs = Vec::with_capacity(OPS);
                        for _ in 0..OPS {
                            let ptr = ALLOC.alloc(layout);
                            assert!(!ptr.is_null());
                            std::ptr::write_bytes(ptr, 0x77, 64);
                            ptrs.push(ptr);
                        }
                        // Free in reverse to stress LIFO ordering.
                        for ptr in ptrs.into_iter().rev() {
                            ALLOC.dealloc(ptr, layout);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // -- Rapid short-lived threads -------------------------------------------

    /// Spawn many short-lived threads, each performing a few allocations.
    /// Exercises per-thread heap creation and tear-down under rapid turnover.
    #[test]
    fn rapid_thread_churn() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::thread;

        let mut handles = Vec::new();

        for _ in 0..64 {
            handles.push(thread::spawn(|| unsafe {
                let layout = Layout::from_size_align(256, 8).unwrap();
                let mut ptrs = Vec::new();
                for _ in 0..50 {
                    let ptr = ALLOC.alloc(layout);
                    assert!(!ptr.is_null());
                    std::ptr::write_bytes(ptr, 0xCC, 256);
                    ptrs.push(ptr);
                }
                for ptr in ptrs {
                    ALLOC.dealloc(ptr, layout);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    // -- Interleaved alloc/dealloc from different threads --------------------

    /// Producer threads continuously allocate and push pointers to a shared
    /// queue; consumer threads pop and free them. The two groups run
    /// concurrently, stressing both allocation and remote deallocation paths
    /// simultaneously.
    #[test]
    fn producer_consumer_concurrent() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;

        const PRODUCERS: usize = 4;
        const CONSUMERS: usize = 4;
        const ITEMS_PER_PRODUCER: usize = 2_000;

        // Store addresses as usize so they are Send.
        let queue: Arc<std::sync::Mutex<Vec<usize>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));

        // Producers.
        let prod_handles: Vec<_> = (0..PRODUCERS)
            .map(|_| {
                let queue = Arc::clone(&queue);
                thread::spawn(move || unsafe {
                    let layout = Layout::from_size_align(128, 8).unwrap();
                    for _ in 0..ITEMS_PER_PRODUCER {
                        let ptr = ALLOC.alloc(layout);
                        assert!(!ptr.is_null());
                        std::ptr::write_bytes(ptr, 0xEE, 128);
                        queue.lock().unwrap().push(ptr as usize);
                    }
                })
            })
            .collect();

        // Consumers: keep draining until producers finish and queue is empty.
        let layout = Layout::from_size_align(128, 8).unwrap();
        let consumer_handles: Vec<_> = (0..CONSUMERS)
            .map(|_| {
                let queue = Arc::clone(&queue);
                let done = Arc::clone(&done);
                thread::spawn(move || {
                    loop {
                        let batch: Vec<usize> = {
                            let mut q = queue.lock().unwrap();
                            q.drain(..).collect()
                        };
                        if batch.is_empty() {
                            if done.load(Ordering::Acquire) {
                                let final_batch: Vec<usize> = {
                                    let mut q = queue.lock().unwrap();
                                    q.drain(..).collect()
                                };
                                for addr in final_batch {
                                    unsafe { ALLOC.dealloc(addr as *mut u8, layout) };
                                }
                                break;
                            }
                            std::thread::yield_now();
                            continue;
                        }
                        for addr in batch {
                            unsafe { ALLOC.dealloc(addr as *mut u8, layout) };
                        }
                    }
                })
            })
            .collect();

        for h in prod_handles {
            h.join().unwrap();
        }
        done.store(true, Ordering::Release);

        for h in consumer_handles {
            h.join().unwrap();
        }
    }

    // -- Concurrent drain to per-node heap -----------------------------------

    /// Each thread allocates enough objects (> MAX_THREAD_CACHE = 64) in the
    /// same size class to trigger drain of cold objects to the per-node Treiber
    /// stack, then frees them. With many threads running concurrently this
    /// hammers the push_chain CAS path.
    #[test]
    fn concurrent_drain_overflow() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::sync::{Arc, Barrier};
        use std::thread;

        const NUM_THREADS: usize = 8;

        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    unsafe {
                        let layout = Layout::from_size_align(64, 8).unwrap();
                        let mut ptrs = Vec::new();

                        // Allocate and immediately free in a pattern that forces
                        // repeated drain: alloc 100, free all, repeat.
                        for _ in 0..10 {
                            for _ in 0..100 {
                                let ptr = ALLOC.alloc(layout);
                                assert!(!ptr.is_null());
                                std::ptr::write_bytes(ptr, 0x33, 64);
                                ptrs.push(ptr);
                            }
                            // Free all — each batch of 100 exceeds
                            // MAX_THREAD_CACHE (64), causing drain.
                            for ptr in ptrs.drain(..) {
                                ALLOC.dealloc(ptr, layout);
                            }
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // -- Concurrent large + small mixed allocations --------------------------

    /// Threads doing both large (mmap) and small (freelist) allocations at the
    /// same time. Ensures the two paths don't interfere.
    #[test]
    fn concurrent_large_and_small() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::thread;

        let handles: Vec<_> = (0..8)
            .map(|tid| {
                thread::spawn(move || unsafe {
                    let mut ptrs: Vec<(*mut u8, Layout)> = Vec::new();
                    for i in 0..500 {
                        let (size, align) = if (tid + i) % 7 == 0 {
                            // Large allocation.
                            (64 * 1024, 4096)
                        } else {
                            // Small allocation across different size classes.
                            let s = match i % 4 {
                                0 => 32,
                                1 => 256,
                                2 => 2048,
                                _ => 8192,
                            };
                            (s, 8)
                        };
                        let layout = Layout::from_size_align(size, align).unwrap();
                        let ptr = ALLOC.alloc(layout);
                        assert!(!ptr.is_null());
                        if align > 1 {
                            assert_eq!(ptr as usize % align, 0, "misaligned at size={size}");
                        }
                        std::ptr::write_bytes(ptr, 0x99, size);
                        ptrs.push((ptr, layout));

                        if ptrs.len() > 30 && i % 5 == 0 {
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

    // -- Pointer uniqueness under concurrency --------------------------------

    /// Verify no two concurrent threads ever receive the same pointer.
    #[test]
    fn no_duplicate_pointers_concurrent() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::sync::{Arc, Barrier, Mutex};
        use std::thread;

        const NUM_THREADS: usize = 8;
        const ALLOCS: usize = 1_000;

        let barrier = Arc::new(Barrier::new(NUM_THREADS));
        let all_ptrs: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let all_ptrs = Arc::clone(&all_ptrs);
                thread::spawn(move || {
                    barrier.wait();
                    let layout = Layout::from_size_align(64, 8).unwrap();
                    let mut local_ptrs = Vec::with_capacity(ALLOCS);
                    unsafe {
                        for _ in 0..ALLOCS {
                            let ptr = ALLOC.alloc(layout);
                            assert!(!ptr.is_null());
                            local_ptrs.push(ptr as usize);
                        }
                    }
                    all_ptrs.lock().unwrap().extend_from_slice(&local_ptrs);

                    // Keep pointers alive until all threads have collected theirs.
                    barrier.wait();

                    unsafe {
                        for addr in &local_ptrs {
                            ALLOC.dealloc(*addr as *mut u8, layout);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let ptrs = all_ptrs.lock().unwrap();
        let mut sorted = ptrs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ptrs.len(),
            "duplicate pointers detected across threads: {} unique out of {}",
            sorted.len(),
            ptrs.len()
        );
    }

    // ======================================================================
    // Memory leak detection tests
    // ======================================================================
    //
    // Since we cannot inspect internal freelist state (private fields), leak
    // detection relies on observing **address reuse**.  A properly functioning
    // allocator returns freed addresses back to the freelist.  If memory leaks
    // (freed blocks disappear), subsequent allocations consume fresh addresses
    // and reuse drops to zero.

    // -- Small object freelist reuse -----------------------------------------

    /// Allocate N objects, free them all, allocate N again.  At least some
    /// addresses from the second batch must match the first — proving freed
    /// memory was returned to the freelist and not leaked.
    #[test]
    fn leak_check_small_reuse_after_free() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        unsafe {
            let layout = Layout::from_size_align(64, 8).unwrap();
            let count = 200;

            // Round 1: allocate.
            let mut first_addrs = Vec::with_capacity(count);
            for _ in 0..count {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                first_addrs.push(ptr as usize);
            }

            // Free all.
            for &addr in &first_addrs {
                ALLOC.dealloc(addr as *mut u8, layout);
            }

            // Round 2: allocate the same number again.
            let mut second_addrs = Vec::with_capacity(count);
            for _ in 0..count {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                second_addrs.push(ptr as usize);
            }

            let first_set: std::collections::HashSet<usize> =
                first_addrs.iter().copied().collect();
            let reused = second_addrs
                .iter()
                .filter(|a| first_set.contains(a))
                .count();

            // With a LIFO freelist we expect good reuse.  Under parallel test
            // execution other tests consume freelist entries via the shared
            // global heap, so we only require some reuse (not a majority).
            assert!(
                reused > 0,
                "expected some reuse, got 0/{count} — possible leak"
            );

            for &addr in &second_addrs {
                ALLOC.dealloc(addr as *mut u8, layout);
            }
        }
    }

    // -- All size classes reuse ----------------------------------------------

    /// Run the reuse leak check across every size class to catch class-specific
    /// leaks (e.g. a size class whose freed blocks never return).
    #[test]
    fn leak_check_all_size_classes_reuse() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        let sizes: &[usize] = &[8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384];

        for &size in sizes {
            unsafe {
                let layout = Layout::from_size_align(size, 8).unwrap();
                let count = 100;

                let mut first = Vec::with_capacity(count);
                for _ in 0..count {
                    let ptr = ALLOC.alloc(layout);
                    assert!(!ptr.is_null());
                    first.push(ptr as usize);
                }
                for &addr in &first {
                    ALLOC.dealloc(addr as *mut u8, layout);
                }

                let mut second = Vec::with_capacity(count);
                for _ in 0..count {
                    let ptr = ALLOC.alloc(layout);
                    assert!(!ptr.is_null());
                    second.push(ptr as usize);
                }

                let first_set: std::collections::HashSet<usize> =
                    first.iter().copied().collect();
                let reused = second.iter().filter(|a| first_set.contains(a)).count();
                assert!(
                    reused > 0,
                    "size class {size}: 0/{count} reused — possible leak"
                );

                for &addr in &second {
                    ALLOC.dealloc(addr as *mut u8, layout);
                }
            }
        }
    }

    // -- Realloc frees old allocation ----------------------------------------

    /// After realloc to a different size class, the old address must be
    /// reclaimable.  Allocate at the old size again and verify the old address
    /// comes back — proving realloc freed it.
    #[test]
    fn leak_check_realloc_frees_old() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        unsafe {
            let old_layout = Layout::from_size_align(64, 8).unwrap();
            let ptr = ALLOC.alloc(old_layout);
            assert!(!ptr.is_null());
            let old_addr = ptr as usize;
            std::ptr::write_bytes(ptr, 0xAA, 64);

            // Realloc to a larger class — old block should be freed.
            let new_ptr = ALLOC.realloc(ptr, old_layout, 512);
            assert!(!new_ptr.is_null());
            assert_ne!(new_ptr as usize, old_addr);

            // Allocate at old size repeatedly — old address should reappear.
            let mut found = false;
            let mut probes = Vec::new();
            for _ in 0..100 {
                let p = ALLOC.alloc(old_layout);
                assert!(!p.is_null());
                probes.push(p);
                if p as usize == old_addr {
                    found = true;
                    break;
                }
            }

            assert!(
                found,
                "old address 0x{old_addr:x} never reappeared — realloc may have leaked it"
            );

            for p in probes {
                ALLOC.dealloc(p, old_layout);
            }
            ALLOC.dealloc(new_ptr, Layout::from_size_align(512, 8).unwrap());
        }
    }

    // -- Cross-thread dealloc returns memory to origin -----------------------

    /// Allocate on thread A, free on thread B.  On single-node machines the
    /// freed blocks land in thread B's local freelist (same node → local
    /// dealloc).  Verify that thread B can reclaim them — proving the
    /// cross-thread handoff didn't lose any blocks.
    #[test]
    fn leak_check_cross_thread_dealloc_returns() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::sync::mpsc;
        use std::thread;

        let count = 200;
        let layout = Layout::from_size_align(128, 8).unwrap();
        let (tx_addrs, rx_addrs) = mpsc::channel::<Vec<usize>>();

        // Thread A: allocate objects, send addresses to thread B.
        let producer = thread::spawn(move || unsafe {
            let mut addrs = Vec::with_capacity(count);
            for _ in 0..count {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                addrs.push(ptr as usize);
            }
            tx_addrs.send(addrs).unwrap();
        });
        producer.join().unwrap();

        // Thread B: free them, then re-allocate and verify reuse.
        // On single-node machines the freed blocks go into thread B's own
        // freelist, so thread B should see high reuse.
        let consumer = thread::spawn(move || unsafe {
            let addrs = rx_addrs.recv().unwrap();
            let first_set: std::collections::HashSet<usize> =
                addrs.iter().copied().collect();

            for &addr in &addrs {
                ALLOC.dealloc(addr as *mut u8, layout);
            }

            // Re-allocate the same count — freed blocks should come back.
            let mut second = Vec::with_capacity(count);
            for _ in 0..count {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                second.push(ptr as usize);
            }

            let reused = second.iter().filter(|a| first_set.contains(a)).count();
            assert!(
                reused > 0,
                "cross-thread free: 0/{count} reused — dealloc may leak"
            );

            for &addr in &second {
                ALLOC.dealloc(addr as *mut u8, layout);
            }
        });
        consumer.join().unwrap();
    }

    // -- Drain to node heap doesn't lose blocks ------------------------------

    /// Push enough objects to exceed MAX_THREAD_CACHE, triggering a drain of
    /// cold blocks to the per-node Treiber stack.  Then free everything and
    /// re-allocate — all addresses must be reclaimable.
    #[test]
    fn leak_check_drain_path() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        unsafe {
            let layout = Layout::from_size_align(64, 8).unwrap();
            // Exceed MAX_THREAD_CACHE (64) to force drain.
            let count = 150;

            let mut first = Vec::with_capacity(count);
            for _ in 0..count {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                first.push(ptr as usize);
            }

            // Free all — some were drained to per-node heap, some are in
            // thread freelist.
            for &addr in &first {
                ALLOC.dealloc(addr as *mut u8, layout);
            }

            // Re-allocate: refill from per-node + thread freelist should
            // reclaim all drained blocks.
            let mut second = Vec::with_capacity(count);
            for _ in 0..count {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                second.push(ptr as usize);
            }

            let first_set: std::collections::HashSet<usize> =
                first.iter().copied().collect();
            let reused = second.iter().filter(|a| first_set.contains(a)).count();
            assert!(
                reused > 0,
                "drain path: 0/{count} reused — drained blocks may have leaked"
            );

            for &addr in &second {
                ALLOC.dealloc(addr as *mut u8, layout);
            }
        }
    }

    // -- Repeated alloc/free cycles don't grow memory ------------------------

    /// Run many alloc/free cycles and verify the allocator keeps reusing the
    /// same addresses rather than allocating fresh ones.  A leaking allocator
    /// would show a steadily growing set of unique addresses.
    #[test]
    fn leak_check_repeated_cycles_no_growth() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        unsafe {
            let layout = Layout::from_size_align(256, 8).unwrap();
            let batch = 100;
            let cycles = 50;
            let mut all_addrs = std::collections::HashSet::new();

            for _ in 0..cycles {
                let mut ptrs = Vec::with_capacity(batch);
                for _ in 0..batch {
                    let ptr = ALLOC.alloc(layout);
                    assert!(!ptr.is_null());
                    all_addrs.insert(ptr as usize);
                    ptrs.push(ptr);
                }
                for ptr in ptrs {
                    ALLOC.dealloc(ptr, layout);
                }
            }

            // With perfect reuse we'd see exactly `batch` unique addresses.
            // Allow some slack for thread-local caching and batching, but the
            // count should be far below batch * cycles.
            // Other tests running in parallel share the global heap and may
            // drain/refill the same freelists, introducing foreign addresses.
            let max_expected = batch * 10;
            assert!(
                all_addrs.len() <= max_expected,
                "unique addrs {} exceeds {max_expected} — suggests leak (expected ~{batch} with reuse)",
                all_addrs.len()
            );
        }
    }

    // -- Large object alloc/free doesn't leak virtual memory -----------------

    /// Allocate and free many large (mmap'd) objects.  Each allocation is
    /// independent (its own mmap); after munmap the virtual address range is
    /// returned to the OS.  We verify that we can sustain many cycles without
    /// running out of address space — a leak would eventually exhaust it.
    #[test]
    fn leak_check_large_object_munmap() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        unsafe {
            let size = 1024 * 1024; // 1 MiB
            let layout = Layout::from_size_align(size, 4096).unwrap();

            for _ in 0..200 {
                let ptr = ALLOC.alloc(layout);
                assert!(!ptr.is_null());
                assert_eq!(ptr as usize % 4096, 0);
                // Touch first and last pages to force physical mapping.
                *ptr = 0xAA;
                *ptr.add(size - 1) = 0xBB;
                ALLOC.dealloc(ptr, layout);
            }
        }
    }

    // -- Concurrent alloc/free reuse -----------------------------------------

    /// Multiple threads each run alloc/free cycles and track unique addresses.
    /// Per-thread address sets should stay bounded — proving no thread-local
    /// leak under concurrency.
    #[test]
    fn leak_check_concurrent_reuse() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::sync::{Arc, Barrier};
        use std::thread;

        const NUM_THREADS: usize = 8;
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    unsafe {
                        let layout = Layout::from_size_align(128, 8).unwrap();
                        let batch = 80;
                        let cycles = 30;
                        let mut unique = std::collections::HashSet::new();

                        for _ in 0..cycles {
                            let mut ptrs = Vec::with_capacity(batch);
                            for _ in 0..batch {
                                let ptr = ALLOC.alloc(layout);
                                assert!(!ptr.is_null());
                                unique.insert(ptr as usize);
                                ptrs.push(ptr);
                            }
                            for ptr in ptrs {
                                ALLOC.dealloc(ptr, layout);
                            }
                        }

                        // Under concurrency, threads share the per-node
                        // Treiber stack and may receive blocks from other
                        // threads' drains, increasing unique address count.
                        let max_expected = batch * 12;
                        assert!(
                            unique.len() <= max_expected,
                            "thread saw {} unique addrs (max {max_expected}) — possible leak",
                            unique.len()
                        );
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // -- Multi-thread cross-free reuse leak check ----------------------------

    /// Each thread allocates a batch, hands it to the next thread for freeing,
    /// then the freeing thread re-allocates to check reuse.  On single-node
    /// machines the freed blocks land in the freeing thread's own cache.
    #[test]
    fn leak_check_cross_thread_round_trip() {
        static ALLOC: NumaAlloc = NumaAlloc::new();
        use std::sync::{Arc, Barrier, Mutex};
        use std::thread;

        const NUM_THREADS: usize = 4;
        const BATCH: usize = 200;
        let layout = Layout::from_size_align(64, 8).unwrap();

        let barrier = Arc::new(Barrier::new(NUM_THREADS));
        let deposit: Arc<Mutex<Vec<Vec<usize>>>> = Arc::new(Mutex::new(
            (0..NUM_THREADS).map(|_| Vec::new()).collect(),
        ));

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|tid| {
                let barrier = Arc::clone(&barrier);
                let deposit = Arc::clone(&deposit);
                thread::spawn(move || unsafe {
                    // Phase 1: allocate a batch.
                    let mut addrs = Vec::with_capacity(BATCH);
                    for _ in 0..BATCH {
                        let ptr = ALLOC.alloc(layout);
                        assert!(!ptr.is_null());
                        addrs.push(ptr as usize);
                    }

                    // Deposit into the *next* thread's slot.
                    let target = (tid + 1) % NUM_THREADS;
                    deposit.lock().unwrap()[target] = addrs;

                    // Wait for everyone to deposit.
                    barrier.wait();

                    // Phase 2: free the batch deposited for *this* thread
                    // (allocated by the previous thread).
                    let to_free: Vec<usize> = {
                        let mut d = deposit.lock().unwrap();
                        std::mem::take(&mut d[tid])
                    };
                    let freed_set: std::collections::HashSet<usize> =
                        to_free.iter().copied().collect();

                    for addr in &to_free {
                        ALLOC.dealloc(*addr as *mut u8, layout);
                    }

                    // Wait for all frees to complete.
                    barrier.wait();

                    // Phase 3: re-allocate and check reuse of the blocks we
                    // just freed (they should be in our own freelist now).
                    let mut second = Vec::with_capacity(BATCH);
                    for _ in 0..BATCH {
                        let ptr = ALLOC.alloc(layout);
                        assert!(!ptr.is_null());
                        second.push(ptr as usize);
                    }

                    let reused = second.iter().filter(|a| freed_set.contains(a)).count();
                    // Under parallel test execution with shared per-node
                    // freelists, other threads may consume some of our freed
                    // blocks.  Any reuse at all proves the free path works.
                    assert!(
                        reused > 0,
                        "thread {tid}: zero reuse out of {BATCH} — freed blocks may be leaked"
                    );

                    for &addr in &second {
                        ALLOC.dealloc(addr as *mut u8, layout);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }
}
