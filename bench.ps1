# Run the benchmark under one of the build presets (1-4) or all of them.
# Builds via build.ps1, then runs the produced r3 binary (run() executes the
# benchmark suite). Logs each run to bench_mode<N>.log.
#
# Allocator selection:
#   -Allocator mimalloc  (default) — `mimalloc` crate, mimalloc v3.3.2 (пакет r3)
#   -Allocator rustfs              — `rustfs-mimalloc`, mimalloc v3.5.3 (пакет rustfs-bench)
#   -Allocator both                — обе конфигурации подряд
#
# Usage:
#   powershell -File bench.ps1 1
#   powershell -File bench.ps1 2
#   powershell -File bench.ps1 3
#   powershell -File bench.ps1 4
#   powershell -File bench.ps1 all
#   powershell -File bench.ps1 -Style mono/dispatch/both   # arena version to run (default: both)
#   powershell -File bench.ps1 1 -Allocator rustfs         # общий бенч на mimalloc v3.5.3
#   powershell -File bench.ps1 1 -Allocator both           # прямое сравнение
param(
    [ValidateSet(1, 2, 3, 4, 'all')]$mode = 1,
    [ValidateSet('mono', 'dispatch', 'both')][string]$Style = 'both',
    [ValidateSet('mimalloc', 'rustfs', 'both')][string]$Allocator = 'mimalloc'
)

$ErrorActionPreference = "Stop"
$triple = (rustc --print host-tuple).Trim()

function BinFor($m, $a) {
    $pkgDir = if ($a -eq 'rustfs') { 'rustfs-bench' } else { '' }
    $binName = if ($a -eq 'rustfs') { 'r3-rustfs' } else { 'r3' }
    if ($m -eq 2 -or $m -eq 4) { return ".\$(if ($pkgDir) { "$pkgDir\" })target\$triple\native\$binName.exe" }
    return ".\$(if ($pkgDir) { "$pkgDir\" })target\release\$binName.exe"
}

$modes = if ($mode -eq 'all') { @(1, 2, 3, 4) } else { @([int]$mode) }
$allocators = if ($Allocator -eq 'both') { @('mimalloc', 'rustfs') } else { @($Allocator) }

foreach ($a in $allocators) {
    $package = if ($a -eq 'rustfs') { 'rustfs-bench' } else { 'r3' }
    $logSuffix = if ($a -eq 'rustfs') { '_rustfs' } else { '' }

    foreach ($m in $modes) {
        Write-Host "===== Build (mode $m, package $package) =====" -ForegroundColor Cyan
        & powershell -File build.ps1 -mode $m -package $package

        $bin = BinFor $m $a
        Write-Host "===== Running benchmark (mode $m, style=$Style, allocator=$a) =====" -ForegroundColor Cyan
        $env:R3_BENCH_STYLE = $Style
        if ($a -eq 'rustfs') {
            $env:R3_ALLOCATOR_LABEL = "rustfs-mimalloc (mimalloc v3.5.3)"
        } else {
            Remove-Item Env:R3_ALLOCATOR_LABEL -ErrorAction SilentlyContinue
        }
        & $bin | Tee-Object -FilePath "bench_mode$m$logSuffix.log"
    }
}
