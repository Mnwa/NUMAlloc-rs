# Validation and fuzzing

This document describes the correctness/memory-safety validation setup:
what each layer checks, how to run it, and how to turn a fuzz finding into a
regression test.

| Layer | Command | Runs in CI | What it catches |
|---|---|---|---|
| Unit + integration tests | `cargo test` | yes | functional regressions, boundary behaviour, invariant violations |
| Fuzz corpus replay | `cd fuzz/afl && cargo test --release` | yes | regressions on every committed seed/regression input |
| Miri | `scripts/miri.sh` | yes (nightly) | UB in portable logic: provenance, aliasing, uninit reads, misaligned access, leaks |
| AddressSanitizer | `scripts/sanitizers.sh asan` | yes (nightly) | out-of-bounds / use-after-free on system-allocated metadata, stack, globals |
| ThreadSanitizer | `scripts/sanitizers.sh tsan` | manual | data races in the lock-free paths |
| AFL smoke | `scripts/fuzz-smoke.sh` | yes (nightly, 60 s/target) | new crashes on the two harnesses |
| AFL campaign | `scripts/fuzz-campaign.sh <target> [workers] [seconds]` | manual | long-running coverage-guided search |

## The `internal-testing` feature

`cargo test` enables it through a self dev-dependency; the fuzz package
enables it explicitly.  It adds:

* `NumaAlloc::with_config(num_nodes, region_size)` – a virtual topology.
  Multi-node code paths (remote frees, round-robin thread placement) run on a
  single-node host, and tiny regions (256 KiB) make region exhaustion and the
  mmap fallback reachable in a handful of operations.
* `NumaAlloc::validate_internal_state()` – the metadata invariant walker
  (`src/validate.rs`).  It is `unsafe` because it reads intrusive `next`
  links of blocks on the shared per-node stacks, which is only sound while no
  other thread uses the allocator.  It checks:
  * per-node bump pointer within the region, region bases contiguous;
  * every free block inside its node's region, below the bump pointer and
    aligned to its size class;
  * no block appears twice across node stacks and the calling thread's
    caches; blocks of different classes never overlap (8-byte occupancy
    bitmap);
  * every 32 KiB bag granule is claimed by at most one size class;
  * thread freelist `count`, `tail` and chain terminator agree;
  * large-object cache slot/byte accounting is exact; entries are page
    aligned, disjoint, and outside the region.
  Bookkeeping memory is mapped directly with `mmap`, so validation never
  re-enters the allocator.
* `NumaAlloc::owns(ptr)` / `num_nodes()` for tests that need to know which
  path served a request.

The feature is never enabled in production builds; the struct layout of
`NumaAlloc` only changes when it is on.

## Shared state-machine model (`tests/common/mod.rs`)

All deterministic tests and both AFL harnesses drive the allocator through
one model:

* **Slots.** A fixed table of slots; each is empty or owns exactly one live
  allocation with its size, alignment and a generation counter.
* **Contents oracle.** Every allocation is filled with
  `pattern_byte(slot, generation, offset)`.  Before any free, realloc,
  transfer or on `CheckAll`, the bytes are re-verified, so corruption by an
  unrelated operation is detected.  `FillMode::Full` writes every byte;
  `FillMode::Sparse` writes head, tail and one byte per page (used for
  multi-megabyte objects and the fuzzers).
* **Properties checked on every successful allocation:** requested alignment,
  disjointness from all live slots, zero-fill for `alloc_zeroed`, preserved
  prefix for `realloc`, and that bounded requests never fail.
* **Invalid usage is rejected by the model**, never sent to the allocator:
  double frees, frees of empty slots, zero-sized layouts, invalid layouts.
* **Operations:** `Alloc`, `Free`, `Realloc`, `Replace`, `FillEmpty`,
  `FreeAll{Fifo,Lifo,EverySecond}`, `Churn`, `Touch`, `CheckAll`, `Validate`,
  `Huge` (must fail cleanly), `Transfer` (multi-thread only).
