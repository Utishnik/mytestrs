#!/usr/bin/env bash
# Профилирование ТОЛЬКО shared-арены (Forward, mono+dispatch) — без spin-мьютекса.
#
# Гоняется бенч `arena_profile` (benches/arena_profile.rs): в профиле нет ни
# bump_shared_m* (SpinMutex), ни mimm*, ни bump_scope*, ни directional-стратегий.
#
# Сохраняет ДВА отчёта pprof (два независимых прогона одним бинарём):
#   * r3-arena-flame.svg  — flamegraph (SVG)
#   * r3-arena-folded.txt — folded text (совместим с inferno/flamegraph.pl)
# Формат выбирается самим кодом по расширению файла (см. PprofGuard::drop).
# Если задан R3_PPROF_FILE (или --pb), он используется как дополнительный/
# единственный выход вместо пары по умолчанию.
#
# Usage:
#   ./bench_arena_profile.sh                 # svg + txt, 999 Hz
#   ./bench_arena_profile.sh --hz 500        # другая частота сэмплов
#   ./bench_arena_profile.sh --style mono    # только мономорфная арена
#   ./bench_arena_profile.sh --style dispatch# только arena_* (runtime-диспетчер)
#   ./bench_arena_profile.sh --rounds 10     # больше нагрузки (дольше профиль)
#   ./bench_arena_profile.sh --scale 2       # вдвое меньше аллокаций (быстрее)
#   ./bench_arena_profile.sh --no-smt        # только физические ядра
#   ./bench_arena_profile.sh --out-prefix P  # P-flame.svg + P-folded.txt
#   ./bench_arena_profile.sh --pb            # дополнительно r3-arena.pb (go tool pprof)
#
# Оптимизация «горячих мест» профиля (mmap/munmap, SpinMutex в grow_fallback):
#   ./bench_arena_profile.sh --chunk-mb 256        # per-thread чанк арены 256 MB
#                                                    вместо PGO-подсчёта (R3_ARENA_CHUNK_MB)
#   ./bench_arena_profile.sh --fallback-chunk-mb 128
#                                                    минимальный fallback-чанк 128 MB
#                                                    (R3_FALLBACK_CHUNK_MB), меньше mmap/локов
#
# Автоматический подбор chunk size (вместо ручного --chunk-mb):
#   ./bench_arena_profile.sh --tune        # сетка размеров -> минимум grow_fallback/mmap,
#                                          # подобранный R3_ARENA_CHUNK_MB подставляется
#                                          # во все прогоны (и в flamegraph тоже)
#   ./bench_arena_profile.sh --tune --tune-rounds 5   # больше повторов на кандидата
#   ./bench_arena_profile.sh --tune --tune-only       # только автотюн: печать
#                                          # подобранного R3_ARENA_CHUNK_MB, без pprof-прогонов
set -euo pipefail

HZ=""
STYLE="both"
ROUNDS=""
SCALE=""
SMT=1
PREFIX="r3-arena"
PB=0
CHUNK_MB=""
FB_CHUNK_MB=""
TUNE=0
TUNE_ROUNDS=""
TUNE_ONLY=0

while [ $# -gt 0 ]; do
    case "$1" in
        --hz)          shift; HZ="${1:-999}" ;;
        --style)       shift; STYLE="${1:-both}" ;;
        --rounds)      shift; ROUNDS="${1:-5}" ;;
        --scale)       shift; SCALE="${1:-1}" ;;
        --no-smt)      SMT=0 ;;
        --pb)          PB=1 ;;
        --chunk-mb)         shift; CHUNK_MB="${1:?--chunk-mb требует число МБ}" ;;
        --fallback-chunk-mb) shift; FB_CHUNK_MB="${1:?--fallback-chunk-mb требует число МБ}" ;;
        --out-prefix)  shift; PREFIX="${1:-r3-arena}" ;;
        --tune)        TUNE=1 ;;
        --tune-rounds) shift; TUNE_ROUNDS="${1:?--tune-rounds требует число}" ;;
        --tune-only)   TUNE=1; TUNE_ONLY=1 ;;
        -h|--help)     awk 'NR>1 && /^#/ {sub(/^# ?/, ""); print; next} NR>1 && !/^#/ {exit}' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

case "$STYLE" in
    mono|dispatch|both) ;;
    *) echo "bad --style: $STYLE (expected: mono|dispatch|both)" >&2; exit 2 ;;
esac

if [ "$(uname)" != "Linux" ] && [ "$(uname)" != "Darwin" ]; then
    echo "error: pprof feature is unix-only (current OS: $(uname))" >&2
    exit 1
fi

SVG="${PREFIX}-flame.svg"
TXT="${PREFIX}-folded.txt"
PBFILE="${PREFIX}.pb"

OUTS=("$SVG" "$TXT")
[ "$PB" = "1" ] && OUTS+=("$PBFILE")

