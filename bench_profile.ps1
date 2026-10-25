# Unified profiling launcher for the r3 benchmark suite (PowerShell version).
# Supports BOTH profilers:
#   * pprof   — CPU sampling profiler (unix only! On Windows it is skipped with a warning)
#   * hotpath — per-function timings (feature `hotpath`, release build)
#
# Output format is selectable:
#   pprof  -> text (folded .txt), flamegraph (.svg), protobuf (.pb), or all
#   hotpath-> table report or json report
#
# Usage:
#   powershell -File bench_profile.ps1                          # hotpath (table) by default;
#                                                               # pprof auto-enabled on unix
#   powershell -File bench_profile.ps1 -PprofOnly               # only pprof
#   powershell -File bench_profile.ps1 -HotpathOnly             # only hotpath
#   powershell -File bench_profile.ps1 -Format text|flamegraph|pb|all
#   powershell -File bench_profile.ps1 -HotpathFormat json
#   powershell -File bench_profile.ps1 -Out r3.txt              # pprof filename override
#   powershell -File bench_profile.ps1 -HotpathOut hp.json      # hotpath filename override
#   powershell -File bench_profile.ps1 -Hz 999                  # pprof sampling frequency
#   powershell -File bench_profile.ps1 -Style mono|dispatch|both
#   powershell -File bench_profile.ps1 -Prefetch                # + win-prefetch-pages
#   powershell -File bench_profile.ps1 -Cpu                     # + hotpath-cpu (Linux/macOS only)
#   powershell -File bench_profile.ps1 -Capture                 # discard bench stdout
#   powershell -File bench_profile.ps1 -Quick                   # быстрый прогон (R3_BENCH_QUICK)
#   powershell -File bench_profile.ps1 -Scale 8                 # уменьшить нагрузку в N раз
#   powershell -File bench_profile.ps1 -Full                    # полный прогон (без защиты от OOM)
#
# ВАЖНО: на машинах с малым объёмом RAM полный профильный прогон убивается
# OOM-killer'ом ("Terminated"). По умолчанию скрипт включает Quick + Scale 8.
param(
    [switch]$PprofOnly,
    [switch]$HotpathOnly,
    [ValidateSet('text', 'flamegraph', 'pb', 'all')][string]$Format = 'text',
    [ValidateSet('table', 'json')][string]$HotpathFormat = 'table',
    [string]$Out = '',
    [string]$HotpathOut = '',
    [int]$Hz = 0,
    [ValidateSet('mono', 'dispatch', 'both')][string]$Style = 'both',
    [switch]$Prefetch,
    [switch]$Cpu,
    [switch]$Capture,
    [switch]$Quick,
    [int]$Scale = 0,
    [switch]$Full
)

$ErrorActionPreference = "Stop"

# Авто-защита от OOM: если не попросили -Full, режем нагрузку.
if (-not $Full) {
    $Quick = $true
    if ($Scale -eq 0) { $Scale = 8 }
}
if ($Scale -gt 0) { $env:R3_BENCH_SCALE = "$Scale" } else { Remove-Item Env:R3_BENCH_SCALE -ErrorAction SilentlyContinue }
if ($Quick) {
    $env:R3_BENCH_QUICK = "1"
    Write-Host "===== режим: QUICK (R3_BENCH_QUICK=1), scale=$(if ($Scale -gt 0) { $Scale } else { 1 }) — передайте -Full для полного прогона =====" -ForegroundColor Yellow
} else {
    Remove-Item Env:R3_BENCH_QUICK -ErrorAction SilentlyContinue
}

$doPprof = if ($PprofOnly) { $true } elseif ($HotpathOnly) { $false } else { $true }
$doHotpath = if ($HotpathOnly) { $true } elseif ($PprofOnly) { $false } else { $true }

$isUnix = ($IsLinux -or $IsMacOS)

