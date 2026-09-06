//! Regenerate the seed corpora from readable op lists:
//! `cargo run --bin gen_corpus` (from `fuzz/afl`).
use numalloc_afl::SLOTS;
use numalloc_afl::model::boundaries::{CLASS_SIZES, SMALL_LIMIT};
use numalloc_afl::model::{FreeOrder, Op, encode};
use std::path::Path;

fn alloc(slot: usize, size: usize, align: usize) -> Op {
    Op::Alloc {
        slot,
        size,
        align,
        zeroed: false,
    }
}

fn seeds() -> Vec<(&'static str, Vec<Op>)> {
    let mut v = Vec::new();
    v.push(("basic", vec![alloc(0, 64, 8), Op::Free { slot: 0 }]));
    v.push((
        "two_live",
        vec![alloc(0, 64, 8), alloc(1, 64, 8), Op::CheckAll, Op::Free { slot: 0 }, Op::Free { slot: 1 }],
    ));
    v.push((
        "free_realloc_same",
        vec![alloc(0, 128, 8), Op::Free { slot: 0 }, alloc(0, 128, 8), Op::Validate, Op::Free { slot: 0 }],
    ));
    let mut classes = Vec::new();
    for (i, &c) in CLASS_SIZES.iter().enumerate() {
        classes.push(alloc(i, c, 8));
    }
    classes.push(Op::CheckAll);
    classes.push(Op::Validate);
    classes.push(Op::FreeAll { order: FreeOrder::Fifo });
    v.push(("all_classes", classes));
    let mut edges = Vec::new();
    for (i, &c) in CLASS_SIZES.iter().enumerate() {
        edges.push(alloc(3 * i % SLOTS, c - 1, 8));
        edges.push(alloc((3 * i + 1) % SLOTS, c, 8));
        edges.push(alloc((3 * i + 2) % SLOTS, c + 1, 8));
    }
    edges.push(Op::Validate);
    edges.push(Op::FreeAll { order: FreeOrder::EverySecond });
    v.push(("class_edges", edges));
    let mut aligns = Vec::new();
    for (i, shift) in (0..=22).enumerate() {
        aligns.push(alloc(i, 24, 1 << shift));
    }
    aligns.push(Op::CheckAll);
    aligns.push(Op::FreeAll { order: FreeOrder::Lifo });
    v.push(("mixed_aligns", aligns));
    let mut frag = Vec::new();
    frag.push(Op::FillEmpty { size: 48, align: 8 });
    for i in (0..SLOTS).step_by(2) {
        frag.push(Op::Free { slot: i });
    }
    for i in (0..SLOTS).step_by(2) {
        frag.push(alloc(i, 4096, 4096));
    }
    frag.push(Op::Validate);
    frag.push(Op::FreeAll { order: FreeOrder::Fifo });
    v.push(("fragmentation", frag));
    v.push((
        "grow_shrink",
        vec![
            alloc(0, 8, 8),
            Op::Realloc { slot: 0, new_size: 9 },
            Op::Realloc { slot: 0, new_size: 4097 },
            Op::Realloc { slot: 0, new_size: SMALL_LIMIT },
            Op::Realloc { slot: 0, new_size: SMALL_LIMIT + 1 },
            Op::Realloc { slot: 0, new_size: 1 << 20 },
            Op::Realloc { slot: 0, new_size: 4095 },
            Op::Realloc { slot: 0, new_size: 1 },
            Op::Free { slot: 0 },
        ],
    ));
    v.push((
        "zeroed",
        vec![
            alloc(0, 300, 8),
            Op::Free { slot: 0 },
            Op::Alloc { slot: 0, size: 300, align: 8, zeroed: true },
            Op::Alloc { slot: 1, size: SMALL_LIMIT + 4096, align: 4096, zeroed: true },
            Op::Free { slot: 1 },
            Op::Alloc { slot: 1, size: SMALL_LIMIT + 1, align: 8, zeroed: true },
            Op::FreeAll { order: FreeOrder::Lifo },
        ],
    ));
    v.push((
        "churn",
        vec![
            Op::Churn { slot: 0, size: 8, align: 8, cycles: 64 },
            Op::Churn { slot: 1, size: 32768, align: 8, cycles: 16 },
            Op::Churn { slot: 2, size: SMALL_LIMIT + 1, align: 8, cycles: 8 },
            Op::Validate,
        ],
    ));
    v.push((
        "exhaust_region",
        vec![
            Op::FillEmpty { size: 32768, align: 8 },
            Op::Validate,
            Op::FreeAll { order: FreeOrder::EverySecond },
            Op::FillEmpty { size: SMALL_LIMIT, align: 8 },
            Op::FreeAll { order: FreeOrder::Lifo },
            Op::Validate,
        ],
    ));
    v.push((
        "large_cache_close_size",
        vec![
            alloc(0, SMALL_LIMIT + 4 * 4096, 8),
            Op::Free { slot: 0 },
            alloc(0, SMALL_LIMIT + 4096, 8),
            Op::Touch { slot: 0 },
            Op::Free { slot: 0 },
            alloc(1, SMALL_LIMIT + 4 * 4096, 8),
            Op::Free { slot: 1 },
        ],
    ));
    v.push((
        "huge",
        vec![
            alloc(0, 100, 8),
            Op::Huge { size: isize::MAX as usize, align: 8 },
            Op::Huge { size: 1 << 50, align: 4096 },
            Op::Huge { size: usize::MAX - 4096, align: 1 << 22 },
            Op::CheckAll,
            Op::Free { slot: 0 },
        ],
    ));
    v.push((
        "long_lived_mix",
        vec![
            alloc(0, 1 << 20, 8),
            alloc(1, 16, 16),
            Op::Churn { slot: 2, size: 64, align: 8, cycles: 32 },
            Op::Churn { slot: 3, size: 8192, align: 8, cycles: 32 },
            Op::Touch { slot: 0 },
            Op::CheckAll,
            Op::Realloc { slot: 1, new_size: 17 },
            Op::FreeAll { order: FreeOrder::Fifo },
        ],
    ));
    v
}

