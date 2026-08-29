# Build presets for the `r3` crate.
#   Mode 1: normal release build (current, stable).
#   Mode 2: like now, but std+core are compiled for the current CPU
#           (-C target-cpu=native via -Zbuild-std, nightly).
#   Mode 3: PGO (profile-generate -> train -> profile-use).
#   Mode 4: PGO + compile std/core for current CPU.
#
# Usage:
#   powershell -File build.ps1 -mode 1
#   powershell -File build.ps1 -mode 2
#   powershell -File build.ps1 -mode 3
#   powershell -File build.ps1 -mode 4
param(
    [ValidateSet(1, 2, 3, 4)][int]$mode = 1,
    [string]$bin = "r3",
    [string]$triple = "x86_64-pc-windows-msvc"
)

$ErrorActionPreference = "Stop"

function Ensure-Components {
    # rustup prints "info:" to stderr; route through cmd so PowerShell
    # does not treat it as an error.
    cmd /c "rustup component add --toolchain nightly rust-src 2>nul"
    cmd /c "rustup component add llvm-tools-preview 2>nul"
    cmd /c "rustup component add --toolchain nightly llvm-tools-preview 2>nul"
}

function Get-LlvmProfdata($toolchain) {
    $sysroot = (rustc $toolchain --print sysroot).Trim()
    $p = Join-Path $sysroot "lib\rustlib\$triple\bin\llvm-profdata.exe"
    if (-not (Test-Path $p)) {
        throw "llvm-profdata.exe not found in $p. Run: rustup component add llvm-tools-preview"
    }
    return $p
}

$profDir = Join-Path $PWD "pgo-data"

switch ($mode) {
    1 {
        Write-Host "== Mode 1: normal release build ==" -ForegroundColor Cyan
        cargo build --release
        $outBin = ".\target\release\$bin.exe"
    }

    2 {
        Write-Host "== Mode 2: release + std/core for current CPU (-Zbuild-std, target-cpu=native) ==" -ForegroundColor Cyan
        Ensure-Components
        # --target is required so build-std is the ONLY std provider (avoids
        # duplicate core lang-item when building for the host target).
        $env:RUSTFLAGS = "-C target-cpu=native"
        cargo +nightly build -Zbuild-std --target $triple --profile native
        $outBin = ".\target\$triple\native\$bin.exe"
    }

    3 {
        Write-Host "== Mode 3: PGO ==" -ForegroundColor Cyan
        Ensure-Components
        if (Test-Path $profDir) { Remove-Item $profDir -Recurse -Force }
        New-Item -ItemType Directory -Path $profDir | Out-Null

        Write-Host "-- PGO: build with profile-generate --"
        $env:RUSTFLAGS = "-C profile-generate=$profDir"
        cargo build --release

        Write-Host "-- PGO: training (fast representative workload via R3_PGO_TRAIN) --"
        $env:R3_PGO_TRAIN = "1"
        & ".\target\release\$bin.exe"
        Remove-Item Env:R3_PGO_TRAIN

        Write-Host "-- PGO: merge profiles --"
        $prof = Get-LlvmProfdata ""
        & $prof merge -o (Join-Path $profDir "merged.profdata") "$profDir"

        Write-Host "-- PGO: rebuild with profile-use --"
        $env:RUSTFLAGS = "-C profile-use=$(Join-Path $profDir 'merged.profdata')"
        cargo build --release
        $outBin = ".\target\release\$bin.exe"
    }

    4 {
        Write-Host "== Mode 4: PGO + std/core for current CPU ==" -ForegroundColor Cyan
        Ensure-Components
        if (Test-Path $profDir) { Remove-Item $profDir -Recurse -Force }
        New-Item -ItemType Directory -Path $profDir | Out-Null

        Write-Host "-- PGO+native: build with profile-generate (build-std, target-cpu=native) --"
        $env:RUSTFLAGS = "-C target-cpu=native -C profile-generate=$profDir"
        cargo +nightly build -Zbuild-std --target $triple --profile native

        Write-Host "-- PGO+native: training --"
        $env:R3_PGO_TRAIN = "1"
        & ".\target\$triple\native\$bin.exe"
        Remove-Item Env:R3_PGO_TRAIN

        Write-Host "-- PGO+native: merge profiles --"
        $prof = Get-LlvmProfdata "+nightly"
        & $prof merge -o (Join-Path $profDir "merged.profdata") "$profDir"

        Write-Host "-- PGO+native: rebuild with profile-use --"
        $env:RUSTFLAGS = "-C target-cpu=native -C profile-use=$(Join-Path $profDir 'merged.profdata')"
        cargo +nightly build -Zbuild-std --target $triple --profile native
        $outBin = ".\target\$triple\native\$bin.exe"
    }
}

Write-Host ("Done. Binary: " + $outBin) -ForegroundColor Green
