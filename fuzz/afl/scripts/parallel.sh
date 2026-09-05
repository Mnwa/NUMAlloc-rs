#!/usr/bin/env bash
# Multi-worker campaign for one target.
#   scripts/parallel.sh <target> <workers> [seconds] [output-dir]
# Worker 1 is the main node (CmpLog on), the rest are secondaries with
# CmpLog off (-c -) and varied power schedules. Monitor with:
#   cargo afl whatsup -s <output-dir>
set -euo pipefail
cd "$(dirname "$0")/.."
target=${1:?usage: parallel.sh <target> <workers> [seconds] [output-dir]}
workers=${2:?usage: parallel.sh <target> <workers> [seconds] [output-dir]}
seconds=${3:-0}
out=${4:-artifacts/$target}

export AFL_SKIP_CPUFREQ=1 AFL_NO_AFFINITY=1 AFL_NO_UI=1
# No core dumps: on hosts whose core_pattern pipes to a handler, a crashing
# iteration otherwise spends seconds in that handler and AFL++ files it as a
# hang instead of a crash.
ulimit -c 0
# Always (re)build with cargo-afl: a plain `cargo build`/`cargo test` in this
# directory writes un-instrumented binaries to the same target dir.
cargo afl build --release --bin "$target"
binary="target/release/$target"
mkdir -p "$out"

common=(-i "corpus/$target" -o "$out" -t 5000)
[[ "$seconds" -gt 0 ]] && common+=(-V "$seconds")
pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' INT TERM

AFL_FINAL_SYNC=1 cargo afl fuzz "${common[@]}" -M main -- "$binary" >"$out/main.log" 2>&1 &
pids+=("$!")
schedules=(fast explore rare coe exploit)
for ((i = 2; i <= workers; i++)); do
  s=${schedules[$(((i - 2) % ${#schedules[@]}))]}
  cargo afl fuzz "${common[@]}" -c - -S "$s-$i" -p "$s" -- "$binary" >"$out/$s-$i.log" 2>&1 &
  pids+=("$!")
done
echo "started ${#pids[@]} workers for $target -> $out"
wait
