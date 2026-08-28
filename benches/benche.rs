use r3::*;
use std::sync::OnceLock;

/// Количество прогонов (sample count) для всех бенчей.
pub(crate) static RUNS: u32 = 25;

/// PGO-размеры чанков (per-thread), вычисляются один раз и кешируются.
fn pgo() -> (usize, usize, usize, usize) {
    static P: OnceLock<(usize, usize, usize, usize)> = OnceLock::new();
    *P.get_or_init(|| {
        (
            profile_bump_chunk_size_full(),
            profile_bump_chunk_size_light(),
            profile_arena_chunk_size_full(),
            profile_arena_chunk_size_light(),
        )
    })
}

#[hotpath::main]
fn main() {
    pgo(); //прогреваем
    divan::main();
}

// ---------------- MiMalloc (baseline) ----------------
#[divan::bench(name = "mimm/smt", sample_count = RUNS)]
fn mimm_smt() {
    mimm(true);
}

#[divan::bench(name = "mimm/no-smt", sample_count = RUNS)]
fn mimm_no_smt() {
    mimm(false);
}

#[divan::bench(name = "mimm_light/smt", sample_count = RUNS)]
fn mimm_light_smt() {
    mimm_light(true);
}

#[divan::bench(name = "mimm_light/no-smt", sample_count = RUNS)]
fn mimm_light_no_smt() {
    mimm_light(false);
}

// ---------------- Per-thread bumpalo ----------------
#[divan::bench(name = "bump_scope_m/smt", sample_count = RUNS)]
fn bump_scope_m_smt() {
    let (fb, _, _, _) = pgo();
    bump_scope_m(fb, true);
}

#[divan::bench(name = "bump_scope_m/no-smt", sample_count = RUNS)]
fn bump_scope_m_no_smt() {
    let (fb, _, _, _) = pgo();
    bump_scope_m(fb, false);
}

#[divan::bench(name = "bump_scope_m_light/smt", sample_count = RUNS)]
fn bump_scope_m_light_smt() {
    let (_, lb, _, _) = pgo();
    bump_scope_m_light(lb, true);
}

#[divan::bench(name = "bump_scope_m_light/no-smt", sample_count = RUNS)]
fn bump_scope_m_light_no_smt() {
    let (_, lb, _, _) = pgo();
    bump_scope_m_light(lb, false);
}

// ---------------- Shared bump + spin::Mutex ----------------
#[divan::bench(name = "bump_shared_m/smt", sample_count = RUNS)]
fn bump_shared_m_smt() {
    let (fb, _, _, _) = pgo();
    bump_shared_m(fb, true);
}

#[divan::bench(name = "bump_shared_m/no-smt", sample_count = RUNS)]
fn bump_shared_m_no_smt() {
    let (fb, _, _, _) = pgo();
    bump_shared_m(fb, false);
}

#[divan::bench(name = "bump_shared_m_light/smt", sample_count = RUNS)]
fn bump_shared_m_light_smt() {
    let (_, lb, _, _) = pgo();
    bump_shared_m_light(lb, true);
}

#[divan::bench(name = "bump_shared_m_light/no-smt", sample_count = RUNS)]
fn bump_shared_m_light_no_smt() {
    let (_, lb, _, _) = pgo();
    bump_shared_m_light(lb, false);
}

// ---------------- Shared arena (single big buffer) ----------------
#[divan::bench(name = "arena_full/smt", sample_count = RUNS)]
fn arena_full_smt() {
    let (_, _, fa, _) = pgo();
    arena_full(fa, true);
}

#[divan::bench(name = "arena_full/no-smt", sample_count = RUNS)]
fn arena_full_no_smt() {
    let (_, _, fa, _) = pgo();
    arena_full(fa, false);
}

#[divan::bench(name = "arena_light/smt", sample_count = RUNS)]
fn arena_light_smt() {
    let (_, _, _, la) = pgo();
    arena_light(la, true);
}

#[divan::bench(name = "arena_light/no-smt", sample_count = RUNS)]
fn arena_light_no_smt() {
    let (_, _, _, la) = pgo();
    arena_light(la, false);
}

// ---------------- Shared arena: directional fill ----------------
// Forward — эталон (слева направо).
#[divan::bench(name = "arena_full/forward/smt", sample_count = RUNS)]
fn arena_full_forward_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_dir(fa, true, BumpDir::Forward);
}

