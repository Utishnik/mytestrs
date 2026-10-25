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
#   ./bench_profile.sh --half                   # pprof: half sampling freq (~500 Hz)
#                                               #   AND half memory footprint (R3_BENCH_SCALE=2)
#   ./bench_profile.sh --style mono|dispatch|both        # arena version to run
#   ./bench_profile.sh --prefetch               # hotpath build: + win-prefetch-pages
#   ./bench_profile.sh --cpu                    # hotpath: + hotpath-cpu (Linux/macOS only)
#   ./bench_profile.sh --capture                # discard bench stdout (no tee)
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
HALF=0                 # pprof: halve sampling freq and memory footprint
CAPTURE=0
CPU=0
PREFETCH=0
STYLE="both"

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
        --half)            HALF=1 ;;
        --style)           shift; STYLE="${1:-both}" ;;
        --prefetch)        PREFETCH=1 ;;
        --cpu)             CPU=1 ;;
        --capture)         CAPTURE=1 ;;
        -h|--help)         sed -n '2,$p' "$0" | grep '^#' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

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

run_pprof_once() {
    local fmt="$1" file="$2"
    export R3_PPROF_FILE="$file"
    [ -n "$HZ" ] && export R3_PPROF_HZ="$HZ" || unset R3_PPROF_HZ 2>/dev/null || true
    # --half: в 2 раза меньше сэмплов (частота /2) и в 2 раза меньше выделяемой
    # памяти бенча (R3_BENCH_SCALE=2 делит все аллокационные циклы пополам).
    if [ "$HALF" = "1" ]; then
        export R3_PPROF_HALF=1
        export R3_BENCH_SCALE="${R3_BENCH_SCALE:-2}"
    else
        unset R3_PPROF_HALF 2>/dev/null || true
    fi
    echo "===== running pprof bench ($fmt) -> $file ====="
    if [ "$CAPTURE" = "1" ]; then
        ./target/profiling/r3 >/dev/null
    else
        ./target/profiling/r3 | tee "bench_pprof_${fmt}.log"
    fi
    echo "===== pprof ($fmt) written to $(pwd)/$file ====="
}

run_hotpath_once() {
    local hp_features="hotpath"
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
        ./target/release/r3 >/dev/null
    else
        ./target/release/r3 | tee bench_hotpath.log
    fi
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
