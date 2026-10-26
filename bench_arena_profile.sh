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
set -euo pipefail

HZ=""
STYLE="both"
ROUNDS=""
SCALE=""
SMT=1
PREFIX="r3-arena"
PB=0

while [ $# -gt 0 ]; do
    case "$1" in
        --hz)          shift; HZ="${1:-999}" ;;
        --style)       shift; STYLE="${1:-both}" ;;
        --rounds)      shift; ROUNDS="${1:-5}" ;;
        --scale)       shift; SCALE="${1:-1}" ;;
        --no-smt)      SMT=0 ;;
        --pb)          PB=1 ;;
        --out-prefix)  shift; PREFIX="${1:-r3-arena}" ;;
        -h|--help)     sed -n '2,$p' "$0" | grep '^#' | sed 's/^# \{0,1\}//'; exit 0 ;;
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

# Один и тот же бинарь (profile profiling + feature pprof) гоняется отдельно
# ради каждого выхода: отдельные прогоны = чистые независимые профили.
for out in "${OUTS[@]}"; do
    export R3_PPROF_FILE="$out"
    echo "===== arena-only profile run -> $out (style=$STYLE${HZ:+, hz=$HZ}${ROUNDS:+, rounds=$ROUNDS}${SCALE:+, scale=$SCALE}, smt=$SMT) ====="
    cargo bench --profile profiling --features pprof --bench arena_profile
    echo "===== wrote $(pwd)/$out ====="
done

unset R3_PPROF_FILE
echo "===== done: ${OUTS[*]} ====="
