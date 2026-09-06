# numalloc AFL++ harnesses

See `docs/testing.md` at the repository root for the full description.

```
corpus/<target>/        seed inputs (regenerate: cargo run --release --bin gen_corpus)
regressions/<target>/   minimised inputs of fixed bugs; replayed by `cargo test`
dictionaries/           op-kind / size / alignment byte tokens
src/lib.rs              fuzz_ops / fuzz_threads target bodies (engine-neutral)
src/bin/*.rs            thin AFL adapters; replay stdin when built without cargo-afl
```

Quick start:

```bash
cargo afl build --release
cargo test --release                              # corpus + regression replay
../../scripts/fuzz-smoke.sh                       # bounded
../../scripts/fuzz-campaign.sh alloc_ops 8 3600   # parallel campaign
```

Toolchain used for the initial campaigns: rustc 1.98.1, cargo-afl 0.18.2
(AFL++ 4.40c), x86_64-unknown-linux-gnu (WSL2, 24 cores).
