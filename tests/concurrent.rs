//! Concurrent stress: shared allocator across threads, cross-thread ownership
//! transfer, per-thread cache teardown and shared freelist contention.
//! Every check is on memory owned by the checking thread, so any failure is
//! an allocator bug rather than harness racing.

mod common;

use common::boundaries::CLASS_SIZES;
use common::{FillMode, FreeOrder, Model, Op, Owned, Rng};
use numalloc::NumaAlloc;
use std::alloc::{GlobalAlloc, Layout};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Barrier};

const THREADS: usize = if cfg!(miri) { 3 } else { 8 };
const OPS_BYTES: usize = if cfg!(miri) { 600 } else { 40_000 };

fn no_validate(ops: Vec<Op>) -> Vec<Op> {
    ops.into_iter().filter(|op| *op != Op::Validate).collect()
}

fn drain_inbox<A: common::Harness>(m: &mut Model<'_, A>, rx: &Receiver<Owned>, rng: &mut Rng) {
    while let Ok(owned) = rx.try_recv() {
        if rng.below(2) == 0 {
            let slot = rng.below(m.nslots());
            m.adopt(slot, owned);
        } else {
            m.release(owned);
        }
    }
}

#[test]
fn threaded_models_with_ownership_transfer() {
    static ALLOC: NumaAlloc = NumaAlloc::with_config(4, 2 << 20);
    let (txs, rxs): (Vec<Sender<Owned>>, Vec<Receiver<Owned>>) =
        (0..THREADS).map(|_| channel()).unzip();
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = rxs
        .into_iter()
        .enumerate()
        .map(|(t, rx)| {
            let txs = txs.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut rng = Rng::new(1000 + t as u64);
                let ops = no_validate(rng.ops(32, OPS_BYTES));
                let mut m = Model::new(&ALLOC, 32, FillMode::Sparse);
                barrier.wait();
                for (i, &op) in ops.iter().enumerate() {
                    m.apply(op);
                    drain_inbox(&mut m, &rx, &mut rng);
                    if i % 8 == 0
                        && let Some(owned) = m.take(rng.below(32))
                    {
                        let to = rng.below(THREADS);
                        let _ = txs[to].send(owned);
                    }
                }
                drop(txs);
                // Everything still in flight to us is ours to free.
                while let Ok(owned) = rx.recv() {
                    m.release(owned);
                }
                m.check_all();
                m.free_all(FreeOrder::EverySecond);
            })
        })
        .collect();
    drop(txs);
    for h in handles {
        h.join().unwrap();
    }
    // SAFETY: all worker threads have exited.
    unsafe { ALLOC.validate_internal_state() };
}

#[test]
fn short_lived_threads_tear_down_caches() {
    static ALLOC: NumaAlloc = NumaAlloc::with_config(2, 2 << 20);
    let (tx, rx) = channel::<Owned>();
    let rounds = if cfg!(miri) { 4 } else { 200 };
    let mut main = Model::new(&ALLOC, 64, FillMode::Sparse);
    for round in 0..rounds {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut m = Model::new(&ALLOC, 64, FillMode::Sparse);
            for i in 0..64 {
                let size = CLASS_SIZES[(round + i) % CLASS_SIZES.len()];
                let size = if cfg!(miri) { size.min(8192) } else { size };
                m.alloc(i, size + (i % 2), 8, false);
            }
            for i in (0..64).step_by(2) {
                m.free(i);
            }
            for i in (1..64).step_by(2) {
                tx.send(m.take(i).unwrap()).unwrap();
            }
            // Blocks freed here go through the exiting thread's cache and
            // are drained to the node stacks by the TLS destructor.
            m.alloc(0, 8, 8, false);
        })
        .join()
        .unwrap();
        // Free the transferred blocks after their allocating thread is gone.
        let mut rng = Rng::new(round as u64);
        drain_inbox(&mut main, &rx, &mut rng);
        if round % 16 == 0 {
            main.check_all();
            // SAFETY: no other thread is alive.
            unsafe { ALLOC.validate_internal_state() };
        }
    }
    drop(tx);
    while let Ok(o) = rx.try_recv() {
        main.release(o);
    }
    main.free_all(FreeOrder::Fifo);
    // SAFETY: no other thread is alive.
    unsafe { ALLOC.validate_internal_state() };
}