* **Decoder.** Bytes → ops with boundary-biased value decoding: sizes come
  from a 256-entry table of `B-1, B, B+1` around every size class, bag,
  page, `SMALL_LIMIT`, large-cache and madvise threshold (plus small
  products and powers of two); alignments span 1 .. 4 MiB.  An `encode`
  inverse produces readable seed corpora (`fuzz/afl/src/bin/gen_corpus.rs`).
* **Differential check.** `tests/model.rs` runs identical op streams against
  `std::alloc::System` and compares success/failure, live sets and contents
  (never addresses).

## AFL targets (`fuzz/afl`)

Standalone workspace (the `afl` crate never enters the main lockfile).

* `alloc_ops` – sequential state machine.  Header byte selects 1–4 virtual
  nodes and a 256 KiB – 2 MiB region; the rest decodes to ≤ 4096 ops on 64
  slots; ≤ 20 000 allocations per input; `Validate` ops run the invariant
  walker mid-sequence and it always runs at the end (before and after freeing
  everything).
* `alloc_threads` – 2–4 scoped threads, each with its own model on one
  shared allocator; `Transfer` ops move live allocations over channels; the
  receiver verifies the sender's pattern before adopting/freeing.  Only
  memory owned by the checking thread is ever inspected, so failures are real
  allocator bugs, not harness races.  Stability is lower than `alloc_ops`
  (scheduling), which is expected.

A **fresh `NumaAlloc::with_config` per iteration** makes persistent-mode
iterations independent: any crash reproduces from its input alone.

```bash
cd fuzz/afl
cargo afl build --release                 # instrumented
cargo run --release --bin gen_corpus      # regenerate seeds from op lists
cargo test --release                      # replay corpus + regressions
../../scripts/fuzz-smoke.sh               # 60 s per target
../../scripts/fuzz-campaign.sh alloc_ops 8 3600
target/release/alloc_ops < artifacts/alloc_ops/main/crashes/id:000000*   # replay
cargo afl tmin -i <crash> -o min.bin -- target/release/alloc_ops           # minimise
```

Host notes: on WSL2 `/proc/sys/kernel/core_pattern` is a pipe, so the
scripts export `AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES=1` and `ulimit -c 0`
(otherwise aborting iterations are misfiled as hangs).  Do not run
`cargo afl system-config` automatically (it needs sudo).

### Triage workflow

1. Copy the crash file into `fuzz/afl/regressions/<target>/` with a
   descriptive name — `cargo test --release` in `fuzz/afl` then replays it
   forever.
2. Reproduce: `target/release/<target> < file` (built with plain
   `cargo build --release`, no AFL needed) and `RUST_BACKTRACE=1`.
3. Minimise with `cargo afl tmin`; decode with the model's `Decoder` to read
   the op list.
4. Fix the root cause in `src/`, add a deterministic test in `tests/` that
   does not depend on the fuzz format, re-run `scripts/test.sh`.

## Miri

`src/platform.rs` swaps `mmap`/`munmap` for page-aligned system allocations
under `cfg(miri)` and turns `mbind`, sysfs probing, affinity and `madvise`
into no-ops, so every portable path (size classes, bump regions, intrusive
freelists, Treiber stacks with tagged pointers, large headers and caches,
TLS teardown, cross-thread frees) runs under the interpreter with
provenance and aliasing checks.  The default region is 4 MiB under Miri.
Tests scale their iteration counts down with `cfg!(miri)`; nothing is
skipped except address-space exhaustion: Miri reports an unsatisfiable
allocation as a hard "resource exhaustion" error instead of returning null,
so `Model::huge` is a no-op under Miri and `usize_extremes_fail_cleanly` is
ignored there.  A full Miri run takes roughly an hour on a fast machine
(the unit tests alone about five minutes); `scripts/miri.sh --test <name>`
runs a single suite.

## Sanitizers

`scripts/sanitizers.sh` builds into `target/asan` / `target/tsan` so normal
builds are untouched. It selects the host target by default; set
`SANITIZER_TARGET` to override it for a configured cross-target runner.
ASan cannot see inside the allocator's own `mmap`
regions (it only poisons its own malloc), so it primarily guards the
system-allocated metadata (`PerThreadHeap`, `LargeCache`, `SharedHeap`) and
the test harness.  TSan rebuilds `std` with `-Zbuild-std` so channel/thread
primitives are instrumented and reports are trustworthy.

