//! Engine-neutral fuzz target bodies.  The AFL binaries in `src/bin/` are
//! thin adapters; `tests/replay.rs` runs the same bodies over the corpus and
//! regression inputs under plain `cargo test`.
//!
//! Both targets build a *fresh* allocator per iteration (`with_config`), so
//! persistent-mode iterations never see state from an earlier input and any
//! crash reproduces from its input alone.

#[path = "../../../tests/common/mod.rs"]
pub mod model;

use model::{Decoder, FillMode, Model, Op, Owned, Rng};
use numalloc::NumaAlloc;
use std::sync::mpsc::{Receiver, Sender, channel};

/// Slots per model.
pub const SLOTS: usize = 64;
/// Upper bound on allocations per iteration, so `FillEmpty`/`Churn` chains
/// cannot turn one input into seconds of mmap traffic.
pub const ALLOC_BUDGET: usize = 20_000;
/// Upper bound on `Validate` ops per iteration.  Each one walks all metadata
/// with its own bookkeeping mapping, so a repeated-byte mutation producing
/// thousands of them exceeds AFL's timeout and is misfiled as a hang.
pub const VALIDATE_BUDGET: usize = 64;

/// Topology from one header byte: 1-4 virtual nodes, 256 KiB - 2 MiB regions.
/// Small regions make exhaustion (mmap fallback) reachable in a few ops.
fn topology(b: u8) -> (usize, usize) {
    let nodes = 1 + (b & 3) as usize;
    let region = (256 * 1024) << ((b >> 2) & 3);
    (nodes, region)
}

/// Sequential state-machine target.
pub fn fuzz_ops(data: &[u8]) {
    let mut dec = Decoder::new(data);
    let Some(header) = dec.byte() else { return };
    let (nodes, region) = topology(header);
    let ops = dec.ops(SLOTS);
    let alloc = NumaAlloc::with_config(nodes, region);
    let mut m = Model::new(&alloc, SLOTS, FillMode::Sparse);
    let mut validates = 0;
    for &op in &ops {
        if op == Op::Validate {
            validates += 1;
            if validates > VALIDATE_BUDGET {
                continue;
            }
        }
        m.apply(op);
        if m.stats.allocs > ALLOC_BUDGET {
            break;
        }
    }
    m.check_all();
    // SAFETY: single-threaded.
    unsafe { alloc.validate_internal_state() };
    drop(m);
    // SAFETY: single-threaded; all blocks are back on the freelists.
    unsafe { alloc.validate_internal_state() };
}

/// Multi-threaded target: 2-4 worker threads each drive their own model on
/// one shared allocator, transferring live allocations over channels.
/// Each thread only ever checks memory it owns, so the oracle is race-free
/// even though thread interleaving is not deterministic.
pub fn fuzz_threads(data: &[u8]) {
    let mut dec = Decoder::new(data);
    let Some(header) = dec.byte() else { return };
    let Some(tb) = dec.byte() else { return };
    let (nodes, region) = topology(header);
    let nthreads = 2 + (tb & 3) as usize;
    // Each op is prefixed by a thread byte.
    let mut per_thread: Vec<Vec<Op>> = vec![Vec::new(); nthreads];
    let mut total = 0;
    while total < model::MAX_OPS {
        let Some(t) = dec.byte() else { break };
        let Some(op) = dec.op(SLOTS) else { break };
        if op == Op::Validate {
            continue; // never valid while other threads run
        }
        per_thread[t as usize % nthreads].push(op);
        total += 1;
    }
    let alloc = NumaAlloc::with_config(nodes, region);
    let (txs, rxs): (Vec<Sender<Owned>>, Vec<Receiver<Owned>>) =
        (0..nthreads).map(|_| channel()).unzip();
    std::thread::scope(|s| {
        for (t, rx) in rxs.into_iter().enumerate() {
            let ops = std::mem::take(&mut per_thread[t]);
            let txs = txs.clone();
            let alloc = &alloc;
            s.spawn(move || {
                let mut rng = Rng::new(t as u64 + 1);
                let mut m = Model::new(alloc, SLOTS, FillMode::Sparse);
                for &op in &ops {
                    match op {
                        Op::Transfer { slot, to } => {
                            if let Some(owned) = m.take(slot) {
                                let _ = txs[to % nthreads].send(owned);
                            }
                        }
                        other => m.apply(other),
                    }
                    while let Ok(owned) = rx.try_recv() {
                        if rng.below(2) == 0 {
                            m.adopt(rng.below(SLOTS), owned);
                        } else {
                            m.release(owned);
                        }
                    }
                    if m.stats.allocs > ALLOC_BUDGET / nthreads {
                        break;
                    }
                }
                drop(txs);
                while let Ok(owned) = rx.recv() {
                    m.release(owned);
                }
                m.check_all();
            });
        }
        drop(txs);
    });
    // SAFETY: every worker has been joined by the scope.
    unsafe { alloc.validate_internal_state() };
}

/// Read all of stdin (used by the replay path of the binaries).
pub fn read_stdin() -> Vec<u8> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf).expect("read stdin");
    buf
}
