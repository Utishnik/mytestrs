#!/usr/bin/env bash
# Сравнение двух глобальных аллокаторов mimalloc v3 на одних и тех же бенчмарках:
#   1) `mimalloc` crate (libmimalloc-sys, mimalloc v3.3.2) — пакет r3 (default)
#   2) `rustfs-mimalloc` (bindings к mimalloc v3.5.3)      — пакет rustfs-bench
#
# Оба sys-крейта объявляют links = "mimalloc", поэтому сравниваем их двумя
# отдельными бинарниками с одинаковой нагрузкой и профилями сборки.
#
# Usage:
#   ./bench_allocators.sh                # alloc_bench + run(), scale=4
#   ./bench_allocators.sh --scale 1      # полная нагрузка
#   ./bench_allocators.sh --skip-run     # только divan alloc_bench
#   ./bench_allocators.sh --skip-divan   # только основной run()
#   ./bench_allocators.sh --full-divan   # + тяжёлый divan-сьют benche
set -euo pipefail

SCALE=4
SKIP_DIVAN=0
SKIP_RUN=0
FULL_DIVAN=0

while [ $# -gt 0 ]; do
    case "$1" in
        --scale)      shift; SCALE="${1:-4}" ;;
        --skip-run)   SKIP_RUN=1 ;;
        --skip-divan) SKIP_DIVAN=1 ;;
        --full-divan) FULL_DIVAN=1 ;;
        *) echo "unknown arg: $1"; exit 2 ;;
    esac
    shift
done

if [ "$SKIP_DIVAN" = "0" ]; then
    unset R3_ALLOCATOR_LABEL || true
    echo "===== divan alloc_bench: alloc-mimalloc (mimalloc v3.3.2) ====="
    cargo bench --bench alloc_bench 2>&1 | tee bench_alloc_mimalloc.log

    export R3_ALLOCATOR_LABEL="rustfs-mimalloc (mimalloc v3.5.3)"
    echo "===== divan alloc_bench: rustfs-mimalloc (mimalloc v3.5.3) ====="
    cargo bench --manifest-path rustfs-bench/Cargo.toml --bench alloc_bench 2>&1 | tee bench_alloc_rustfs.log

    if [ "$FULL_DIVAN" = "1" ]; then
        unset R3_ALLOCATOR_LABEL || true
        echo "===== divan benche: alloc-mimalloc (mimalloc v3.3.2) ====="
        cargo bench --bench benche 2>&1 | tee bench_full_mimalloc.log

        export R3_ALLOCATOR_LABEL="rustfs-mimalloc (mimalloc v3.5.3)"
        echo "===== divan benche: rustfs-mimalloc (mimalloc v3.5.3) ====="
        cargo bench --manifest-path rustfs-bench/Cargo.toml --bench benche 2>&1 | tee bench_full_rustfs.log
    fi
fi

if [ "$SKIP_RUN" = "0" ]; then
    export R3_BENCH_SCALE="$SCALE"
    export R3_BENCH_STYLE="mono"
    unset R3_ALLOCATOR_LABEL || true

    echo "===== run(): alloc-mimalloc (mimalloc v3.3.2), scale=$SCALE ====="
    cargo build --release
    ./target/release/r3 2>&1 | tee bench_run_mimalloc.log

    echo "===== run(): rustfs-mimalloc (mimalloc v3.5.3), scale=$SCALE ====="
    cargo build --release --manifest-path rustfs-bench/Cargo.toml
    ./rustfs-bench/target/release/r3-rustfs 2>&1 | tee bench_run_rustfs.log
fi

echo
echo "Логи: bench_alloc_*.log, bench_full_*.log, bench_run_*.log"
