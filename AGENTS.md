# NUMAlloc - NUMA-Aware Memory Allocator

## Project Overview

NUMAlloc is a high-performance NUMA-aware memory allocator for Rust. It replaces the standard `GlobalAlloc` and optimizes memory placement on multi-socket NUMA machines by binding allocations to the physical node closest to the executing thread.

**Key metrics**: 20.9% faster than glibc, 15.7% faster than mimalloc, 9x fewer remote memory accesses, 18x fewer TLB misses.

## Architecture

### Four-Layer Allocation Hierarchy

```
Thread (lock-free)  ->  Node (lock-free CAS)  ->  Region (atomic bump)  ->  OS mmap (large only)
```

1. **Per-thread freelist** (`ThreadFreelist`) — single-owner, zero synchronization, LIFO. Fastest path.
2. **Per-node freelist** (`TreiberStack`) — lock-free Treiber stack shared by all threads on the same NUMA node. Used for refills and remote deallocations.
3. **Region bump allocator** (`NodeRegion`) — atomic CAS to carve bags (32 KB for small classes, up to 256 KB for larger classes) from a contiguous pre-mapped region bound to a physical node. Bag base is aligned to the bag size for correct object alignment.
4. **Direct mmap** — for objects > 256 KB. Each large allocation gets its own mapping with a prepended `LargeHeader`. Per-thread large object cache avoids repeated syscalls.

### Memory Layout

A single contiguous virtual region is mapped at init (`128 MB * num_nodes`). Each node gets a sub-region that is `mbind`-ed to its physical NUMA node. This enables **O(1) origin-node lookup** via pointer arithmetic: `node = (ptr - base) / region_size`.

### Key Source Files

| File                          | Responsibility                                                             |
|-------------------------------|----------------------------------------------------------------------------|
| `src/lib.rs`                  | Public API, re-exports `NumaAlloc`, integration tests                      |
| `src/allocator.rs`            | `GlobalAlloc` implementation, alloc/dealloc/realloc/alloc_zeroed paths     |
| `src/heap.rs`                 | `GlobalHeap` (singleton), `NodeRegion` (per-node bump allocator)           |
| `src/node_heap.rs`            | `PerNodeHeap` — lock-free Treiber stacks per size class                    |
| `src/thread_heap.rs`          | `PerThreadHeap` — single-threaded freelists per size class                 |
| `src/freelist.rs`             | `FreeBlock`, `TreiberStack` (lock-free), `ThreadFreelist` (single-thread)  |
| `src/size_class.rs`           | 16 power-of-2 size classes (8 B – 256 KB), variable bag sizing              |
| `src/platform.rs`             | OS abstraction: mmap, munmap, mbind, sched_setaffinity, topology detection |
| `docs/architecture_design.md` | Full design document with diagrams and benchmarks                          |

### Allocation Path (Small Object, <= 256 KB)

1. Look up size class index (16 power-of-2 classes from 8 B to 256 KB).
2. Try per-thread freelist → return if hit.
3. Try per-node Treiber stack → `take_all` detaches the whole chain in one CAS, chain-insert into thread freelist, drain anything beyond `max_thread_cache(class)` back to the node stack → return one.
4. Bump-allocate a fresh bag from the node region (32 KB for classes ≤ 16 KB, object-sized for larger) → carve objects → push to per-thread freelist → return one.
5. If region exhausted → fall back to mmap (large object path).

### Deallocation Path

1. Determine origin node via O(1) pointer arithmetic.
2. If **local** (same node as current thread): push to per-thread freelist. If count exceeds `max_thread_cache(class)` (64–2048 depending on class), drain cold objects to per-node Treiber stack.
3. If **remote** (different node): push directly to origin node's per-node Treiber stack (lock-free CAS).

### Large Object Path (> 256 KB)

- **Alloc**: check per-thread large cache (exact/close-size match) → cache miss: `mmap` with alignment padding + `LargeHeader` prepended.
- **Dealloc**: read header → try to cache for reuse (with `madvise` for regions ≥ 512 KB) → cache full: `munmap`.
- **Realloc**: alloc new → copy → dealloc old (no in-place growth).

## Design Decisions

### Why contiguous virtual mapping
O(1) node identification from any pointer. No metadata lookup, no hash table — just integer division.

### Why Treiber stacks (not mutexes)
Lock-free CAS scales linearly with thread count. No priority inversion, no convoy effects. ABA protection via 16-bit generation tag packed into `AtomicU64`.

### Why per-thread heaps are heap-allocated (system allocator)
Avoids bootstrap recursion: the NUMA allocator cannot allocate from itself during initialization.

### Why batch drain/refill
Drain uses `push_chain` (single CAS for entire chain). Refill detaches the whole node chain with `take_all` (single CAS) and walks it once: a per-block `pop` would have to read `next` of a block another thread may already have handed to user code (a data race), and the generation tag only protects the CAS, not that read. Keeps hot objects in thread-local cache.

### Why dynamic per-class cache thresholds
Small objects (8–64 B) cache up to 2048 items; large objects (≥ 16 KB) cache 64. This avoids excessive drain/refill cycles in bulk allocation patterns while bounding memory overhead for large classes.

### Why variable bag sizes
Classes ≤ 16 KB use the standard 32 KB bag (multiple objects per bag). Classes from 32 KB to 256 KB use bag size = object size (one object per bag). This keeps medium objects on the fast freelist path instead of falling through to expensive mmap.

### Why intrusive freelists
Free blocks store their `next` pointer in the freed memory itself (`UnsafeCell<Option<NonNull<FreeBlock>>>`). Zero extra memory overhead for bookkeeping.

