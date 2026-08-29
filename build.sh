#!/usr/bin/env bash
# Build presets for the `r3` crate (Linux).
#   Mode 1: normal release build (current, stable).
#   Mode 2: like now, but std+core are compiled for the current CPU
#           (-C target-cpu=native via -Zbuild-std, nightly).
#   Mode 3: PGO (profile-generate -> train -> profile-use).
#   Mode 4: PGO + compile std/core for current CPU.
#
# Usage:
#   ./build.sh 1
#   ./build.sh 2
#   ./build.sh 3
#   ./build.sh 4
set -euo pipefail

BIN="r3"
# Host triple, e.g. x86_64-unknown-linux-gnu. Pass --target explicitly so
# build-std is the ONLY std provider (avoids the duplicate-core lang-item bug
# when building for the host target).
TRIPLE="$(rustc --print host-tuple)"
PROFDIR="$(pwd)/pgo-data"

ensure_components() {
    # These print "info:" to stderr; ignore failures (already installed, etc.).
    rustup component add --toolchain nightly rust-src 2>/dev/null || true
    rustup component add llvm-tools-preview 2>/dev/null || true
    rustup component add --toolchain nightly llvm-tools-preview 2>/dev/null || true
}

get_llvm_profdata() {
    local tc="$1"
    local sysroot
    sysroot="$(rustc ${tc} --print sysroot)"
    echo "${sysroot}/lib/rustlib/${TRIPLE}/bin/llvm-profdata"
}

mode="${1:-1}"

case "$mode" in
    1)
        echo "== Mode 1: normal release build =="
        cargo build --release
        OUT_BIN="./target/release/${BIN}"
        ;;

    2)
        echo "== Mode 2: release + std/core for current CPU (-Zbuild-std, target-cpu=native) =="
        ensure_components
        # --target is required so build-std is the only std provider.
        export RUSTFLAGS="-C target-cpu=native"
        cargo +nightly build -Zbuild-std --target "${TRIPLE}" --profile native
        OUT_BIN="./target/${TRIPLE}/native/${BIN}"
        ;;

    3)
        echo "== Mode 3: PGO =="
        ensure_components
        rm -rf "${PROFDIR}"
        mkdir -p "${PROFDIR}"

        echo "-- PGO: build with profile-generate --"
        export RUSTFLAGS="-C profile-generate=${PROFDIR}"
        cargo build --release

        echo "-- PGO: training (fast representative workload via R3_PGO_TRAIN) --"
        R3_PGO_TRAIN=1 ./target/release/${BIN}

        echo "-- PGO: merge profiles --"
        PROF="$(get_llvm_profdata "")"
        "${PROF}" merge -o "${PROFDIR}/merged.profdata" "${PROFDIR}"

        echo "-- PGO: rebuild with profile-use --"
        export RUSTFLAGS="-C profile-use=${PROFDIR}/merged.profdata"
        cargo build --release
        OUT_BIN="./target/release/${BIN}"
        ;;

    4)
        echo "== Mode 4: PGO + std/core for current CPU =="
        ensure_components
        rm -rf "${PROFDIR}"
        mkdir -p "${PROFDIR}"

        echo "-- PGO+native: build with profile-generate (build-std, target-cpu=native) --"
        export RUSTFLAGS="-C target-cpu=native -C profile-generate=${PROFDIR}"
        cargo +nightly build -Zbuild-std --target "${TRIPLE}" --profile native

        echo "-- PGO+native: training --"
        R3_PGO_TRAIN=1 "./target/${TRIPLE}/native/${BIN}"

        echo "-- PGO+native: merge profiles --"
        PROF="$(get_llvm_profdata "+nightly")"
        "${PROF}" merge -o "${PROFDIR}/merged.profdata" "${PROFDIR}"

        echo "-- PGO+native: rebuild with profile-use --"
        export RUSTFLAGS="-C target-cpu=native -C profile-use=${PROFDIR}/merged.profdata"
        cargo +nightly build -Zbuild-std --target "${TRIPLE}" --profile native
        OUT_BIN="./target/${TRIPLE}/native/${BIN}"
        ;;

    *)
        echo "Unknown mode: $mode (use 1, 2, 3 or 4)" >&2
        exit 1
        ;;
esac

echo "Done. Binary: ${OUT_BIN}"
