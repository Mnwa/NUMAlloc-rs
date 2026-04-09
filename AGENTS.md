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
3. **Region bump allocator** (`NodeRegion`) — atomic `fetch_add` to carve 32 KB bags from a contiguous pre-mapped region bound to a physical node.
4. **Direct mmap** — for objects > 16 KB. Each large allocation gets its own mapping with a prepended `LargeHeader`.

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
| `src/size_class.rs`           | 12 power-of-2 size classes (8 B – 16 KB), bag math                         |
| `src/platform.rs`             | OS abstraction: mmap, munmap, mbind, sched_setaffinity, topology detection |
| `docs/architecture_design.md` | Full design document with diagrams and benchmarks                          |

### Allocation Path (Small Object, <= 16 KB)

1. Look up size class index.
2. Try per-thread freelist → return if hit.
3. Try per-node Treiber stack → batch-pop up to `REFILL_BATCH` (32) objects → return one.
4. Bump-allocate a fresh 32 KB bag from the node region → carve objects → push to per-thread freelist → return one.

### Deallocation Path

1. Determine origin node via O(1) pointer arithmetic.
2. If **local** (same node as current thread): push to per-thread freelist. If count exceeds `MAX_THREAD_CACHE` (64), drain cold objects to per-node Treiber stack.
3. If **remote** (different node): push directly to origin node's per-node Treiber stack (lock-free CAS).

### Large Object Path (> 16 KB)

- **Alloc**: `mmap` with alignment padding + `LargeHeader` prepended.
- **Dealloc**: read header, `munmap` the original region.
- **Realloc**: alloc new → copy → dealloc old (no in-place growth).

## Design Decisions

### Why contiguous virtual mapping
O(1) node identification from any pointer. No metadata lookup, no hash table — just integer division.

### Why Treiber stacks (not mutexes)
Lock-free CAS scales linearly with thread count. No priority inversion, no convoy effects. ABA protection via 16-bit generation tag packed into `AtomicU64`.

### Why per-thread heaps are heap-allocated (system allocator)
Avoids bootstrap recursion: the NUMA allocator cannot allocate from itself during initialization.

### Why batch drain/refill
Amortizes CAS cost across 32 objects instead of one. Keeps hot objects in thread-local cache.

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
- All tests must pass before merge: `cargo test`.
- Tests cover: all 12 size classes, alignment, reuse, bulk allocation, multi-threaded allocation, cross-thread deallocation, realloc, alloc_zeroed, and concurrent stress.

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