function Get-PprofExt([string]$fmt) {
    switch ($fmt) {
        'text'       { 'txt' }
        'flamegraph' { 'svg' }
        'pb'         { 'pb' }
    }
}

function Invoke-PprofRun([string]$fmt, [string]$file) {
    $env:R3_PPROF_FILE = $file
    if ($Hz -gt 0) { $env:R3_PPROF_HZ = "$Hz" } else { Remove-Item Env:R3_PPROF_HZ -ErrorAction SilentlyContinue }
    Write-Host "===== running pprof bench ($fmt) -> $file =====" -ForegroundColor Cyan
    $bin = ".\target\profiling\r3.exe"
    if (-not (Test-Path $bin)) { $bin = "./target/profiling/r3" }
    if ($Capture) {
        & $bin *> $null
        $rc = $LASTEXITCODE
    } else {
        & $bin | Tee-Object -FilePath "bench_pprof_$fmt.log"
        $rc = $LASTEXITCODE
    }
    if ($rc -ne 0) {
        Write-Host "!!! pprof bench завершился с кодом $rc (137/128+SIGKILL на unix = OOM; уменьшите нагрузку: -Scale 16)" -ForegroundColor Red
        exit $rc
    }
    Write-Host "===== pprof ($fmt) written to $((Get-Location).Path)\$file =====" -ForegroundColor Green
}

# --- pprof part -----------------------------------------------------------
if ($doPprof) {
    if (-not $isUnix) {
        Write-Warning "pprof feature is unix-only; skipping pprof on Windows."
    } else {
        Write-Host "===== building pprof bench (--profile profiling --features pprof) =====" -ForegroundColor Cyan
        & cargo build --profile profiling --features pprof

        if ($Format -eq 'all') {
            foreach ($f in @('text', 'flamegraph', 'pb')) {
                Invoke-PprofRun $f "r3-pprof.$(Get-PprofExt $f)"
            }
        } else {
            if ($Out -eq '') { $Out = "r3-pprof.$(Get-PprofExt $Format)" }
            Invoke-PprofRun $Format $Out
        }
    }
}

# --- hotpath part ----------------------------------------------------------
if ($doHotpath) {
    $features = "hotpath"
    if ($Prefetch) { $features = "hotpath win-prefetch-pages" }
    if ($Cpu)      { $features = "$features hotpath-cpu" }

    if ($HotpathOut -eq '') {
        $ext = if ($HotpathFormat -eq 'json') { 'json' } else { 'table' }
        $HotpathOut = "hotpath-report.$ext"
    }

    Write-Host "===== building hotpath bench (features: $features) =====" -ForegroundColor Cyan
    & cargo build --release --features $features

    if ($HotpathFormat -eq 'json') {
        $env:HOTPATH_OUTPUT_FORMAT = "json"
    } else {
        Remove-Item Env:HOTPATH_OUTPUT_FORMAT -ErrorAction SilentlyContinue
    }
    $env:HOTPATH_OUTPUT_PATH = (Join-Path (Get-Location) $HotpathOut)
    $env:R3_BENCH_STYLE = $Style

    Write-Host "===== running hotpath bench (style=$Style) -> $HotpathOut =====" -ForegroundColor Cyan
    $bin = ".\target\release\r3.exe"
    if (-not (Test-Path $bin)) { $bin = "./target/release/r3" }
    if ($Capture) {
        & $bin *> $null
        $rc = $LASTEXITCODE
    } else {
        & $bin | Tee-Object -FilePath "bench_hotpath.log"
        $rc = $LASTEXITCODE
    }
    if ($rc -ne 0) {
        Write-Host "!!! hotpath bench завершился с кодом $rc (OOM? уменьшите нагрузку: -Scale 16)" -ForegroundColor Red
        exit $rc
    }
    Write-Host "===== hotpath report written to $((Get-Location).Path)\$HotpathOut =====" -ForegroundColor Green
}

Write-Host "===== done =====" -ForegroundColor Green
