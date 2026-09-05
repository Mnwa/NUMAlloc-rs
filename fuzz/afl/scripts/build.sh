#!/usr/bin/env bash
# Build all AFL++ targets (instrumented, release, debug assertions on).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo afl --version
cargo afl build --release "$@"
