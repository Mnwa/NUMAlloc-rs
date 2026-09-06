#!/usr/bin/env bash
# Normal CI gate: formatting, lints, unit + integration tests, fuzz corpus replay.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
( cd fuzz/afl && cargo test --release )
