# AFL++ fuzzing for numalloc

Coverage-guided fuzzing of the public `GlobalAlloc` surface of `numalloc`
with [cargo-afl](https://github.com/rust-fuzz/afl.rs) (AFL++). This directory
is a standalone Cargo workspace so `afl` never enters the main crate's
dependency graph.

## Targets

| Target          | What it drives                                                                                     | Oracles                                                                                          |
|-----------------|----------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------|
| `alloc_ops`     | One thread runs a bounded program of `alloc` / `alloc_zeroed` / `dealloc` / `realloc` / verify ops | non-null, alignment, no overlap with live blocks, fill pattern intact until free, zeroed memory, realloc preserves `min(old,new)` |
| `alloc_threads` | The input is split in two programs run on fresh threads; two more fresh threads then realloc/free the *other* thread's blocks | same oracles, plus remote deallocation, thread-exit drains and per-node refills across two virtual NUMA nodes |

Both bodies live in `src/lib.rs` and are engine-neutral; `src/bin/*.rs` are
thin adapters. Under `cargo afl build` (`cfg(fuzzing)`) they run the
persistent AFL++ loop; under plain `cargo build` they replay one input from
stdin. Both builds share `target/`, so a plain `cargo build` or `cargo test`
overwrites the instrumented binaries — the scripts always rebuild with
`cargo afl` before fuzzing; do the same by hand.

### Input format

A program is a sequence of 4-byte ops `[opcode, a, b, c]`:

| `opcode % 5` | op             | operands                                                                 |
|--------------|----------------|--------------------------------------------------------------------------|
| 0            | alloc          | size = `decode_size(a, b)`, align = `ALIGNS[c & 15]` (1 B … 512 KiB)      |
| 1            | alloc_zeroed   | as above                                                                 |
| 2            | dealloc        | live index = `a | c << 8` (mod live count)                               |
| 3            | realloc        | live index = `a`, new size = `decode_size(b, c)`, alignment unchanged    |
| 4            | verify         | live index = `a | c << 8`                                                |

`decode_size` (see `src/lib.rs`) is biased towards size-class boundaries
(2^k, 2^k ± 1, 2^k + small, 1.5·2^k, page multiples) and caps at 2 MiB.

### Bounds

| Limit                    | Value  | Why                                                                     |
|--------------------------|--------|-------------------------------------------------------------------------|
| ops per iteration        | 4096   | enough for 1024+ distinct-size large alloc/free pairs (cache eviction)  |
| live allocations         | 1024   | exceeds the large cache's 1024 slots; small-class drains reachable      |
| live bytes               | 32 MiB | above the fuzz region so mmap fallback on exhaustion is reachable       |
| single allocation        | 2 MiB  | covers all 16 size classes and the large path                           |
| AFL++ `-t`               | 5000ms | a worst-case iteration writes hundreds of MiB of fill pattern           |

### Determinism

`numalloc` is built with its `fuzz-hooks` feature (test seam, never for
production). The harness sets, if unset, `NUMALLOC_FUZZ_NODES=2` (two virtual
nodes; `mbind` to a missing node fails silently) and
`NUMALLOC_FUZZ_REGION_MB=16` before the first allocation, and calls
`NumaAlloc::fuzz_reset` at the start of every body so each input starts from
a pristine allocator. The AFL++ binaries also run one warm-up body before the
persistent loop so one-time initialisation edges are not blamed on the first
iteration.

Measured on a 24-core WSL2 host with cargo-afl 0.18.2 / rustc 1.98.1:

| Target          | Stability | Speed (one worker) | Notes                                                                                   |
|-----------------|-----------|--------------------|-----------------------------------------------------------------------------------------|
| `alloc_ops`     | ~97%      | ~250 execs/s       | remaining variance is address-dependent bucketing in the large cache                    |
| `alloc_threads` | ~60%      | ~550 execs/s       | inherent: spawn/join/channel code and per-node CAS retry loops are schedule-dependent   |

If `alloc_ops` stability drops well below that, something leaks state between
iterations (a new cache, a new `static`) and needs investigating, not ignoring.

## Running

```bash
cargo install cargo-afl --version 0.18.2 --locked   # pinned in CI
scripts/build.sh                                    # cargo afl build --release

# replay any input (works with plain cargo too)
target/release/alloc_ops < corpus/alloc_ops/basic.bin

# bounded single worker (what CI runs)
scripts/smoke.sh alloc_ops 60

# multi-core campaign: main (CmpLog) + secondaries with varied schedules
scripts/parallel.sh alloc_ops 8 [seconds] [artifacts/alloc_ops]
cargo afl whatsup -s artifacts/alloc_ops

# resume a campaign
cargo afl fuzz -i - -o artifacts/alloc_ops -S resume -- target/release/alloc_ops
```

On hosts whose `/proc/sys/kernel/core_pattern` pipes to a handler (WSL2, many
desktops) AFL++ refuses to start; either fix it with `cargo afl system-config`
(needs root, changes system settings) or export
`AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES=1` (crashes are still detected via
signals, only core dumps are lost). The scripts also run `ulimit -c 0`:
without it, an aborting iteration waits for the crash handler and AFL++
records it under `hangs/` rather than `crashes/`. If you see hangs that
replay instantly, that is what happened.

## Triage

```bash
scripts/triage.sh alloc_ops artifacts/alloc_ops/main/crashes/id:000000,... triage/000000
RUST_BACKTRACE=1 target/release/alloc_ops < triage/000000/minimized.bin

# replay many inputs in one process, in order, timing each (persistent-mode analogue)
target/release/replay alloc_ops artifacts/alloc_ops/*/hangs/id*
```

`triage.sh` copies the artifact, records tool versions, replays, minimizes
with `cargo afl tmin`, and replays the minimized input. Fix the root cause in
`src/`, then:

1. copy `minimized.bin` to `regressions/<target>/<descriptive-name>.bin`;
2. add a focused unit test in `../../src/lib.rs`;
3. run `cargo test --release` here (replays `corpus/` and `regressions/`).

## Findings so far

Inputs under `regressions/` reproduce the bugs the harness found or confirmed
while it was being built (all fixed in the main crate, each with a unit test):

- `alloc-zeroed-large-cache-reuse.bin` — `alloc_zeroed` for > 256 KiB
  returned the previous owner's data when the mapping came from the
  per-thread large cache.
- `realloc-same-class-on-mmap-fallback.bin` — after region exhaustion a small
  request is backed by an exactly-sized mmap; `realloc` treated it as a full
  size-class object and returned it unchanged for a larger request, writing
  past the mapping (SIGSEGV or silent corruption of the neighbouring header).

## Layout

```
fuzz/afl/
├── Cargo.toml          standalone workspace, afl + numalloc(fuzz-hooks)
├── src/lib.rs          target bodies, decoder, model and oracles
├── src/bin/            AFL++ adapters / stdin replay binaries, plus `replay`
├── corpus/<target>/    hand-written seeds (keep small; see corpus/README.md)
├── regressions/<target>/  minimized inputs for fixed bugs (replayed by tests)
├── tests/regressions.rs   replays corpus/ and regressions/
├── scripts/            build, smoke, parallel, triage, cmin
└── artifacts/, triage/ campaign output (git-ignored)
```

## Known gaps

- AFL explores inputs, not schedules: true concurrent interleavings (Treiber
  stack ABA, concurrent refill/drain) are covered only by the ordinary
  multi-threaded tests, not by these targets.
- Only Linux x86-64 has been run. Virtual node 1 never actually `mbind`s on
  a single-node host; remote *placement* is not verified, only the code path.
- The OS-level effects of `munmap` sizes (page leaks) are not observable from
  the harness; those are covered by debug assertions in `dealloc_large`.