## Findings so far (2026-09-06)

All of these were found while building this setup, by review, benchmark or
test; none required suppressing a diagnostic.  Each has a deterministic
regression test.

| # | Symptom | Root cause | Fix | Regression test |
|---|---|---|---|---|
| 1 | Topology detection always reported one node; thread affinity was never set (uncommitted `platform.rs`) | The node digit of `/sys/devices/system/node/node<N>/cpulist` was written at byte 28 (the `e` of `node`) instead of 29, so the probed path never existed | Path built by `node_cpulist_path()` from the template's prefix length | `platform::tests::cpulist_path_places_digit_after_node_prefix` |
| 2 | Bulk 1–64 KiB workloads 2–4× slower, page-aligned 4–64 KiB 8× slower, 4 KiB producer/consumer 3.7× slower than the last commit (uncommitted `take_all` refill) | Each refill detached the whole node chain, walked all N blocks and pushed most back, so a burst of N allocations cost O(N²/64) pointer chases | Refill walks at most `REFILL_BATCH` blocks and parks the remainder as a thread-owned spare chain, returned on thread exit | `concurrent::spare_chain_is_consumed_and_returned_on_thread_exit`; benchmark table in `README.md` |
| 3 | `alloc_zeroed` on the large path memset every fresh mapping (uncommitted change), making `vec![0u8; n]` fault in all pages eagerly | Zeroing was applied regardless of whether the mapping came from the cache | Only mappings reused from the per-thread large cache are zeroed; fresh mappings rely on kernel zero-fill | `regressions::large_alloc_zeroed_fresh_mapping_stays_lazy`, `…after_dirty_reuse_below_madvise_threshold` |
| 4 | Bag capacity per node varied between runs (AFL stability ~80 %) and up to 252 KiB per node was wasted | Bags are aligned to 256 KiB but the region base was only page aligned | Heap over-maps by one alignment unit and aligns its base; capacity is now exactly `region / bag` | `stress::region_exhaustion_falls_back_and_recovers`, heap base assertions in `validate_internal_state` |
| 5 | `realloc` built a `Layout` with `from_size_align_unchecked` from a caller-supplied size | No validation of the (contract-violating but cheap to check) overflow case | `Layout::from_size_align`; returns null instead of an invalid layout | `boundaries::usize_extremes_fail_cleanly` |
| 6 | Treiber tag packing used plain int↔ptr casts (provenance lost under Miri) and silently truncated addresses above 2^48 | — | `expose_provenance` / `with_exposed_provenance_mut` plus a debug assertion on the address width | Miri run of the concurrent freelist tests |
| 7 | Miri: `global_allocator::cross_thread_dealloc_global_allocator` failed with "tag does not exist in the borrow stack" when a block freed through a `Box` was recycled for a larger request in the same size class | `dealloc` pushed the caller's pointer as-is onto the freelists, so the recycled block carried the caller's provenance (a `Box<T>` covers only `size_of::<T>()` bytes) instead of the slot's; re-deriving from the mapping base instead tripped the weak protector of a `Box` freed inside the frame that received it | `FreeBlock::from_dealloc_ptr` exposes the caller's provenance and rebuilds the block from its address (the mapping base is exposed at heap init), so every byte resolves to the narrowest valid tag; identity on real hardware | `regressions::recycled_block_regains_full_slot_provenance` (fails under Miri without the fix) |
| 8 | AFL saved a hang on `alloc_ops` after a 15 min × 8 worker campaign | Not an allocator hang: a havoc repeat mutation produced 4 021 consecutive `Validate` ops, each a full metadata walk with its own bookkeeping mapping (1.85 s instrumented vs. a ~30 ms timeout) | `VALIDATE_BUDGET` caps `Validate` ops per iteration in the harness, like `ALLOC_BUDGET` does for allocations | `fuzz/afl/regressions/alloc_ops/validate_flood_4k_ops.bin` via `replay::regressions_alloc_ops` |

Pre-existing uncommitted changes from the previous session (heap handle
reference counting, `take_all`, large-cache extent fix, non-allocating
sysfs probing) were kept and are covered by the same suites.
