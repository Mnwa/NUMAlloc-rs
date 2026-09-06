#!/usr/bin/env bash
# Long-running parallel AFL++ campaign.
#   scripts/fuzz-campaign.sh <target> [workers] [seconds]
# One main node (CmpLog on) plus secondaries with varied power schedules,
# all sharing artifacts/<target>.  Resume by re-running with the same target
# (the output directory is reused via `-i -`).
set -euo pipefail
cd "$(dirname "$0")/../fuzz/afl"
target="${1:?target (alloc_ops|alloc_threads)}"
workers="${2:-4}"
seconds="${3:-0}"
OUT="artifacts/$target"
export AFL_SKIP_CPUFREQ=1 AFL_NO_AFFINITY=1 AFL_I_DONT_CARE_ABOUT_MISSING_CRASHES=1
ulimit -c 0
cargo afl build --release --bin "$target"
mkdir -p "$OUT"
vopt=(); if [ "$seconds" -gt 0 ]; then vopt=(-V "$seconds"); fi
input="corpus/$target"; if [ -d "$OUT/main" ]; then input=-; fi
schedules=(fast explore coe rare exploit seek)
cargo afl fuzz -i "$input" -o "$OUT" -M main -p explore "${vopt[@]}" \
  -x dictionaries/alloc_ops.dict -- "target/release/$target" > "$OUT/main.log" 2>&1 &
for ((i = 1; i < workers; i++)); do
  sched="${schedules[$((i % ${#schedules[@]}))]}"
  cargo afl fuzz -i "$input" -o "$OUT" -S "w$i" -p "$sched" -c - "${vopt[@]}" \
    -x dictionaries/alloc_ops.dict -- "target/release/$target" > "$OUT/w$i.log" 2>&1 &
done
wait
cargo afl whatsup -s "$OUT" || true
find "$OUT" -path '*/crashes/id:*' -o -path '*/hangs/id:*'
