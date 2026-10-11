#!/usr/bin/env bash
# Run the benchmark under one of the build presets (1-4) or all of them.
# Builds via build.sh, then runs the produced r3 binary (run() executes the
# benchmark suite). Logs each run to bench_mode<N>.log.
#
# Allocator selection (3rd arg):
#   mimalloc  (default) — `mimalloc` crate, mimalloc v3.3.2 (пакет r3)
#   rustfs              — `rustfs-mimalloc`, mimalloc v3.5.3 (пакет rustfs-bench)
#   both                — обе конфигурации подряд
#
# Usage:
#   ./bench.sh 1
#   ./bench.sh 2
#   ./bench.sh 3
#   ./bench.sh 4
#   ./bench.sh all
#   ./bench.sh 1 "" rustfs     # общий бенч на mimalloc v3.5.3
#   ./bench.sh 1 "" both       # прямое сравнение
set -euo pipefail

MODE="${1:-1}"
STYLE="${2:-}"
ALLOCATOR="${3:-mimalloc}"
TRIPLE="$(rustc --print host-tuple)"

if [ -n "$STYLE" ]; then
    export R3_BENCH_STYLE="$STYLE"
fi

case "$ALLOCATOR" in
    mimalloc) ALLOCATORS="mimalloc" ;;
    rustfs)   ALLOCATORS="rustfs" ;;
    both)     ALLOCATORS="mimalloc rustfs" ;;
    *) echo "unknown allocator: $ALLOCATOR (use mimalloc, rustfs or both)" >&2; exit 2 ;;
esac

bin_for() {
    local m="$1" a="$2"
    local pkg_dir="" bin_name="r3"
    if [ "$a" = "rustfs" ]; then pkg_dir="rustfs-bench/"; bin_name="r3-rustfs"; fi
    case "$m" in
        2|4) echo "./${pkg_dir}target/${TRIPLE}/native/${bin_name}" ;;
        *)   echo "./${pkg_dir}target/release/${bin_name}" ;;
    esac
}

if [ "$MODE" = "all" ]; then
    MODES="1 2 3 4"
else
    MODES="$MODE"
fi

for a in $ALLOCATORS; do
    pkg="r3"
    suffix=""
    if [ "$a" = "rustfs" ]; then pkg="rustfs-bench"; suffix="_rustfs"; fi

    for m in $MODES; do
        echo "===== Build (mode $m, package $pkg) ====="
        ./build.sh "$m" "$pkg"

        BIN="$(bin_for "$m" "$a")"
        echo "===== Running benchmark (mode $m, allocator=$a) ====="
        if [ "$a" = "rustfs" ]; then
            export R3_ALLOCATOR_LABEL="rustfs-mimalloc (mimalloc v3.5.3)"
        else
            unset R3_ALLOCATOR_LABEL || true
        fi
        "$BIN" | tee "bench_mode${m}${suffix}.log"
    done
done
