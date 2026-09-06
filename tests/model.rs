//! Deterministic state-machine runs (PRNG-driven through the same
//! boundary-biased decoder the AFL harness uses) and differential checks
//! against the system allocator.

mod common;

use common::{Decoder, FillMode, Model, Op, Rng, encode};
use numalloc::NumaAlloc;
use std::alloc::System;

const SEEDS: u64 = if cfg!(miri) { 2 } else { 12 };
const BYTES: usize = if cfg!(miri) { 400 } else { 24_000 };

fn cap_sizes(ops: &[Op], cap: usize) -> Vec<Op> {
    ops.iter()
        .map(|&op| match op {
            Op::Alloc {
                slot,
                size,
                align,
                zeroed,
            } => Op::Alloc {
                slot,
                size: size.min(cap),
                align: align.min(4096),
                zeroed,
            },
            Op::Realloc { slot, new_size } => Op::Realloc {
                slot,
                new_size: new_size.min(cap),
            },
            Op::Replace { slot, size, align } => Op::Replace {
                slot,
                size: size.min(cap),
                align: align.min(4096),
            },
            Op::FillEmpty { size, align } => Op::FillEmpty {
                size: size.min(cap),
                align: align.min(4096),
            },
            Op::Churn {
                slot,
                size,
                align,
                cycles,
            } => Op::Churn {
                slot,
                size: size.min(cap),
                align: align.min(4096),
                cycles,
            },
            Op::Huge { .. } => Op::CheckAll,
            other => other,
        })
        .collect()
}

#[test]
fn random_sequences_hold_invariants() {
    for seed in 1..=SEEDS {
        let alloc = NumaAlloc::with_config(1 + (seed as usize % 3), 1 << 20);
        let mut ops = Rng::new(seed).ops(64, BYTES);
        if cfg!(miri) {
            ops = cap_sizes(&ops, 16384);
        }
        let mut m = Model::new(&alloc, 64, FillMode::Sparse);
        m.run(&ops);
        m.check_all();
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
        drop(m);
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
    }
}

#[test]
fn random_sequences_full_fill_small_objects() {
    for seed in 100..100 + SEEDS {
        let alloc = NumaAlloc::with_config(2, 1 << 20);
        let ops = cap_sizes(
            &Rng::new(seed).ops(48, BYTES),
            if cfg!(miri) { 1024 } else { 8192 },
        );
        let mut m = Model::new(&alloc, 48, FillMode::Full);
        m.run(&ops);
        m.check_all();
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
    }
}

#[test]
fn differential_against_system_allocator() {
    // Same op stream on both allocators: every bounded request must succeed
    // on both, contents must survive, and the live sets must match.
    for seed in 200..200 + SEEDS {
        let ops = cap_sizes(
            &Rng::new(seed).ops(64, BYTES),
            if cfg!(miri) { 8192 } else { 4 << 20 },
        );
        let ops: Vec<Op> = ops.into_iter().filter(|op| *op != Op::Validate).collect();
        let alloc = NumaAlloc::with_config(2, 2 << 20);
        let mut ours = Model::new(&alloc, 64, FillMode::Sparse);
        let mut reference = Model::new(&System, 64, FillMode::Sparse);
        for &op in &ops {
            ours.apply(op);
            reference.apply(op);
            assert_eq!(ours.live_count(), reference.live_count(), "after {op:?}");
        }
        assert_eq!(ours.stats.allocs, reference.stats.allocs);
        assert_eq!(ours.stats.frees, reference.stats.frees);
        assert_eq!(ours.stats.reallocs, reference.stats.reallocs);
        ours.check_all();
        reference.check_all();
        // SAFETY: single-threaded test.
        unsafe { alloc.validate_internal_state() };
    }
}

#[test]
fn encoder_round_trips_through_decoder() {
    let ops = vec![
        Op::Alloc {
            slot: 3,
            size: 4097,
            align: 16,
            zeroed: false,
        },
        Op::Alloc {
            slot: 4,
            size: 262145,
            align: 4096,
            zeroed: true,
        },
        Op::Realloc {
            slot: 3,
            new_size: 1 << 20,
        },
        Op::Replace {
            slot: 4,
            size: 300,
            align: 1,
        },
        Op::FillEmpty {
            size: 64,
            align: 64,
        },
        Op::FreeAll {
            order: common::FreeOrder::EverySecond,
        },
        Op::Churn {
            slot: 1,
            size: 33,
            align: 8,
            cycles: 9,
        },
        Op::Touch { slot: 2 },
        Op::CheckAll,
        Op::Validate,
        Op::Huge {
            size: 1 << 50,
            align: 8,
        },
        Op::Free { slot: 3 },
    ];
    let bytes = encode(&ops, 64);
    let decoded = Decoder::new(&bytes).ops(64);
    assert_eq!(decoded, ops);
}

#[test]
fn decoder_never_panics_on_arbitrary_bytes() {
    let mut rng = Rng::new(7);
    for len in [0usize, 1, 2, 3, 5, 17, 256, 4096] {
        let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        let ops = Decoder::new(&bytes).ops(16);
        assert!(ops.len() <= common::MAX_OPS);
        for op in &ops {
            if let Op::Alloc { size, align, .. } = op {
                assert!(*size >= 1 && *size <= common::MAX_OP_SIZE);
                assert!(align.is_power_of_two() && *align <= 1 << common::MAX_ALIGN_SHIFT);
            }
        }
    }
}
