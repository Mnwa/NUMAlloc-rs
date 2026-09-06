#!/usr/bin/env bash
# Run the portable allocator logic under Miri (nightly).  Platform calls are
# replaced by cfg(miri) shims in src/platform.rs; see docs/testing.md.
# Usage: scripts/miri.sh [extra cargo test args...]
set -euo pipefail
cd "$(dirname "$0")/.."
rustup component add --toolchain nightly miri >/dev/null 2>&1 || true
# The Treiber stack packs pointers into tagged integers, so the program is
# inherently a permissive-provenance program; the flag only silences Miri's
# advisory int-to-ptr warning.  Stacked Borrows and the leak check stay on.
export MIRIFLAGS="${MIRIFLAGS:--Zmiri-permissive-provenance}"
cargo +nightly miri test "$@"
