#!/usr/bin/env bash
# Bounded single-worker smoke campaign, suitable for CI.
#   scripts/smoke.sh <target> [seconds] [output-dir]
# Exits non-zero if AFL++ saved any crash or hang.
set -euo pipefail
cd "$(dirname "$0")/.."
target=${1:?usage: smoke.sh <target> [seconds] [output-dir]}
seconds=${2:-60}
out=${3:-artifacts/smoke-$target-$(date -u +%Y%m%dT%H%M%SZ)}

export AFL_SKIP_CPUFREQ=1 AFL_NO_AFFINITY=1 AFL_NO_UI=1
# No core dumps: on hosts whose core_pattern pipes to a handler, a crashing
# iteration otherwise spends seconds in that handler and AFL++ files it as a
# hang instead of a crash.
ulimit -c 0 AFL_FAST_CAL=1 AFL_CMPLOG_ONLY_NEW=1
[[ -e "$out" ]] && { echo "output dir exists: $out" >&2; exit 1; }

cargo afl build --release --bin "$target"
cargo afl fuzz -i "corpus/$target" -o "$out" -S ci -V "$seconds" -t 5000 -- "target/release/$target"

mapfile -d '' findings < <(find "$out" \( -path '*/crashes/*' -o -path '*/hangs/*' \) -type f ! -name 'README*' -print0)
if ((${#findings[@]} > 0)); then
  printf 'AFL++ findings:\n' >&2; printf '  %s\n' "${findings[@]}" >&2; exit 1
fi
echo "smoke run of $target finished without crashes or hangs ($out)"
