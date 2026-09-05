//! Engine-neutral fuzz target bodies for the `numalloc` allocator.
//!
//! Each public `fn(&[u8])` here is a complete target: it decodes the input
//! into a bounded program of allocator operations, runs that program against
//! a process-wide [`NumaAlloc`] instance, and panics when an oracle fails.
//! The AFL++ binaries in `src/bin/` are thin adapters; the regression tests
//! in `tests/` replay saved inputs through the same functions.
//!
//! # Oracles
//!
//! * `alloc`/`alloc_zeroed`/`realloc` never return null for bounded sizes.
//! * Returned pointers honour `Layout::align()`.
//! * A new allocation never overlaps a live one (interval check).
//! * Every live allocation keeps its fill pattern until it is freed
//!   (detects double-hand-out, freelist metadata written into live memory,
//!   and cross-allocation corruption).
//! * `alloc_zeroed` returns all-zero memory.
//! * `realloc` preserves `min(old, new)` bytes.
//!
//! # Determinism
//!
//! The allocator instance is a `static` shared across persistent-mode
//! iterations. Every target body starts by calling `NumaAlloc::fuzz_reset`
//! (a `fuzz-hooks`-only seam) so freelists, bump pointers, the large-object
//! cache and round-robin node assignment are pristine: the same input always
//! runs the same paths, and a finding replays from that one input. Bodies
//! are serialised with a mutex so replaying several inputs from parallel
//! test threads stays sound. The `fuzz-hooks` feature also shrinks the region
//! and forces two virtual NUMA nodes so that region exhaustion and remote
//! deallocation are reachable within a single iteration.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::{Mutex, MutexGuard, Once};

use numalloc::NumaAlloc;

/// Process-wide allocator under test (shared across iterations, see module docs).
static ALLOC: NumaAlloc = NumaAlloc::new();

/// Maximum operations executed per iteration. Large enough that 1024+
/// distinct-size large alloc/free pairs can overflow the 1024-slot large
/// cache and reach its eviction path.
pub const MAX_OPS: usize = 4096;
/// Maximum simultaneously live allocations per program.
pub const MAX_LIVE: usize = 1024;
/// Largest single allocation the harness will request (2 MiB).
pub const MAX_ALLOC: usize = 2 << 20;
/// Cap on bytes held live at once per program (32 MiB, above the 16 MiB
/// fuzz region so exhaustion is reachable).
pub const MAX_LIVE_BYTES: usize = 32 << 20;

/// Virtual NUMA nodes the allocator is told about under `fuzz-hooks`.
const FUZZ_NODES: &str = "2";
/// Per-node region size in MiB under `fuzz-hooks` (exhaustion reachable
/// within one iteration because `MAX_LIVE_BYTES` exceeds it).
const FUZZ_REGION_MB: &str = "16";

const ALIGNS: [usize; 16] = [
    1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 4096, 8192, 65536, 262144, 524288,
];

static ENV_INIT: Once = Once::new();

/// Serialises target bodies: `fuzz_reset` requires that no other thread is
/// using the allocator (relevant when tests replay inputs in parallel).
static BODY_LOCK: Mutex<()> = Mutex::new(());

/// Take the body lock and reset the allocator. Must be held for the whole
/// body; all worker threads must be joined before it is released.
fn begin_iteration() -> MutexGuard<'static, ()> {
    ensure_env();
    let guard = BODY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the lock guarantees no other body is running, every previous
    // body freed all its allocations and joined its threads.
    unsafe { ALLOC.fuzz_reset() };
    guard
}

