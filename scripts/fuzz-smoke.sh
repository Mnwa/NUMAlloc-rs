#!/usr/bin/env bash
# Bounded AFL++ smoke run for CI / pre-commit: builds both targets, replays
# the corpus, then fuzzes each target for $SMOKE_SECONDS (default 60).
set -euo pipefail
cd "$(dirname "$0")/../fuzz/afl"
SMOKE_SECONDS="${SMOKE_SECONDS:-60}"
OUT="${OUT:-artifacts/smoke}"
export AFL_SKIP_CPUFREQ=1 AFL_NO_AFFINITY=1 AFL_FAST_CAL=1
# Aborting iterations are the crash signal; never let them write core files
# (on hosts whose core_pattern is a pipe, cores would otherwise be misfiled as hangs).
export AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES=1
ulimit -c 0
cargo afl build --release --bin alloc_ops --bin alloc_threads
cargo test --release
status=0
for target in alloc_ops alloc_threads; do
  rm -rf "$OUT/$target"
  cargo afl fuzz -i "corpus/$target" -o "$OUT/$target" -S ci -V "$SMOKE_SECONDS" \
    -x dictionaries/alloc_ops.dict -- "target/release/$target" >/dev/null || true
  crashes=$(find "$OUT/$target" -path '*/crashes/id:*' | wc -l)
  hangs=$(find "$OUT/$target" -path '*/hangs/id:*' | wc -l)
  echo "$target: crashes=$crashes hangs=$hangs"
  if [ "$crashes" -gt 0 ] || [ "$hangs" -gt 0 ]; then status=1; fi
done
exit $status
