use r3::*;
use std::sync::OnceLock;

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
    divan::main();
}

// ---------------- MiMalloc (baseline) ----------------
#[divan::bench(name = "mimm/smt", sample_count = 10)]
fn mimm_smt() {
    mimm(true);
}

#[divan::bench(name = "mimm/no-smt", sample_count = 10)]
fn mimm_no_smt() {
    mimm(false);
}

#[divan::bench(name = "mimm_light/smt", sample_count = 10)]
fn mimm_light_smt() {
    mimm_light(true);
}

#[divan::bench(name = "mimm_light/no-smt", sample_count = 10)]
fn mimm_light_no_smt() {
    mimm_light(false);
}

// ---------------- Per-thread bumpalo ----------------
#[divan::bench(name = "bump_scope_m/smt", sample_count = 10)]
fn bump_scope_m_smt() {
    let (fb, _, _, _) = pgo();
    bump_scope_m(fb, true);
}

#[divan::bench(name = "bump_scope_m/no-smt", sample_count = 10)]
fn bump_scope_m_no_smt() {
    let (fb, _, _, _) = pgo();
    bump_scope_m(fb, false);
}

#[divan::bench(name = "bump_scope_m_light/smt", sample_count = 10)]
fn bump_scope_m_light_smt() {
    let (_, lb, _, _) = pgo();
    bump_scope_m_light(lb, true);
}

#[divan::bench(name = "bump_scope_m_light/no-smt", sample_count = 10)]
fn bump_scope_m_light_no_smt() {
    let (_, lb, _, _) = pgo();
    bump_scope_m_light(lb, false);
}

// ---------------- Shared bump + spin::Mutex ----------------
#[divan::bench(name = "bump_shared_m/smt", sample_count = 10)]
fn bump_shared_m_smt() {
    let (fb, _, _, _) = pgo();
    bump_shared_m(fb, true);
}

#[divan::bench(name = "bump_shared_m/no-smt", sample_count = 10)]
fn bump_shared_m_no_smt() {
    let (fb, _, _, _) = pgo();
    bump_shared_m(fb, false);
}

#[divan::bench(name = "bump_shared_m_light/smt", sample_count = 10)]
fn bump_shared_m_light_smt() {
    let (_, lb, _, _) = pgo();
    bump_shared_m_light(lb, true);
}

#[divan::bench(name = "bump_shared_m_light/no-smt", sample_count = 10)]
fn bump_shared_m_light_no_smt() {
    let (_, lb, _, _) = pgo();
    bump_shared_m_light(lb, false);
}

// ---------------- Shared arena (single big buffer) ----------------
#[divan::bench(name = "arena_full/smt", sample_count = 10)]
fn arena_full_smt() {
    let (_, _, fa, _) = pgo();
    arena_full(fa, true);
}

#[divan::bench(name = "arena_full/no-smt", sample_count = 10)]
fn arena_full_no_smt() {
    let (_, _, fa, _) = pgo();
    arena_full(fa, false);
}

#[divan::bench(name = "arena_light/smt", sample_count = 10)]
fn arena_light_smt() {
    let (_, _, _, la) = pgo();
    arena_light(la, true);
}

#[divan::bench(name = "arena_light/no-smt", sample_count = 10)]
fn arena_light_no_smt() {
    let (_, _, _, la) = pgo();
    arena_light(la, false);
}