#[test]
fn producers_and_consumers_hammer_node_stacks() {
    static ALLOC: NumaAlloc = NumaAlloc::with_config(2, 2 << 20);
    let producers = THREADS / 2;
    let consumers = THREADS - producers;
    let per_producer = if cfg!(miri) { 60 } else { 20_000 };
    let (tx, rx) = channel::<Owned>();
    let rx = Arc::new(std::sync::Mutex::new(rx));
    let mut handles = Vec::new();
    for p in 0..producers {
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            let mut m = Model::new(&ALLOC, 16, FillMode::Sparse);
            let mut rng = Rng::new(77 + p as u64);
            for i in 0..per_producer {
                let size = CLASS_SIZES[rng.below(if cfg!(miri) { 10 } else { 13 })] - (i % 3);
                m.alloc(i % 16, size, 8, false);
                tx.send(m.take(i % 16).unwrap()).unwrap();
            }
        }));
    }
    drop(tx);
    for c in 0..consumers {
        let rx = rx.clone();
        handles.push(std::thread::spawn(move || {
            let mut m = Model::new(&ALLOC, 8, FillMode::Sparse);
            let mut rng = Rng::new(99 + c as u64);
            loop {
                let msg = rx.lock().unwrap().recv();
                match msg {
                    Ok(owned) => {
                        if rng.below(4) == 0 {
                            m.adopt(rng.below(8), owned);
                        } else {
                            m.release(owned);
                        }
                    }
                    Err(_) => break,
                }
            }
            m.check_all();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // SAFETY: all worker threads have exited.
    unsafe { ALLOC.validate_internal_state() };
}

#[test]
fn shared_freelist_contention_single_class() {
    static ALLOC: NumaAlloc = NumaAlloc::with_config(1, 4 << 20);
    let bursts = if cfg!(miri) { 3 } else { 40 };
    let burst = if cfg!(miri) { 40 } else { 3000 };
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut m = Model::new(&ALLOC, burst, FillMode::Sparse);
                barrier.wait();
                for b in 0..bursts {
                    for i in 0..burst {
                        m.alloc(i, 64, 8, false);
                    }
                    m.check_all();
                    m.free_all(if (b + t) % 2 == 0 {
                        FreeOrder::Fifo
                    } else {
                        FreeOrder::Lifo
                    });
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // SAFETY: all worker threads have exited.
    unsafe { ALLOC.validate_internal_state() };
}

#[test]
fn allocation_during_tls_destruction() {
    // Allocating and freeing from a TLS destructor exercises the path where
    // the per-thread cache may already be gone (`thread_heap() == None`).
    static ALLOC: NumaAlloc = NumaAlloc::with_config(2, 1 << 20);
    struct LateUser(Vec<(usize, usize, usize)>);
    impl Drop for LateUser {
        fn drop(&mut self) {
            let layout = Layout::from_size_align(96, 8).unwrap();
            // SAFETY: standard alloc/dealloc pairing with matching layouts.
            unsafe {
                for &(addr, size, align) in &self.0 {
                    ALLOC.dealloc(
                        addr as *mut u8,
                        Layout::from_size_align(size, align).unwrap(),
                    );
                }
                let p = ALLOC.alloc(layout);
                assert!(!p.is_null());
                p.write_bytes(0x7e, 96);
                ALLOC.dealloc(p, layout);
                let big = Layout::from_size_align(300_000, 8).unwrap();
                let q = ALLOC.alloc(big);
                assert!(!q.is_null());
                q.write_bytes(0x11, 64);
                ALLOC.dealloc(q, big);
            }
        }
    }
    thread_local! {
        static LATE: std::cell::RefCell<Option<LateUser>> = const { std::cell::RefCell::new(None) };
    }
    for _ in 0..(if cfg!(miri) { 2 } else { 32 }) {
        std::thread::spawn(|| {
            let mut m = Model::new(&ALLOC, 8, FillMode::Full);
            for (i, &size) in CLASS_SIZES.iter().enumerate().take(8) {
                m.alloc(i, size, 8, false);
            }
            // Hand half of the live blocks to the TLS destructor to free late.
            let raw: Vec<(usize, usize, usize)> =
                (0..4).map(|i| m.take(i).unwrap().into_raw()).collect();
            LATE.with(|l| *l.borrow_mut() = Some(LateUser(raw)));
        })
        .join()
        .unwrap();
    }
    // SAFETY: all worker threads have exited.
    unsafe { ALLOC.validate_internal_state() };
}

#[test]
fn spare_chain_is_consumed_and_returned_on_thread_exit() {
    // A refill detaches the whole node stack but walks only 64 blocks; the
    // rest is parked as the thread's spare chain.  It must be consumed by
    // later refills, survive validation, and go back to the node stack when
    // the thread exits (no block lost, none duplicated).
    static ALLOC: NumaAlloc = NumaAlloc::with_config(1, 4 << 20);
    let n = if cfg!(miri) { 300 } else { 6000 };
    // Thread A: fill the node stack with thousands of 64-byte blocks.
    std::thread::spawn(move || {
        let mut m = Model::new(&ALLOC, n, FillMode::Full);
        for i in 0..n {
            m.alloc(i, 64, 8, false);
        }
        m.free_all(FreeOrder::Fifo);
    })
    .join()
    .unwrap();
    // SAFETY: no other thread is alive.
    unsafe { ALLOC.validate_internal_state() };
    // Thread B: one allocation takes the whole chain, keeps 64, parks the
    // rest; a few hundred more allocations must be served from the spare.
    std::thread::spawn(move || {
        let mut m = Model::new(&ALLOC, 300, FillMode::Full);
        m.alloc(0, 64, 8, false);
        // SAFETY: single active thread.
        unsafe { ALLOC.validate_internal_state() };
        for i in 1..300.min(n) {
            m.alloc(i, 64, 8, false);
            assert!(ALLOC.owns(m.ptr(i).unwrap()), "spare not reused");
        }
        m.check_all();
        // Leave some blocks live for the main thread to free after exit.
        let raw: Vec<(usize, usize, usize)> =
            (0..8).map(|i| m.take(i).unwrap().into_raw()).collect();
        raw
    })
    .join()
    .unwrap()
    .into_iter()
    .for_each(|(addr, size, align)| {
        // SAFETY: live blocks from thread B with their exact layouts.
        unsafe {
            ALLOC.dealloc(
                addr as *mut u8,
                Layout::from_size_align(size, align).unwrap(),
            )
        };
    });
    // SAFETY: no other thread is alive.
    unsafe { ALLOC.validate_internal_state() };
    // Everything must be reusable from the region without new bags: the
    // bump pointer is unchanged by a full re-allocation of `n` blocks.
    let mut m = Model::new(&ALLOC, n, FillMode::Sparse);
    for i in 0..n {
        m.alloc(i, 64, 8, false);
        assert!(ALLOC.owns(m.ptr(i).unwrap()));
    }
    m.free_all(FreeOrder::Lifo);
    // SAFETY: no other thread is alive.
    unsafe { ALLOC.validate_internal_state() };
}
