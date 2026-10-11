# Сравнение двух глобальных аллокаторов mimalloc v3 на одних и тех же бенчмарках:
#   1) `mimalloc` crate (libmimalloc-sys, mimalloc v3.3.2) — пакет r3 (default)
#   2) `rustfs-mimalloc` (bindings к mimalloc v3.5.3)      — пакет rustfs-bench
#
# Оба sys-крейта объявляют links = "mimalloc", поэтому сравниваем их двумя
# отдельными бинарниками с одинаковой нагрузкой и профилями сборки.
#
# Usage:
#   powershell -File bench_allocators.ps1                 # alloc_bench + run(), scale=4
#   powershell -File bench_allocators.ps1 -Scale 1        # полная нагрузка
#   powershell -File bench_allocators.ps1 -SkipRun        # только divan alloc_bench
#   powershell -File bench_allocators.ps1 -SkipDivan      # только основной run()
#   powershell -File bench_allocators.ps1 -FullDivan      # + тяжёлый divan-сьют benche
param(
    [int]$Scale = 4,
    [switch]$SkipDivan,
    [switch]$SkipRun,
    [switch]$FullDivan
)

$ErrorActionPreference = "Stop"

$oldScale = $env:R3_BENCH_SCALE
$oldStyle = $env:R3_BENCH_STYLE
$oldLabel = $env:R3_ALLOCATOR_LABEL

try {
    if (-not $SkipDivan) {
        Remove-Item Env:R3_ALLOCATOR_LABEL -ErrorAction SilentlyContinue
        Write-Host "===== divan alloc_bench: alloc-mimalloc (mimalloc v3.3.2) =====" -ForegroundColor Cyan
        & cargo bench --bench alloc_bench 2>&1 |
            Tee-Object -FilePath "bench_alloc_mimalloc.log"

        $env:R3_ALLOCATOR_LABEL = "rustfs-mimalloc (mimalloc v3.5.3)"
        Write-Host "===== divan alloc_bench: rustfs-mimalloc (mimalloc v3.5.3) =====" -ForegroundColor Cyan
        & cargo bench --manifest-path rustfs-bench\Cargo.toml --bench alloc_bench 2>&1 |
            Tee-Object -FilePath "bench_alloc_rustfs.log"

        if ($FullDivan) {
            Remove-Item Env:R3_ALLOCATOR_LABEL -ErrorAction SilentlyContinue
            Write-Host "===== divan benche: alloc-mimalloc (mimalloc v3.3.2) =====" -ForegroundColor Cyan
            & cargo bench --bench benche 2>&1 |
                Tee-Object -FilePath "bench_full_mimalloc.log"

            $env:R3_ALLOCATOR_LABEL = "rustfs-mimalloc (mimalloc v3.5.3)"
            Write-Host "===== divan benche: rustfs-mimalloc (mimalloc v3.5.3) =====" -ForegroundColor Cyan
            & cargo bench --manifest-path rustfs-bench\Cargo.toml --bench benche 2>&1 |
                Tee-Object -FilePath "bench_full_rustfs.log"
        }
    }

    if (-not $SkipRun) {
        $env:R3_BENCH_SCALE = "$Scale"
        $env:R3_BENCH_STYLE = "mono"

        Remove-Item Env:R3_ALLOCATOR_LABEL -ErrorAction SilentlyContinue
        Write-Host "===== run(): alloc-mimalloc (mimalloc v3.3.2), scale=$Scale =====" -ForegroundColor Cyan
        & cargo build --release
        & .\target\release\r3.exe 2>&1 | Tee-Object -FilePath "bench_run_mimalloc.log"

        Write-Host "===== run(): rustfs-mimalloc (mimalloc v3.5.3), scale=$Scale =====" -ForegroundColor Cyan
        & cargo build --release --manifest-path rustfs-bench\Cargo.toml
        & .\rustfs-bench\target\release\r3-rustfs.exe 2>&1 | Tee-Object -FilePath "bench_run_rustfs.log"
    }
}
finally {
    if ($null -ne $oldScale) { $env:R3_BENCH_SCALE = $oldScale } else { Remove-Item Env:R3_BENCH_SCALE -ErrorAction SilentlyContinue }
    if ($null -ne $oldStyle) { $env:R3_BENCH_STYLE = $oldStyle } else { Remove-Item Env:R3_BENCH_STYLE -ErrorAction SilentlyContinue }
    if ($null -ne $oldLabel) { $env:R3_ALLOCATOR_LABEL = $oldLabel } else { Remove-Item Env:R3_ALLOCATOR_LABEL -ErrorAction SilentlyContinue }
}

Write-Host ""
Write-Host "Логи: bench_alloc_*.log, bench_full_*.log, bench_run_*.log" -ForegroundColor Green
