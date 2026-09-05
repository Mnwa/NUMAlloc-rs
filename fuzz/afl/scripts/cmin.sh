#!/usr/bin/env bash
# Minimize a campaign queue back into a small seed corpus.
#   scripts/cmin.sh <target> <queue-or-raw-dir> <out-dir>
set -euo pipefail
cd "$(dirname "$0")/.."
target=${1:?} input=${2:?} out=${3:?}
[[ -e "$out" ]] && { echo "output dir exists: $out" >&2; exit 1; }
AFL_SKIP_CPUFREQ=1 AFL_NO_AFFINITY=1 cargo afl cmin -i "$input" -o "$out" -- "target/release/$target"
