# Run the benchmark under one of the build presets (1-4) or all of them.
# Builds via build.ps1, then runs the produced r3 binary (run() executes the
# benchmark suite). Logs each run to bench_mode<N>.log.
#
# Usage:
#   powershell -File bench.ps1 1
#   powershell -File bench.ps1 2
#   powershell -File bench.ps1 3
#   powershell -File bench.ps1 4
#   powershell -File bench.ps1 all
#   powershell -File bench.ps1 -Style mono/dispatch/both   # arena version to run (default: both)
param(
    [ValidateSet(1, 2, 3, 4, 'all')]$mode = 1,
    [ValidateSet('mono', 'dispatch', 'both')][string]$Style = 'both'
)

$ErrorActionPreference = "Stop"
$triple = (rustc --print host-tuple).Trim()

function BinFor($m) {
    if ($m -eq 2 -or $m -eq 4) { return ".\target\$triple\native\r3.exe" }
    return ".\target\release\r3.exe"
}

$modes = if ($mode -eq 'all') { @(1, 2, 3, 4) } else { @([int]$mode) }

foreach ($m in $modes) {
    Write-Host "===== Build (mode $m) =====" -ForegroundColor Cyan
    & powershell -File build.ps1 -mode $m

    $bin = BinFor $m
    Write-Host "===== Running benchmark (mode $m, style=$Style) =====" -ForegroundColor Cyan
    $env:R3_BENCH_STYLE = $Style
    & $bin | Tee-Object -FilePath "bench_mode$m.log"
}
