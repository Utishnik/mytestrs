#!/usr/bin/env bash
# Run the benchmark under one of the build presets (1-4) or all of them.
# Builds via build.sh, then runs the produced r3 binary (run() executes the
# benchmark suite). Logs each run to bench_mode<N>.log.
#
# Allocator selection (3rd arg):
#   mimalloc  (default) — пакет r3, глобальный аллокатор rustfs-mimalloc (mimalloc v3.5.3)
#   rustfs              — пакет rustfs-bench (та же rustfs-mimalloc v3.5.3, отдельный бинарь)
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
#   ./bench.sh 1 "" mimalloc --quick   # мелкий бенч: только FULL/LIGHT × SMT/NO SMT
#   ./bench.sh 1 "" mimalloc --tune    # перед прогоном: автоподбор
#                                      # R3_ARENA_CHUNK_MB (arena_tune_scan)
#
# Флаги (порядок не важен):
#   --quick  мелкий бенч (R3_BENCH_QUICK=1)
#   --tune   перед бенчем гоняется автотюн размера per-thread чанка арены
#            (та же процедура, что в bench_arena_profile.sh --tune): cargo bench
#            с фичей tune пробегает сетку чанков и выбирает минимальный
#            без fallback-вызовов; подобранное значение экспортируется как
#            R3_ARENA_CHUNK_MB для всех последующих прогонов скрипта.
#            Тюн меряет нагрузку при текущем R3_BENCH_SCALE/R3_TUNE_ROUNDS —
#            держите их теми же, с которыми пойдёт основной прогон.
set -euo pipefail

MODE="${1:-1}"
STYLE="${2:-}"
ALLOCATOR="${3:-mimalloc}"
QUICK="${4:-}"
TUNE=0
# Флаги можно указывать в любой позиции ($4 сохранён для обратной совместимости).
for arg in "$@"; do
    case "$arg" in
        --quick) QUICK="--quick" ;;
        --tune)  TUNE=1 ;;
    esac
done
TRIPLE="$(rustc --print host-tuple)"

if [ -n "$STYLE" ]; then
    export R3_BENCH_STYLE="$STYLE"
fi

if [ "$QUICK" = "--quick" ]; then
    export R3_BENCH_QUICK=1
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

# --- Автотюн per-thread чанка арены (флаг --tune) -------------------------------
# Прогон НЕ профилируется pprof: его задача — только подобрать размер чанка
# (см. arena_tune_scan). Подобранное R3_ARENA_CHUNK_MB подхватывают как
# моментальный бинарь основной прогоны, так и оба пакета (`r3`/`rustfs-bench`).
if [ "$TUNE" = "1" ]; then
    echo "===== autotune (arena_tune_scan: сетка R3_ARENA_CHUNK_MB -> min fallback_calls; smt=yes, style=${STYLE:-default}) ====="
    TUNE_LOG="$(mktemp "${TMPDIR:-/tmp}/r3-tune.XXXXXX")"
    # shellcheck disable=SC2064
    trap "rm -f '$TUNE_LOG'" EXIT
    R3_TUNE_CHUNKS=1 cargo bench --bench arena_profile --features tune 2>&1 | tee "$TUNE_LOG"

    TUNED_MB="$(sed -n 's/.*chosen chunk=\([0-9]\+\) MB.*/\1/p' "$TUNE_LOG" | tail -1)"
    if [ -z "$TUNED_MB" ]; then
        echo "error: автотюн не выдал результат (не найдена строка '[tune] ... chosen chunk=... MB')." >&2
        echo "       Проверьте, что бенч собирается с фичей tune (cargo bench --bench arena_profile --features tune)." >&2
        exit 1
    fi
    unset R3_TUNE_CHUNKS
    trap - EXIT
    rm -f "$TUNE_LOG"

    export R3_ARENA_CHUNK_MB="$TUNED_MB"
    echo "===== tuned: R3_ARENA_CHUNK_MB=${R3_ARENA_CHUNK_MB} MB ====="
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