/// Thread-target seeds: (header, threads byte, [(thread, op)]).
fn thread_seeds() -> Vec<(&'static str, u8, u8, Vec<(u8, Op)>)> {
    let mut v = Vec::new();
    v.push((
        "transfer_pair",
        1,
        0,
        vec![
            (0, alloc(0, 64, 8)),
            (0, Op::Transfer { slot: 0, to: 1 }),
            (1, alloc(1, 4096, 4096)),
            (1, Op::Transfer { slot: 1, to: 0 }),
            (0, Op::CheckAll),
            (1, Op::CheckAll),
        ],
    ));
    let mut ring = Vec::new();
    for t in 0..4u8 {
        for (i, &c) in CLASS_SIZES.iter().enumerate() {
            ring.push((t, alloc(i, c, 8)));
        }
        for i in 0..CLASS_SIZES.len() {
            ring.push((t, Op::Transfer { slot: i, to: (t as usize + 1) % 4 }));
        }
        ring.push((t, Op::CheckAll));
    }
    v.push(("ring_all_classes", 3, 2, ring));
    let mut churn = Vec::new();
    for t in 0..3u8 {
        churn.push((t, Op::Churn { slot: 0, size: 64, align: 8, cycles: 64 }));
        churn.push((t, Op::FillEmpty { size: 64, align: 8 }));
        churn.push((t, Op::Transfer { slot: 5, to: 1 }));
        churn.push((t, Op::FreeAll { order: FreeOrder::Lifo }));
    }
    v.push(("shared_class_churn", 0, 1, churn));
    let mut exhaust = Vec::new();
    for t in 0..2u8 {
        exhaust.push((t, Op::FillEmpty { size: 32768, align: 8 }));
        exhaust.push((t, Op::Transfer { slot: 3, to: 1 - t as usize }));
        exhaust.push((t, Op::FreeAll { order: FreeOrder::EverySecond }));
        exhaust.push((t, Op::FillEmpty { size: SMALL_LIMIT + 1, align: 8 }));
        exhaust.push((t, Op::FreeAll { order: FreeOrder::Fifo }));
    }
    v.push(("exhaust_two_threads", 0, 0, exhaust));
    v
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    let ops_dir = root.join("alloc_ops");
    let thr_dir = root.join("alloc_threads");
    std::fs::create_dir_all(&ops_dir).unwrap();
    std::fs::create_dir_all(&thr_dir).unwrap();
    for (name, ops) in seeds() {
        // Header byte: 2 nodes, 512 KiB regions.
        let mut bytes = vec![0b0101u8];
        bytes.extend(encode(&ops, SLOTS));
        std::fs::write(ops_dir.join(format!("{name}.bin")), bytes).unwrap();
    }
    for (name, header, tb, ops) in thread_seeds() {
        let mut bytes = vec![header, tb];
        for (t, op) in ops {
            bytes.push(t);
            bytes.extend(encode(std::slice::from_ref(&op), SLOTS));
        }
        std::fs::write(thr_dir.join(format!("{name}.bin")), bytes).unwrap();
    }
    println!("wrote seeds to {}", root.display());
}
