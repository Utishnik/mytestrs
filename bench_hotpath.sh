#!/usr/bin/env bash
# Build and run the r3 benchmark suite with the `hotpath` profiling feature
# enabled (so #[hotpath::measure] and hotpath::measure_block! actually produce
# a report). Built WITHOUT PGO — hotpath instrumentation would skew PGO profile
# gathering, so keep PGO training runs feature-less.
#
# The report is written to a file (not stdout) so it survives the run.
#
# Usage:
#   ./bench_hotpath.sh                                # table report -> hotpath-report.table
#   ./bench_hotpath.sh --json                         # json report  -> hotpath-report.json
#   ./bench_hotpath.sh --out custom.json              # custom report filename
#   ./bench_hotpath.sh --cpu                          # Linux/macOS only: + hotpath-cpu
#   ./bench_hotpath.sh --style mono|dispatch|both     # arena version to run (default: both)
set -euo pipefail

PREFETCH=0
JSON=0
CPU=0
OUT=""
CAPTURE=0
DIVAN=0
STYLE="both"

while [ $# -gt 0 ]; do
    case "$1" in
        --prefetch)    PREFETCH=1 ;;
        --json)        JSON=1 ;;
        --cpu)         CPU=1 ;;
        --capture)     CAPTURE=1 ;;
        --divan)       DIVAN=1 ;;
        --style)       shift; STYLE="${1:-both}" ;;
        --out)         shift; OUT="${1:-}" ;;
        *) echo "unknown arg: $1"; exit 2 ;;
    esac
    shift
done

FEATURES="hotpath"
if [ "$PREFETCH" = "1" ]; then
    FEATURES="hotpath win-prefetch-pages"
fi
if [ "$CPU" = "1" ]; then
    # hotpath-cpu is Linux/macOS-only (the crate hard-errors on Windows).
    FEATURES="$FEATURES hotpath-cpu"
fi

# Default report filename by format.
if [ -z "$OUT" ]; then
    if [ "$JSON" = "1" ]; then OUT="hotpath-report.json"; else OUT="hotpath-report.table"; fi
fi

echo "===== building (features: $FEATURES) ====="
if [ "$DIVAN" = "1" ]; then
    cargo bench --bench benche --features "$FEATURES"
else
    cargo build --release --features "$FEATURES"
fi

if [ "$JSON" = "1" ]; then
    export HOTPATH_OUTPUT_FORMAT=json
else
    unset HOTPATH_OUTPUT_FORMAT || true
fi
export HOTPATH_OUTPUT_PATH="$(pwd)/${OUT}"
export R3_BENCH_STYLE="$STYLE"

echo "===== hotpath report -> $(pwd)/${OUT} ====="
if [ "$DIVAN" = "1" ]; then
    echo "===== running divan hotpath benchmark ====="
    cargo bench --bench benche --features "$FEATURES" | tee bench_hotpath_divan.log
elif [ "$CAPTURE" = "1" ]; then
    echo "===== running hotpath benchmark (style=$STYLE) ====="
    ./target/release/r3 >/dev/null
else
    echo "===== running hotpath benchmark (style=$STYLE) ====="
    ./target/release/r3 | tee bench_hotpath.log
fi
echo "===== report written to $(pwd)/${OUT} ====="
