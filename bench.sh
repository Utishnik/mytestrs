#!/usr/bin/env bash
# Run the benchmark under one of the build presets (1-4) or all of them.
# Builds via build.sh, then runs the produced r3 binary (run() executes the
# benchmark suite). Logs each run to bench_mode<N>.log.
#
# Usage:
#   ./bench.sh 1
#   ./bench.sh 2
#   ./bench.sh 3
#   ./bench.sh 4
#   ./bench.sh all
set -euo pipefail

MODE="${1:-1}"
TRIPLE="$(rustc --print host-tuple)"

bin_for() {
    case "$1" in
        2|4) echo "./target/${TRIPLE}/native/r3" ;;
        *)   echo "./target/release/r3" ;;
    esac
}

if [ "$MODE" = "all" ]; then
    MODES="1 2 3 4"
else
    MODES="$MODE"
fi

for m in $MODES; do
    echo "===== Build (mode $m) ====="
    ./build.sh "$m"

    BIN="$(bin_for "$m")"
    echo "===== Running benchmark (mode $m) ====="
    "$BIN" | tee "bench_mode${m}.log"
done
