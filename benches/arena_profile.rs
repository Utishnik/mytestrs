//! Профильный прогон ТОЛЬКО shared-арены (без spin-мьютекса).
//!
//! В профиле не должно быть ничего, кроме пути `SharedArena` + per-thread bump:
//!   * НЕ гоняем `bump_shared_m*` — это общий Bump под `SpinMutex` (шум в flamegraph);
//!   * НЕ гоняем `mimm*` / `bump_scope_m*` — глобальный аллокатор и одиночные bump'ы;
//!   * НЕ гоняем directional-стратегии (Backward/MiddleOut/Neighbors/Pair/Donors).
//!
//! Гоняются только Forward-варианты арены: mono (`arena_m_*_forward`) и/или
//! dispatch (`arena_*`), стиль выбирается через `R3_BENCH_STYLE` (по умолчанию both).
//!
//! pprof включается фичей `pprof`: если бинарник собран с `--features pprof`,
//! профайлер стартует сам (файл — по `R3_PPROF_FILE`, формат — по расширению:
//! .svg — flamegraph, .txt/.folded — folded text, .pb — protobuf).
use r3::*;
use std::sync::OnceLock;

/// PGO-размеры чанков арены (per-thread), вычисляются один раз и кешируются.
fn arena_pgo() -> (usize, usize) {
    static P: OnceLock<(usize, usize)> = OnceLock::new();
    *P.get_or_init(|| (profile_arena_chunk_size_full(), profile_arena_chunk_size_light()))
}

#[hotpath::main]
fn main() {
    let _pprof = start_pprof(); // no-op без фичи `pprof`

    println!("Глобальный аллокатор: {}", allocator_label());
    let (fa, la) = arena_pgo();

    let smt = std::env::var("R3_BENCH_SMT").as_deref() != Ok("0");
    let mode_str = if smt { "SMT" } else { "NO SMT" };
    println!("=== Arena-only profile run ({mode_str}) ===");

    let style = std::env::var("R3_BENCH_STYLE").unwrap_or_else(|_| "both".to_string());
    let run_mono = style != "dispatch";
    let run_dispatch = style != "mono";

    // Прогрев выбранных вариантов арены.
    for _ in 0..2 {
        if run_mono {
            arena_m_full_forward(fa, smt);
            arena_m_light_forward(la, smt);
        }
        if run_dispatch {
            arena_full(fa, smt);
            arena_light(la, smt);
        }
    }

    // Основная нагрузка: только арена (Forward), много повторов, чтобы в
    // профиле были хорошо различимые alloc/reset/prefault-пути.
    let rounds = std::env::var("R3_PROFILE_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(5)
        .max(1);
    for round in 0..rounds {
        if run_mono {
            arena_m_full_forward(fa, smt);
            arena_m_light_forward(la, smt);
        }
        if run_dispatch {
            arena_full(fa, smt);
            arena_light(la, smt);
        }
        println!("round {} done", round + 1);
    }

    drop(_pprof); // отчёт пишется в Drop профайлера
    println!("=== arena-only profile run finished ===");
}