export R3_BENCH_STYLE="$STYLE"
[ -n "$HZ" ]     && export R3_PPROF_HZ="$HZ"
[ -n "$ROUNDS" ] && export R3_PROFILE_ROUNDS="$ROUNDS"
[ -n "$SCALE" ]  && export R3_BENCH_SCALE="$SCALE"
[ "$SMT" = "0" ] && export R3_BENCH_SMT=0
# Оптимизация hotspots из профиля: крупные чанки вместо частых mmap/munmap и
# захватов SpinMutex в grow_fallback (см. fallback_chunk_bytes / arena_chunk_override).
[ -n "$CHUNK_MB" ]    && export R3_ARENA_CHUNK_MB="$CHUNK_MB"
[ -n "$FB_CHUNK_MB" ] && export R3_FALLBACK_CHUNK_MB="$FB_CHUNK_MB"
[ -n "$TUNE_ROUNDS" ] && export R3_TUNE_ROUNDS="$TUNE_ROUNDS"

# Фича `tune` включает счётчик grow_fallback + скан arena_tune_scan внутри
# бенча (срабатывает при R3_TUNE_CHUNKS=1, задаётся ниже). Отдельный tune-прогон
# НЕ профилируется pprof: его задача — только подобрать размер чанка; подобранный
# R3_ARENA_CHUNK_MB печатается в stdout и подхватывается скриптом для всех
# последующих (чистых, без фичи tune) профилей.
FEATURES=(pprof)
if [ "$TUNE" = "1" ]; then
    FEATURES+=(tune)
    export R3_TUNE_CHUNKS=1
fi

# --- Прогон автотюна -----------------------------------------------------------
# Скрипт сам парсит вывод `[tune] ... chosen chunk=N MB`, чтобы tune-лог не
# попал в профили и чтобы финальные прогоны собирались БЕЗ фичи tune (бинарь
# идентичен обычному профилю, только с подобранным чанком).
if [ "$TUNE" = "1" ]; then
    echo "===== autotune run (grid R3_ARENA_CHUNK_MB -> min grow_fallback/mmap; style=$STYLE${TUNE_ROUNDS:+, tune_rounds=$TUNE_ROUNDS}${SMT:+, smt=$SMT}) ====="
    TUNE_LOG="$(mktemp "${TMPDIR:-/tmp}/r3-tune.XXXXXX")"
    # shellcheck disable=SC2064
    trap "rm -f '$TUNE_LOG'" EXIT
    cargo bench --profile profiling --features "${FEATURES[*]}" --bench arena_profile 2>&1 | tee "$TUNE_LOG"
    # pipefail выше гарантирует, что сюда мы попадаем только при успехе cargo.

    TUNED_MB="$(sed -n 's/.*chosen chunk=\([0-9]\+\) MB.*/\1/p' "$TUNE_LOG" | tail -1)"
    if [ -z "$TUNED_MB" ]; then
        echo "error: автотюн не выдал результат (не найдена строка '[tune] ... chosen chunk=... MB')." >&2
        echo "       Проверь, что бинарь собирается с фичей tune и что в бенче есть arena_tune_scan." >&2
        exit 1
    fi
    if [ -n "$CHUNK_MB" ]; then
        echo "===== note: --chunk-mb=$CHUNK_MB переопределён автотюном: R3_ARENA_CHUNK_MB=$TUNED_MB MB ====="
    fi
    export R3_ARENA_CHUNK_MB="$TUNED_MB"
    unset R3_TUNE_CHUNKS
    echo "===== tuned: R3_ARENA_CHUNK_MB=${R3_ARENA_CHUNK_MB} MB ====="
    rm -f "$TUNE_LOG"
    trap - EXIT

    if [ "$TUNE_ONLY" = "1" ]; then
        echo "===== done (--tune-only): подобран R3_ARENA_CHUNK_MB=${R3_ARENA_CHUNK_MB} MB; pprof-прогоны пропущены ====="
        exit 0
    fi
fi

# Один и тот же бинарь (profile profiling + feature pprof) гоняется отдельно
# ради каждого выхода: отдельные прогоны = чистые независимые профили.
for out in "${OUTS[@]}"; do
    export R3_PPROF_FILE="$out"
    echo "===== arena-only profile run -> $out (style=$STYLE${HZ:+, hz=$HZ}${ROUNDS:+, rounds=$ROUNDS}${SCALE:+, scale=$SCALE}${R3_ARENA_CHUNK_MB:+, chunk_mb=$R3_ARENA_CHUNK_MB}${FB_CHUNK_MB:+, fb_chunk_mb=$FB_CHUNK_MB}, smt=$SMT) ====="
    cargo bench --profile profiling --features pprof --bench arena_profile
    echo "===== wrote $out ====="
done

unset R3_PPROF_FILE
echo "===== done: ${OUTS[*]} ====="
