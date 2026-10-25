#!/usr/bin/env bash
# Unified profiling launcher for the r3 benchmark suite.
# Supports BOTH profilers at once:
#   * pprof   — CPU sampling profiler (unix only, feature `pprof`, profile `profiling`)
#   * hotpath — per-function timings (feature `hotpath`, release build)
#
# Output format is selectable for each profiler:
#   pprof  -> text (folded .txt), flamegraph (.svg), protobuf (.pb)
#   hotpath-> table report or json report
#
# Usage:
#   ./bench_profile.sh                          # both profilers, defaults:
#                                               #   pprof   -> r3-pprof.txt (folded text)
#                                               #   hotpath -> hotpath-report.table
#   ./bench_profile.sh --pprof-only             # only pprof
#   ./bench_profile.sh --hotpath-only           # only hotpath
#   ./bench_profile.sh --format text|flamegraph|pb|all   # pprof output format
#                                               #   all = .txt + .svg + .pb (three runs)
#   ./bench_profile.sh --hotpath-format table|json       # hotpath report format
#   ./bench_profile.sh --out FILE               # pprof output filename override
#   ./bench_profile.sh --hotpath-out FILE       # hotpath report filename override
#   ./bench_profile.sh --hz 999                 # pprof sampling frequency
#   ./bench_profile.sh --style mono|dispatch|both        # arena version to run
#   ./bench_profile.sh --prefetch               # hotpath build: + win-prefetch-pages
#   ./bench_profile.sh --cpu                    # hotpath: + hotpath-cpu (Linux/macOS only)
#   ./bench_profile.sh --capture                # discard bench stdout (no tee)
#   ./bench_profile.sh --quick                  # быстрый прогон (R3_BENCH_QUICK=1):
#                                               #   только базовые FULL/LIGHT × SMT/NO SMT
#   ./bench_profile.sh --scale N                # уменьшить нагрузку в N раз
#                                               #   (R3_BENCH_SCALE=N, см. src/lib.rs)
#   ./bench_profile.sh --full                   # полный прогон БЕЗ защиты от OOM
#
# ВАЖНО про "Terminated" (OOM):
#   Полный профильный прогон спавнит по потоку на каждое логическое ядро и
#   держит сотни МБ живых аллокаций одновременно. На машине с 2-4 ГБ RAM
#   (в т.ч. WSL2 с дефолтным лимитом памяти) процесс убивает OOM-killer
#   прямо на первом бенче ("SMT (all logical cores)"). Поэтому скрипт по
#   умолчанию включает --quick + --scale 8. Для полного прогона на большой
#   машине передайте --full; при нехватке памяти повышайте --scale (16, 32)
#   или ограничьте ядра: taskset -c 0-3 ./bench_profile.sh ...
#
# Notes:
#   * pprof needs a separate binary (built with --profile profiling --features pprof),
#     because the `pprof` and `hotpath` features require different build settings
#     (debug symbols vs. plain release). This script builds/runs both sequentially.
#   * Built WITHOUT PGO on purpose — instrumentation would skew PGO training data.
set -euo pipefail

PPROF=1
HOTPATH=1
FORMAT="text"          # pprof format: text | flamegraph | pb | all
HP_FORMAT="table"      # hotpath format: table | json
OUT=""                 # pprof output file override
HP_OUT=""              # hotpath output file override
HZ=""                  # pprof sampling frequency
CAPTURE=0
CPU=0
PREFETCH=0
STYLE="both"
QUICK=0                # включается автоматически, если не задан --full
SCALE=""               # пусто = авто (8), если не задан --full
FULL=0

while [ $# -gt 0 ]; do
    case "$1" in
        --pprof-only)      PPROF=1; HOTPATH=0 ;;
        --hotpath-only)    PPROF=0; HOTPATH=1 ;;
        --both)            PPROF=1; HOTPATH=1 ;;
        --format)          shift; FORMAT="${1:-text}" ;;
        --hotpath-format)  shift; HP_FORMAT="${1:-table}" ;;
        --out)             shift; OUT="${1:-}" ;;
        --hotpath-out)     shift; HP_OUT="${1:-}" ;;
        --hz)              shift; HZ="${1:-999}" ;;
        --style)           shift; STYLE="${1:-both}" ;;
        --prefetch)        PREFETCH=1 ;;
        --cpu)             CPU=1 ;;
        --capture)         CAPTURE=1 ;;
        --quick)           QUICK=1 ;;
        --scale)           shift; SCALE="${1:-8}" ;;
        --full)            FULL=1 ;;
        -h|--help)         sed -n '2,$p' "$0" | grep '^#' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

# Авто-защита от OOM: без --full режем нагрузку (quick + scale 8).
if [ "$FULL" = "0" ]; then
    QUICK=1
    [ -z "$SCALE" ] && SCALE=8
fi

if [ -n "$SCALE" ]; then
    case "$SCALE" in (*[!0-9]*|'') echo "bad --scale: $SCALE (expected integer >= 1)" >&2; exit 2;; esac
    if [ "$SCALE" -ge 1 ]; then export R3_BENCH_SCALE="$SCALE"; else unset R3_BENCH_SCALE 2>/dev/null || true; fi
