#!/usr/bin/env bash
# Sanitizer builds (nightly only; never part of release builds).
#   scripts/sanitizers.sh asan     AddressSanitizer
#   scripts/sanitizers.sh tsan     ThreadSanitizer (std rebuilt with -Zbuild-std)
#   scripts/sanitizers.sh all
# Extra args are passed to `cargo test`.
set -euo pipefail
cd "$(dirname "$0")/.."
# Match the host by default so local validation also works on Apple Silicon.
# Cross-target runners can still select an explicit sanitizer target.
TARGET="${SANITIZER_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
which="${1:-all}"; shift || true

run_asan() {
  echo "== AddressSanitizer =="
  RUSTFLAGS="-Zsanitizer=address" \
  CARGO_TARGET_DIR=target/asan \
  ASAN_OPTIONS="${ASAN_OPTIONS:-detect_leaks=0}" \
    cargo +nightly test --target "$TARGET" --lib --tests "$@"
}

run_tsan() {
  echo "== ThreadSanitizer =="
  rustup component add --toolchain nightly rust-src >/dev/null 2>&1 || true
  RUSTFLAGS="-Zsanitizer=thread" \
  CARGO_TARGET_DIR=target/tsan \
  TSAN_OPTIONS="${TSAN_OPTIONS:-halt_on_error=1 report_signal_unsafe=0}" \
    cargo +nightly test -Zbuild-std --target "$TARGET" --lib --tests "$@"
}

case "$which" in
  asan) run_asan "$@" ;;
  tsan) run_tsan "$@" ;;
  all)  run_asan "$@"; run_tsan "$@" ;;
  *) echo "unknown sanitizer: $which" >&2; exit 2 ;;
esac
