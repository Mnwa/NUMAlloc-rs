# NUMAlloc

**A blazing-fast, NUMA-aware memory allocator written in pure Rust.**

NUMAlloc is a drop-in replacement for the global allocator, purpose-built for Non-Uniform Memory Access (NUMA) machines. It pins threads and memory to NUMA nodes, routes freed objects back to their origin node, and shares huge pages incrementally -- delivering fewer remote memory accesses, fewer TLB misses, and lower latency than general-purpose allocators.

## Features

- **Zero-cost NUMA awareness** -- O(1) origin-node lookup via pointer arithmetic, no syscalls on the hot path
- **Origin-aware deallocation** -- freed objects return to their origin node's freelist, eliminating remote reuse
- **Lock-free concurrency** -- Treiber stacks with ABA-safe generation tags for inter-thread communication
- **Incremental huge page sharing** -- threads on the same node share 2 MB transparent huge pages in 32 KB increments
- **Memory safe** -- built with Rust's ownership model; every `unsafe` block is documented with safety invariants
- **Minimal dependencies** -- only `libc` for POSIX syscalls, no proc macros, no heavy frameworks
- **Drop-in `GlobalAlloc`** -- one line to replace your allocator

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
numalloc = "0.1"
```

Set as global allocator:

```rust
use numalloc::NumaAlloc;

#[global_allocator]
static ALLOC: NumaAlloc = NumaAlloc::new();

fn main() {
    // All allocations now go through NUMAlloc.
    let v: Vec<u64> = vec![1, 2, 3];
    println!("{v:?}");
}
```

## Architecture

NUMAlloc uses a four-layer allocation hierarchy, from fastest to slowest:

```
Thread (lock-free) -> Node (lock-free CAS) -> Region (atomic bump) -> OS mmap
```

### Allocation path (small objects, <= 16 KB)

```
malloc(size)
  |
  v
Per-Thread Freelist  ----hit----> return pointer (zero synchronization)
  |
  miss
  v
Per-Node Treiber Stack  --hit--> batch-pop 32 objects, return one
  |
  miss
  v
Region Bump Allocator  --------> carve 32 KB bag, fill freelist, return one
```

### Deallocation path

```
free(ptr)
  |
  v
Compute origin node = (ptr - base) / region_size   // O(1), no syscall
  |
  +-- local? --> push to per-thread freelist (no locks)
  |
  +-- remote? --> push to origin node's Treiber stack (lock-free CAS)
```

### Heap layout

A single contiguous virtual region is mapped at init and divided equally among NUMA nodes. Each sub-region is bound to its physical node via `mbind`. This design enables origin-node identification through simple integer division on any pointer.

```
|---- Node 0 ----|---- Node 1 ----|---- Node 2 ----|---- Node N ----|
|  small  | big  |  small  | big  |  small  | big  |  small  | big  |
     ^                ^                ^                ^
     bpSmall          bpSmall          bpSmall          bpSmall
```

For the full design document with Mermaid diagrams and benchmark details, see [docs/architecture_design.md](docs/architecture_design.md).

## Design Principles

- **Hot path = zero synchronization.** Per-thread freelists are single-owner, no atomics, no syscalls.
- **Lock-free over locks.** Shared per-node heaps use Treiber stacks with CAS, not mutexes.
- **Batch everything.** Drain and refill operations move 32 objects at once, amortizing CAS cost.
- **Intrusive data structures.** Free blocks store their `next` pointer in the freed memory itself -- zero extra overhead.
- **Explicit safety.** Every `unsafe` block carries a `// SAFETY:` comment. `NonNull<T>` over `*mut T`. `UnsafeCell` for interior mutability.

## When to Use NUMAlloc

**Good fit:**
- Multi-threaded server applications on NUMA hardware (2+ sockets)
- Workloads with many small allocations and high thread counts
- Environments with transparent huge pages enabled

**Not ideal for:**
- Single-threaded applications
- Heavy cross-node producer-consumer patterns
- Asymmetric or heterogeneous memory topologies

## Contributing

Contributions are welcome! Please ensure your changes pass:

```sh
cargo fmt
cargo clippy -- -D warnings
cargo test
```

## License

See [LICENSE](LICENSE) for details.

## References

Based on: Yang et al., *"NUMAlloc: A Faster NUMA Memory Allocator,"* ISMM 2023, ACM.
