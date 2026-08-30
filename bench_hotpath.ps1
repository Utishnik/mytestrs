# Build and run the r3 benchmark suite with the `hotpath` profiling feature
# enabled (so #[hotpath::measure] and hotpath::measure_block! actually produce
# a report). Built WITHOUT PGO — hotpath instrumentation would skew PGO profile
# gathering, so keep PGO training runs feature-less.
#
# The report is written to a file (not stdout) so it survives the run.
#
# Usage:
#   powershell -File bench_hotpath.ps1                        # table report -> hotpath-report.table
#   powershell -File bench_hotpath.ps1 -Json                  # json report   -> hotpath-report.json
#   powershell -File bench_hotpath.ps1 -Out custom.json       # custom report filename
#   powershell -File bench_hotpath.ps1 -Prefetch              # + win-prefetch-pages
#   powershell -File bench_hotpath.ps1 -Capture               # discard bench stdout
#   powershell -File bench_hotpath.ps1 -Style mono            # arena: mono-only (R3_BENCH_STYLE=mono)
#   powershell -File bench_hotpath.ps1 -Style dispatch        # arena: dispatch-only
#   powershell -File bench_hotpath.ps1 -Style both            # arena: mono + dispatch (default)
param(
    [switch]$Prefetch,
    [switch]$Json,
    [string]$Out = "",
    [switch]$Capture,
    [switch]$Divan,
    [ValidateSet('mono', 'dispatch', 'both')][string]$Style = 'both'
)

$ErrorActionPreference = "Stop"

$features = "hotpath"
if ($Prefetch) {
    $features = "hotpath win-prefetch-pages"
}

$ext = if ($Json) { "json" } else { "table" }
if ($Out -eq "") {
    $Out = "hotpath-report.$ext"
}

# --- Build ---------------------------------------------------------------
if ($Divan) {
    Write-Host "===== cargo bench --bench benche --features $features =====" -ForegroundColor Cyan
    & cargo bench --bench benche --features $features
} else {
    Write-Host "===== cargo build --release --features $features =====" -ForegroundColor Cyan
    & cargo build --release --features $features
}

# --- Run with hotpath env vars -------------------------------------------
if ($Json) {
    $env:HOTPATH_OUTPUT_FORMAT = "json"
} else {
    Remove-Item Env:HOTPATH_OUTPUT_FORMAT -ErrorAction SilentlyContinue
}
$env:HOTPATH_OUTPUT_PATH = (Join-Path (Get-Location) $Out)

$reportPath = Join-Path (Get-Location) $Out
Write-Host "===== hotpath report -> $reportPath =====" -ForegroundColor Cyan

if ($Divan) {
    Write-Host "===== Running divan hotpath benchmark =====" -ForegroundColor Cyan
    & cargo bench --bench benche --features $features | Tee-Object -FilePath "bench_hotpath_divan.log"
    return
}

$bin = ".\target\release\r3.exe"
$env:R3_BENCH_STYLE = $Style
Write-Host "===== Running hotpath benchmark (style=$Style) =====" -ForegroundColor Cyan
if ($Capture) {
    & $bin *> $null
} else {
    & $bin | Tee-Object -FilePath "bench_hotpath.log"
}
Write-Host "===== report written to $reportPath =====" -ForegroundColor Green