/// Configure the allocator's fuzz seams before its first allocation.
///
/// Called at the start of every target; the work happens once per process.
fn ensure_env() {
    ENV_INIT.call_once(|| {
        for (key, val) in [
            ("NUMALLOC_FUZZ_NODES", FUZZ_NODES),
            ("NUMALLOC_FUZZ_REGION_MB", FUZZ_REGION_MB),
        ] {
            if std::env::var_os(key).is_none() {
                // SAFETY: called exactly once, before any worker thread of
                // the harness exists, so no concurrent env access.
                unsafe { std::env::set_var(key, val) };
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Input decoding
// ---------------------------------------------------------------------------

/// One decoded operation. Each op is 4 input bytes: `[opcode, a, b, c]`.
#[derive(Clone, Copy, Debug)]
enum Op {
    Alloc { layout: Layout },
    AllocZeroed { layout: Layout },
    Dealloc { idx: usize },
    Realloc { idx: usize, new_size: usize },
    Verify { idx: usize },
}

/// Map two bytes to an allocation size in `1..=MAX_ALLOC`, biased towards
/// size-class boundaries (2^k, 2^k ± 1, 2^k + small, 1.5·2^k) plus small
/// and page-multiple values.
fn decode_size(a: u8, b: u8) -> usize {
    let exp = u32::from(a & 0x1F).min(21);
    let base = 1usize << exp;
    let fine = usize::from(b);
    let size = match a >> 5 {
        0 => base,
        1 => base - 1,
        2 => base + 1,
        3 => base + fine,
        4 => base.saturating_sub(fine),
        5 => fine + 1,
        6 => fine * 1024 + 1,
        _ => base + base / 2,
    };
    size.clamp(1, MAX_ALLOC)
}

fn decode_align(c: u8) -> usize {
    ALIGNS[usize::from(c & 0x0F)]
}

fn decode_op(chunk: &[u8; 4]) -> Op {
    let [op, a, b, c] = *chunk;
    let idx = usize::from(a) | (usize::from(c) << 8);
    match op % 5 {
        0 | 1 => {
            let size = decode_size(a, b);
            let align = decode_align(c);
            // All sizes ≤ 2 MiB and aligns ≤ 512 KiB, so this cannot fail.
            let layout = Layout::from_size_align(size, align).expect("harness layout");
            if op % 5 == 0 {
                Op::Alloc { layout }
            } else {
                Op::AllocZeroed { layout }
            }
        }
        2 => Op::Dealloc { idx },
        3 => Op::Realloc {
            idx: usize::from(a),
            new_size: decode_size(b, c),
        },
        _ => Op::Verify { idx },
    }
}

fn ops(data: &[u8]) -> impl Iterator<Item = Op> + '_ {
    data.as_chunks::<4>().0.iter().take(MAX_OPS).map(decode_op)
}

// ---------------------------------------------------------------------------
// Model of live allocations
// ---------------------------------------------------------------------------

/// A live allocation owned by the harness. The address is stored as `usize`
/// so the model is `Send` and can be handed to another thread.
#[derive(Clone, Copy, Debug)]
struct Live {
    addr: usize,
    layout: Layout,
    tag: u8,
}

impl Live {
    fn ptr(&self) -> *mut u8 {
        self.addr as *mut u8
    }
    fn end(&self) -> usize {
        self.addr + self.layout.size()
    }
}

/// Allocation model: live set plus byte accounting and a tag counter.
#[derive(Default)]
struct Model {
    live: Vec<Live>,
    live_bytes: usize,
    next_tag: u8,
}

impl Model {
    fn new() -> Self {
        Self {
            live: Vec::with_capacity(MAX_LIVE),
            live_bytes: 0,
            next_tag: 0,
        }
    }

    /// Fresh non-zero fill byte. Non-zero so a stale freelist `next`
    /// pointer (which starts with zero bytes on most addresses) or a bogus
    /// zeroing is distinguishable from the pattern.
    fn tag(&mut self) -> u8 {
        self.next_tag = self.next_tag.wrapping_add(1);
        self.next_tag | 1
    }

    fn check_new_block(&self, ptr: *mut u8, layout: Layout, what: &str) {
        assert!(!ptr.is_null(), "{what}: returned null for {layout:?}");
        let addr = ptr as usize;
        assert_eq!(
            addr % layout.align(),
            0,
            "{what}: pointer {addr:#x} violates alignment {}",
            layout.align()
        );
        let end = addr + layout.size();
        for l in &self.live {
            assert!(
                end <= l.addr || addr >= l.end(),
                "{what}: new block [{addr:#x}, {end:#x}) ({layout:?}) overlaps live block \
                 [{:#x}, {:#x}) ({:?})",
                l.addr,
                l.end(),
                l.layout
            );
        }
    }

    fn record(&mut self, ptr: *mut u8, layout: Layout, tag: u8) {
        self.live.push(Live {
            addr: ptr as usize,
            layout,
            tag,
        });
        self.live_bytes += layout.size();
    }

    fn can_hold(&self, layout: Layout) -> bool {
        self.live.len() < MAX_LIVE && self.live_bytes + layout.size() <= MAX_LIVE_BYTES
    }

    fn take(&mut self, idx: usize) -> Option<Live> {
        if self.live.is_empty() {
            return None;
        }
        let l = self.live.swap_remove(idx % self.live.len());
        self.live_bytes -= l.layout.size();
        Some(l)
    }
}

/// Byte ranges of a block that the harness writes and checks.
///
/// The whole block for small blocks; for big ones the first and last
/// `PROBE_EDGE` bytes plus one byte every `PROBE_STRIDE`. Cross-allocation
/// overlap is caught exactly by the interval check in `check_new_block`, and
/// the allocator only ever writes metadata at a block's start (`FreeBlock`)
/// or just before it (`LargeHeader`), so probing the edges keeps the oracle
/// sharp while avoiding page-faulting every page of every large block.
fn probe_ranges(len: usize) -> impl Iterator<Item = (usize, usize)> {
    const PROBE_EDGE: usize = 4096;
    const PROBE_STRIDE: usize = 64 * 1024;
    let whole = len <= 2 * PROBE_EDGE;
    let head = if whole { (0, len) } else { (0, PROBE_EDGE) };
    let tail = if whole {
        None
    } else {
        Some((len - PROBE_EDGE, len))
    };
    let stride = if whole {
        0..0
    } else {
        (PROBE_EDGE / PROBE_STRIDE + 1)..(len - PROBE_EDGE).div_ceil(PROBE_STRIDE)
    };
    std::iter::once(head)
        .chain(tail)
        .chain(stride.map(|k| (k * PROBE_STRIDE, k * PROBE_STRIDE + 1)))
}

fn fill(ptr: *mut u8, len: usize, tag: u8) {
    for (lo, hi) in probe_ranges(len) {
        // SAFETY: caller guarantees `ptr` is a live allocation of at least
        // `len` bytes and `lo..hi` lies within it.
        unsafe { std::ptr::write_bytes(ptr.add(lo), tag, hi - lo) };
    }
}

fn check_bytes(ptr: *const u8, len: usize, expect: u8, what: &str) {
    for (lo, hi) in probe_ranges(len) {
        // SAFETY: as in `fill`.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.add(lo), hi - lo) };
        if let Some(pos) = bytes.iter().position(|&b| b != expect) {
            panic!(
                "{what}: byte {} of block {:#x} (len {len}) is {:#04x}, expected {expect:#04x}",
                lo + pos,
                ptr as usize,
                bytes[pos]
            );
        }
    }
}

/// After `realloc`, the first `keep` bytes must be preserved. Only the bytes
/// the harness actually wrote (the *old* block's probe ranges) are checked.
fn check_prefix(ptr: *const u8, old_len: usize, keep: usize, expect: u8) {
    for (lo, hi) in probe_ranges(old_len) {
        let hi = hi.min(keep);
        if lo >= hi {
            continue;
        }
        // SAFETY: `lo..hi` lies within the first `keep` bytes of the live
        // block returned by `realloc`.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.add(lo), hi - lo) };
        if let Some(pos) = bytes.iter().position(|&b| b != expect) {
            panic!(
                "realloc (preserved prefix): byte {} of block {:#x} (old len {old_len}, keep {keep}) \
                 is {:#04x}, expected {expect:#04x}",
                lo + pos,
                ptr as usize,
                bytes[pos]
            );
        }
    }
}

fn verify_live(l: &Live, what: &str) {
    check_bytes(l.ptr(), l.layout.size(), l.tag, what);
}

// ---------------------------------------------------------------------------
// Operation interpreter
// ---------------------------------------------------------------------------

/// Execute one operation against `ALLOC`, updating `model`.
fn step(model: &mut Model, op: Op) {
    match op {
        Op::Alloc { layout } => {
            if !model.can_hold(layout) {
                return;
            }
            // SAFETY: layout has non-zero size.
            let ptr = unsafe { ALLOC.alloc(layout) };
            model.check_new_block(ptr, layout, "alloc");
            let tag = model.tag();
            fill(ptr, layout.size(), tag);
            model.record(ptr, layout, tag);
        }
        Op::AllocZeroed { layout } => {
            if !model.can_hold(layout) {
                return;
            }
            // SAFETY: layout has non-zero size.
            let ptr = unsafe { ALLOC.alloc_zeroed(layout) };
            model.check_new_block(ptr, layout, "alloc_zeroed");
            check_bytes(ptr, layout.size(), 0, "alloc_zeroed");
            let tag = model.tag();
            fill(ptr, layout.size(), tag);
            model.record(ptr, layout, tag);
        }
        Op::Dealloc { idx } => {
            let Some(l) = model.take(idx) else { return };
            verify_live(&l, "dealloc (pattern check before free)");
            // SAFETY: `l` is live and was allocated with `l.layout`.
            unsafe { ALLOC.dealloc(l.ptr(), l.layout) };
        }
        Op::Realloc { idx, new_size } => {
            let Some(old) = model.take(idx) else { return };
            let Ok(new_layout) = Layout::from_size_align(new_size, old.layout.align()) else {
                // Put it back untouched; the request was not representable.
                model.live.push(old);
                model.live_bytes += old.layout.size();
                return;
            };
            if model.live_bytes + new_size > MAX_LIVE_BYTES {
                model.live.push(old);
                model.live_bytes += old.layout.size();
                return;
            }
            verify_live(&old, "realloc (pattern check before move)");
            // SAFETY: `old` is live with `old.layout`; new_size > 0 and
            // fits `Layout` with the same alignment.
            let ptr = unsafe { ALLOC.realloc(old.ptr(), old.layout, new_size) };
            model.check_new_block(ptr, new_layout, "realloc");
            let keep = old.layout.size().min(new_size);
            check_prefix(ptr, old.layout.size(), keep, old.tag);
            let tag = model.tag();
            fill(ptr, new_size, tag);
            model.record(ptr, new_layout, tag);
        }
        Op::Verify { idx } => {
            if let Some(l) = model.live.get(idx % model.live.len().max(1)) {
                verify_live(l, "verify");
            }
        }
    }
}

/// Verify and free everything still live.
fn teardown(model: &mut Model) {
    while let Some(l) = model.take(0) {
        verify_live(&l, "teardown (pattern check before free)");
        // SAFETY: `l` is live and was allocated with `l.layout`.
        unsafe { ALLOC.dealloc(l.ptr(), l.layout) };
    }
}

/// Run a whole program on the calling thread and return what is still live.
fn run_program(data: &[u8]) -> Model {
    let mut model = Model::new();
    for op in ops(data) {
        step(&mut model, op);
    }
    model
}

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// Sequential model target: a bounded program of alloc / alloc_zeroed /
/// dealloc / realloc / verify operations on one thread, then free all.
pub fn alloc_ops(data: &[u8]) {
    let _guard = begin_iteration();
    let mut model = run_program(data);
    teardown(&mut model);
}

/// Cross-thread target.
///
/// The input is split in two programs. Phase 1: two fresh threads each run
/// one program and hand back their live sets. Phase 2: two more fresh
/// threads each take the *other* worker's live set, apply their program's
/// realloc/dealloc/verify ops to it, then free everything. With two virtual
/// NUMA nodes and round-robin node assignment this drives the remote
/// deallocation path, thread-exit drains, and refill from per-node stacks.
///
/// Exactly four threads are spawned and each allocates at least once, so the
/// round-robin node assignment pattern is identical on every iteration.
pub fn alloc_threads(data: &[u8]) {
    let _guard = begin_iteration();
    let (p1, p2) = data.split_at(data.len() / 2);
    let (p1, p2) = (p1.to_vec(), p2.to_vec());

    // Phase 1: two workers allocate. Spawned one after another, each
    // registering its per-thread heap before the next starts, so node
    // assignment is 0, 1, 0, 1 on every run rather than racing.
    let h1 = spawn_registered(move || run_program(&p1));
    let h2 = spawn_registered(move || run_program(&p2));
    let m1 = h1.join().expect("phase-1 worker panicked");
    let m2 = h2.join().expect("phase-1 worker panicked");

    // Phase 2: two fresh workers each inherit the *other* worker's live set
    // (so every free is remote), mutate it per their program, then free all.
    let phase2 = |mut model: Model, prog: Vec<u8>| {
        spawn_registered(move || {
            for op in ops(&prog) {
                match op {
                    // Only mutate/free the inherited set; fresh allocations
                    // would just re-run phase 1 on this thread.
                    Op::Dealloc { .. } | Op::Realloc { .. } | Op::Verify { .. } => {
                        step(&mut model, op)
                    }
                    Op::Alloc { .. } | Op::AllocZeroed { .. } => {}
                }
            }
            teardown(&mut model);
        })
    };
    let (p1, p2) = data.split_at(data.len() / 2);
    let f1 = phase2(m2, p1.to_vec());
    let f2 = phase2(m1, p2.to_vec());
    f1.join().expect("phase-2 worker panicked");
    f2.join().expect("phase-2 worker panicked");
}

/// Spawn a worker and return only once it has registered its per-thread heap
/// (and thus consumed its round-robin node slot).
fn spawn_registered<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        touch();
        tx.send(()).expect("main thread waits for registration");
        f()
    });
    rx.recv().expect("worker registers before running");
    handle
}

/// Force the calling thread to register a per-thread heap (consumes one
/// round-robin node slot) so node assignment does not depend on the input.
fn touch() {
    let layout = Layout::from_size_align(8, 8).expect("static layout");
    // SAFETY: non-zero size; freed immediately with the same layout.
    unsafe {
        let p = ALLOC.alloc(layout);
        assert!(!p.is_null(), "touch: alloc returned null");
        ALLOC.dealloc(p, layout);
    }
}

/// Run once before entering the AFL++ persistent loop.
///
/// Pays every one-time cost (environment setup, heap mmap and topology
/// detection, the main thread's per-thread heap, the cached page size) so
/// those edges are not attributed to the first iteration and then reported
/// as unstable for the rest of the campaign.
pub fn warm_up() {
    // A tiny program touching the small, large and zeroed paths.
    let prog: &[u8] = &[
        0, 6, 0, 3, // alloc 64 B, align 8
        1, 18, 0, 3, // alloc_zeroed 256 KiB + 0 (large path)
        2, 0, 0, 0, // dealloc
        2, 0, 0, 0, // dealloc
    ];
    alloc_ops(prog);
}

// ---------------------------------------------------------------------------
// Replay helpers (used by tests and triage)
// ---------------------------------------------------------------------------

/// Look up a target body by binary name.
pub fn target_by_name(name: &str) -> Option<fn(&[u8])> {
    match name {
        "alloc_ops" => Some(alloc_ops),
        "alloc_threads" => Some(alloc_threads),
        _ => None,
    }
}

/// All target names, in the order they appear under `corpus/` and `regressions/`.
pub const TARGETS: [&str; 2] = ["alloc_ops", "alloc_threads"];