### Why round-robin thread-to-node assignment
Distributes memory pressure evenly across NUMA nodes. Avoids hotspotting on node 0.

## Code Quality Rules

### `cargo fmt`
- All code must be formatted with `cargo fmt` before commit.
- Use default rustfmt settings (no custom `rustfmt.toml` overrides unless agreed upon).

### `cargo clippy`
- All code must pass `cargo clippy -- -D warnings` with zero warnings.
- Do not use `#[allow(clippy::...)]` unless the lint is a false positive and the reason is documented in a comment.

### `cargo test`
- All tests must pass before merge: `cargo test` (or `scripts/test.sh`, which also replays the fuzz corpus).
- Tests cover: all 16 size classes, alignment, reuse, bulk allocation, multi-threaded allocation, cross-thread deallocation, realloc, alloc_zeroed, and concurrent stress.
- `tests/` holds the deterministic boundary, stress, model/differential and concurrent suites built on the shared state-machine model in `tests/common/mod.rs`; `tests/regressions.rs` holds one test per fixed bug.

### Mandatory after any allocator change
Any change under `src/` (or to the test model in `tests/common/`) is not done until all of these pass locally:
1. `scripts/test.sh` — fmt, clippy, `cargo test`, fuzz corpus replay.
2. `scripts/miri.sh` — the portable logic under Miri (provenance, aliasing, leaks). Scale new loops with `cfg!(miri)` (see `n()`/`t()` in `src/lib.rs`) rather than skipping tests.
3. `scripts/fuzz-smoke.sh` — rebuild both AFL targets and fuzz each for at least 60 s (longer, `scripts/fuzz-campaign.sh <target> 8 900`, for changes to freelists, refill/drain, the large-object path or `platform.rs`). Zero crashes and zero hangs required; any finding goes through the triage workflow in `docs/testing.md` (minimise, root-cause, fix, regression input in `fuzz/afl/regressions/` plus a test in `tests/`).
4. For changes touching hot paths, `scripts/sanitizers.sh` and a `cargo bench` comparison against the previous commit (see the numbers in `README.md`, "Correctness and validation").

### Validation tooling (see `docs/testing.md`)
- `internal-testing` cargo feature: `NumaAlloc::with_config(nodes, region_size)` and `unsafe fn validate_internal_state()` (metadata invariant walker in `src/validate.rs`). Test/fuzz only — never enable in production.
- `scripts/miri.sh`, `scripts/sanitizers.sh {asan|tsan}`, `scripts/fuzz-smoke.sh`, `scripts/fuzz-campaign.sh`.
- AFL++ harnesses live in the standalone `fuzz/afl` workspace (`alloc_ops`, `alloc_threads`); every fixed fuzz finding gets its input in `fuzz/afl/regressions/` **and** a format-independent test in `tests/`.
- Under `cfg(miri)` `src/platform.rs` replaces mmap/mbind/sysfs/affinity with portable shims; keep new platform calls behind the same gates.

## Safety Guidelines

### `unsafe` Usage Policy
- Every `unsafe` block must have a `// SAFETY:` comment explaining why the invariants hold.
- **Avoid raw pointers** (`*mut T`, `*const T`) wherever possible. Use `NonNull<T>` for non-null pointers, `UnsafeCell<T>` for interior mutability, and other typed wrappers instead of bare pointer casts. Raw pointers are acceptable only at FFI boundaries (e.g., `libc` calls) and `GlobalAlloc` trait signatures which require them.
- Use `UnsafeCell<T>` for interior mutability in single-threaded contexts (e.g., `FreeBlock::next`). Never use raw pointer casts to bypass aliasing rules.
- Minimize `unsafe` surface area: isolate unsafe operations into small, well-documented functions.

### Concurrency Safety
- **Lock-free first**: prefer atomic operations (`AtomicUsize`, `AtomicU64`) and CAS loops over mutexes.
- Use correct memory orderings: `Acquire` on loads, `Release` on stores, `AcqRel` on compare-exchange. Never use `Relaxed` unless the operation is truly order-independent (e.g., advisory `is_empty` checks).
- All shared types must implement `Send + Sync` with explicit `unsafe impl` and documented safety invariants.
- Treiber stack uses generation tags to prevent ABA — do not remove or weaken this protection.

### Performance Priorities
1. **Hot path** (per-thread alloc/dealloc): zero synchronization, no atomics, no syscalls.
2. **Warm path** (per-node refill/drain): lock-free CAS, batch operations to amortize cost.
3. **Cold path** (bag allocation, mmap): acceptable to use atomics/syscalls since this is infrequent.
4. **Inline aggressively** on hot paths (`#[inline]`).
5. Avoid heap allocation on alloc/dealloc paths (no `Vec`, `Box`, `String`).
6. **Use `MaybeUninit`** for arrays and buffers where only a subset of elements are valid (guarded by a count/length field). This avoids wasteful zeroing or initialization of elements that will be overwritten before being read. Example: `LargeCache::entries` uses `MaybeUninit<LargeCacheEntry>` — only entries `[0..count)` are initialised.

## Dependencies

**Minimal by design**: only `libc` for POSIX syscalls. No allocator crate, no proc macros, no `std` collections on the allocation path.

## Public API

```rust
pub struct NumaAlloc;

impl NumaAlloc {
    pub const fn new() -> Self {
        Self
    }
}

unsafe impl GlobalAlloc for NumaAlloc { /* alloc, dealloc, realloc, alloc_zeroed */ }

// Usage:
#[global_allocator]
static ALLOC: numalloc::NumaAlloc = numalloc::NumaAlloc::new();
```