else
    unset R3_BENCH_SCALE 2>/dev/null || true
fi

if [ "$QUICK" = "1" ]; then
    export R3_BENCH_QUICK=1
    echo "===== режим: QUICK (R3_BENCH_QUICK=1), scale=${R3_BENCH_SCALE:-1} — для полного прогона передайте --full ====="
else
    unset R3_BENCH_QUICK 2>/dev/null || true
fi

case "$FORMAT" in text|flamegraph|pb|all) ;; *)
    echo "bad --format: $FORMAT (expected: text|flamegraph|pb|all)" >&2; exit 2 ;;
esac
case "$HP_FORMAT" in table|json) ;; *)
    echo "bad --hotpath-format: $HP_FORMAT (expected: table|json)" >&2; exit 2 ;;
esac

# Map pprof format -> file extension (the Rust code picks the format by extension:
#   .txt/.folded -> folded text, .pb -> protobuf, anything else -> SVG flamegraph).
ext_for_format() {
    case "$1" in
        text)       echo "txt" ;;
        flamegraph) echo "svg" ;;
        pb)         echo "pb" ;;
    esac
}

# Возвращает 0 если процесс завершился нормально; печатает подсказку про OOM
# при смерти по сигналу (137 = SIGKILL, обычно OOM-killer; 143 = SIGTERM).
check_rc() {
    local rc="$1" name="$2"
    if [ "$rc" -eq 137 ] || [ "$rc" -eq 143 ]; then
        echo "!!! $name убит сигналом (exit=$rc) — почти наверняка OOM-killer." >&2
        echo "!!! Уменьшите нагрузку: --scale 16 (или 32)." >&2
        echo "!!! Ограничьте ядра: taskset -c 0-3 ./bench_profile.sh ..." >&2
        echo "!!! WSL2: поднимите память в %UserProfile%\.wslconfig ([wsl2] memory=8GB swap=8GB) и перезапустите wsl --shutdown." >&2
        return 1
    fi
    if [ "$rc" -ne 0 ]; then
        echo "!!! $name завершился с кодом $rc" >&2
        return 1
    fi
    return 0
}

run_pprof_once() {
    local fmt="$1" file="$2" rc=0
    export R3_PPROF_FILE="$file"
    [ -n "$HZ" ] && export R3_PPROF_HZ="$HZ" || unset R3_PPROF_HZ 2>/dev/null || true
    echo "===== running pprof bench ($fmt) -> $file ====="
    if [ "$CAPTURE" = "1" ]; then
        ./target/profiling/r3 >/dev/null || rc=$?
    else
        ./target/profiling/r3 | tee "bench_pprof_${fmt}.log" || rc=${PIPESTATUS[0]}
    fi
    check_rc "$rc" "pprof bench" || return 1
    echo "===== pprof ($fmt) written to $(pwd)/$file ====="
}

run_hotpath_once() {
    local hp_features="hotpath" rc=0
    [ "$PREFETCH" = "1" ] && hp_features="hotpath win-prefetch-pages"
    [ "$CPU" = "1" ] && hp_features="$hp_features hotpath-cpu"

    if [ -z "$HP_OUT" ]; then
        if [ "$HP_FORMAT" = "json" ]; then HP_OUT="hotpath-report.json"; else HP_OUT="hotpath-report.table"; fi
    fi

    echo "===== building hotpath bench (features: $hp_features) ====="
    cargo build --release --features "$hp_features"

    if [ "$HP_FORMAT" = "json" ]; then
        export HOTPATH_OUTPUT_FORMAT=json
    else
        unset HOTPATH_OUTPUT_FORMAT 2>/dev/null || true
    fi
    export HOTPATH_OUTPUT_PATH="$(pwd)/${HP_OUT}"
    export R3_BENCH_STYLE="$STYLE"

    echo "===== running hotpath bench (style=$STYLE) -> $HP_OUT ====="
    if [ "$CAPTURE" = "1" ]; then
        ./target/release/r3 >/dev/null || rc=$?
    else
        ./target/release/r3 | tee bench_hotpath.log || rc=${PIPESTATUS[0]}
    fi
    check_rc "$rc" "hotpath bench" || return 1
    echo "===== hotpath report written to $(pwd)/${HP_OUT} ====="
}

# --- pprof part -----------------------------------------------------------
if [ "$PPROF" = "1" ]; then
    if [ "$(uname)" != "Linux" ] && [ "$(uname)" != "Darwin" ]; then
        echo "warning: pprof feature is unix-only; skipping pprof on $(uname)" >&2
    else
        echo "===== building pprof bench (--profile profiling --features pprof) ====="
        cargo build --profile profiling --features pprof

        if [ "$FORMAT" = "all" ]; then
            for f in text flamegraph pb; do
                run_pprof_once "$f" "r3-pprof.$(ext_for_format "$f")"
            done
        else
            if [ -z "$OUT" ]; then
                OUT="r3-pprof.$(ext_for_format "$FORMAT")"
            fi
            run_pprof_once "$FORMAT" "$OUT"
        fi
    fi
fi

# --- hotpath part ----------------------------------------------------------
if [ "$HOTPATH" = "1" ]; then
    run_hotpath_once
fi

echo "===== done ====="
