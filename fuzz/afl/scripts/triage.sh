#!/usr/bin/env bash
# Reproduce and minimize one crash/hang artifact.
#   scripts/triage.sh <target> <artifact> <triage-dir>
set -euo pipefail
cd "$(dirname "$0")/.."
target=${1:?} artifact=${2:?} dir=${3:?}
binary="target/release/$target"
[[ -e "$dir" ]] && { echo "triage dir exists: $dir" >&2; exit 1; }
mkdir -p "$dir"
cp -- "$artifact" "$dir/original.bin"
{ date -u '+timestamp_utc=%Y-%m-%dT%H:%M:%SZ'; echo "target=$target"; cargo afl --version; rustc -Vv; } >"$dir/metadata.txt"
set +e
RUST_BACKTRACE=full "$binary" <"$dir/original.bin" >"$dir/replay.stdout" 2>"$dir/replay.stderr"; echo $? >"$dir/replay.exit-status"
set -e
cargo afl tmin -i "$dir/original.bin" -o "$dir/minimized.bin" -- "$binary"
set +e
RUST_BACKTRACE=full "$binary" <"$dir/minimized.bin" >"$dir/minimized.stdout" 2>"$dir/minimized.stderr"; echo $? >"$dir/minimized.exit-status"
echo "triage saved to $dir (replay exit $(cat "$dir/replay.exit-status"), minimized exit $(cat "$dir/minimized.exit-status"))"
echo "to keep as a regression: cp $dir/minimized.bin regressions/$target/<descriptive-name>.bin"
