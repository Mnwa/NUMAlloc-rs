//! Integration tests that use NumaAlloc as the **global allocator**.
//!
//! Unlike the unit tests in `src/lib.rs` (which call `GlobalAlloc` methods
//! directly while the test binary uses the system allocator), these tests
//! exercise the real bootstrap path:
//!
//! - Heap initialisation reads `/sys/devices/system/node/` via `std::fs`,
//!   which allocates through the global allocator.  Without a re-entrancy
//!   guard this deadlocks on the `OnceLock` inside `NumaAlloc::heap()`.
//!
//! - Per-thread heap setup calls `bind_thread_to_node()` which uses
//!   `std::fs::read_to_string()`.  If the thread heap is not registered in
//!   TLS before that call, the recursive allocation re-enters the slow path
//!   of `thread_heap()` infinitely.

#[global_allocator]
static ALLOC: numalloc::NumaAlloc = numalloc::NumaAlloc::new();

// -- Heap init does not deadlock -------------------------------------------

/// The very first allocation in the process triggers `OnceLock::get_or_init`
/// → `detect_topology()` → `std::fs::read_dir` → global allocator.
/// If the re-entrancy guard is missing this test hangs (deadlock).
#[test]
fn heap_init_no_deadlock() {
    // A `Box` goes through the global allocator.
    let v: Box<[u8; 64]> = Box::new([0xAB; 64]);
    assert_eq!(v[0], 0xAB);
}

/// `String` and `Vec` exercise multiple allocations and reallocations
/// through the global allocator, verifying that the heap is fully usable
/// after init.
#[test]
fn heap_init_std_collections() {
    let mut s = String::new();
    for i in 0..100 {
        s.push_str(&format!("item {i}, "));
    }
    assert!(s.contains("item 99"));

    let v: Vec<u64> = (0..1000).collect();
    assert_eq!(v.len(), 1000);
    assert_eq!(v[999], 999);
}

// -- Per-thread heap init does not recurse ---------------------------------

/// Spawning a new thread triggers `thread_heap()` on its first allocation.
/// `bind_thread_to_node()` allocates via `std::fs::read_to_string`, which
/// re-enters `alloc` → `thread_heap()`.  If the TLS slot is not set before
/// `bind_thread_to_node`, this recurses infinitely (stack overflow).
#[test]
fn thread_heap_init_no_recursion() {
    let handle = std::thread::spawn(|| {
        // Force an allocation on the new thread.
        let v: Vec<u32> = (0..256).collect();
        assert_eq!(v.len(), 256);
        v.into_iter().sum::<u32>()
    });
    let sum = handle.join().unwrap();
    assert_eq!(sum, (0u32..256).sum());
}

/// Spawn many threads concurrently to stress-test the per-thread heap
/// initialisation path.  Each thread allocates immediately, exercising the
/// TLS registration + `bind_thread_to_node` ordering.
#[test]
fn concurrent_thread_heap_init() {
    let handles: Vec<_> = (0..16)
        .map(|i| {
            std::thread::spawn(move || {
                // Allocate a String (multiple allocs: buffer + metadata).
                let s = format!("thread {i} reporting in");
                assert!(s.contains(&i.to_string()));

                // Allocate a Vec and fill it.
                let v: Vec<u8> = vec![i as u8; 128];
                assert!(v.iter().all(|&b| b == i as u8));
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

// -- Allocations during init are correct -----------------------------------

/// Memory allocated through the system-allocator fallback (during heap init)
/// and memory from the NUMA heap (after init) must both be usable and
/// correctly freed.  This test does a variety of allocation patterns to
/// surface any misrouted dealloc calls.
#[test]
fn post_init_alloc_dealloc_correctness() {
    // Small allocations.
    for size in [8, 16, 32, 64, 128, 256, 512, 1024, 4096, 16384] {
        let mut v: Vec<u8> = vec![0; size];
        v.fill(0xCD);
        assert!(v.iter().all(|&b| b == 0xCD));
    }

    // Large allocation (> 256 KB, goes through mmap path).
    let big: Vec<u8> = vec![42; 512 * 1024];
    assert_eq!(big.len(), 512 * 1024);
    assert!(big.iter().all(|&b| b == 42));
}

/// Cross-thread deallocation: allocate on one thread, free on another.
/// Exercises the remote-dealloc path (push to origin node's Treiber stack)
/// under a real global allocator.
#[test]
fn cross_thread_dealloc_global_allocator() {
    let data: Vec<u8> = vec![0xFF; 256];
    let handle = std::thread::spawn(move || {
        // `data` is dropped (deallocated) on this thread, not the one
        // that allocated it.
        assert!(data.iter().all(|&b| b == 0xFF));
        // drop happens here
    });
    handle.join().unwrap();
}

/// Rapid thread churn: many short-lived threads each allocate and free.
/// Verifies that thread-exit cleanup (drain to per-node heap) works
/// correctly under the global allocator.
#[test]
fn rapid_thread_churn_global_allocator() {
    let handles: Vec<_> = (0..64)
        .map(|_| {
            std::thread::spawn(|| {
                let mut vecs: Vec<Vec<u8>> = Vec::new();
                for size in [16, 64, 256, 1024, 4096] {
                    vecs.push(vec![0xAA; size]);
                }
                for v in &vecs {
                    assert!(v.iter().all(|&b| b == 0xAA));
                }
                // All vecs dropped on thread exit.
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

/// Realloc through the global allocator (Vec growth pattern).
#[test]
fn realloc_via_vec_growth() {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..10_000 {
        v.push(i);
    }
    assert_eq!(v.len(), 10_000);
    assert_eq!(v[9999], 9999);
}