#[divan::bench(name = "arena_full/forward/no-smt", sample_count = RUNS)]
fn arena_full_forward_no_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_dir(fa, false, BumpDir::Forward);
}

// Backward — справа налево (сосед справа доходит до той же границы).
#[divan::bench(name = "arena_full/backward/smt", sample_count = RUNS)]
fn arena_full_backward_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_dir(fa, true, BumpDir::Backward);
}

#[divan::bench(name = "arena_full/backward/no-smt", sample_count = RUNS)]
fn arena_full_backward_no_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_dir(fa, false, BumpDir::Backward);
}

// MiddleOut — заполнение из середины с чередованием сторон.
#[divan::bench(name = "arena_full/middleout/smt", sample_count = RUNS)]
fn arena_full_middleout_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_dir(fa, true, BumpDir::MiddleOut);
}

#[divan::bench(name = "arena_full/middleout/no-smt", sample_count = RUNS)]
fn arena_full_middleout_no_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_dir(fa, false, BumpDir::MiddleOut);
}

// Neighbors — соседние чанки заполняют общую границу навстречу друг другу.
#[divan::bench(name = "arena_full/neighbors/smt", sample_count = RUNS)]
fn arena_full_neighbors_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_neighbors(fa, true);
}

#[divan::bench(name = "arena_full/neighbors/no-smt", sample_count = RUNS)]
fn arena_full_neighbors_no_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_neighbors(fa, false);
}

// Pair — соседи делят ОДИН регион и берут память друг у друга при переполнении.
#[divan::bench(name = "arena_full/pair/smt", sample_count = RUNS)]
fn arena_full_pair_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_pair(fa, true);
}

#[divan::bench(name = "arena_full/pair/no-smt", sample_count = RUNS)]
fn arena_full_pair_no_smt() {
    let (_, _, fa, _) = pgo();
    arena_full_pair(fa, false);
}

// То же самое для LIGHT-версии.
#[divan::bench(name = "arena_light/forward/smt", sample_count = RUNS)]
fn arena_light_forward_smt() {
    let (_, _, _, la) = pgo();
    arena_light_dir(la, true, BumpDir::Forward);
}

#[divan::bench(name = "arena_light/forward/no-smt", sample_count = RUNS)]
fn arena_light_forward_no_smt() {
    let (_, _, _, la) = pgo();
    arena_light_dir(la, false, BumpDir::Forward);
}

#[divan::bench(name = "arena_light/backward/smt", sample_count = RUNS)]
fn arena_light_backward_smt() {
    let (_, _, _, la) = pgo();
    arena_light_dir(la, true, BumpDir::Backward);
}

#[divan::bench(name = "arena_light/backward/no-smt", sample_count = RUNS)]
fn arena_light_backward_no_smt() {
    let (_, _, _, la) = pgo();
    arena_light_dir(la, false, BumpDir::Backward);
}

#[divan::bench(name = "arena_light/middleout/smt", sample_count = RUNS)]
fn arena_light_middleout_smt() {
    let (_, _, _, la) = pgo();
    arena_light_dir(la, true, BumpDir::MiddleOut);
}

#[divan::bench(name = "arena_light/middleout/no-smt", sample_count = RUNS)]
fn arena_light_middleout_no_smt() {
    let (_, _, _, la) = pgo();
    arena_light_dir(la, false, BumpDir::MiddleOut);
}

#[divan::bench(name = "arena_light/neighbors/smt", sample_count = RUNS)]
fn arena_light_neighbors_smt() {
    let (_, _, _, la) = pgo();
    arena_light_neighbors(la, true);
}

#[divan::bench(name = "arena_light/neighbors/no-smt", sample_count = RUNS)]
fn arena_light_neighbors_no_smt() {
    let (_, _, _, la) = pgo();
    arena_light_neighbors(la, false);
}

#[divan::bench(name = "arena_light/pair/smt", sample_count = RUNS)]
fn arena_light_pair_smt() {
    let (_, _, _, la) = pgo();
    arena_light_pair(la, true);
}

#[divan::bench(name = "arena_light/pair/no-smt", sample_count = RUNS)]
fn arena_light_pair_no_smt() {
    let (_, _, _, la) = pgo();
    arena_light_pair(la, false);
}
