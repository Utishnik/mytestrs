use std::alloc::{Layout, alloc, dealloc};
use std::hint::black_box;

/// Количество прогонов (sample count) для всех бенчей.
pub(crate) static RUNS: u32 = 50;

#[inline]
fn layout(size: usize) -> Layout {
    Layout::from_size_align(size, 16).unwrap()
}

// ---------------- alloc + dealloc (raw) ----------------

#[divan::bench(
    args = [16, 32, 64, 128, 256, 512, 1024, 4096, 65536],
    sample_count = RUNS
)]
fn alloc_free(size: usize) {
    unsafe {
        let l = layout(size);
        let p = alloc(l);
        black_box(p);
        dealloc(p, l);
    }
}

// ---------------- batch: N allocs, потом N frees ----------------

#[divan::bench(args = [64, 1024, 16384], sample_count = RUNS)]
fn batch_alloc_free(count: usize) {
    unsafe {
        let l = layout(64);
        let mut ptrs = Vec::with_capacity(count);
        for _ in 0..count {
            ptrs.push(alloc(l));
        }
        black_box(&ptrs);
        for p in ptrs {
            dealloc(p, l);
        }
    }
}

// ---------------- batch, размеры перемешаны ----------------

#[divan::bench(args = [64, 1024, 16384], sample_count = RUNS)]
fn batch_mixed_sizes(count: usize) {
    let sizes = [16, 48, 96, 256, 768, 2048, 8192];
    unsafe {
        let mut ptrs = Vec::with_capacity(count);
        for i in 0..count {
            let l = layout(sizes[i % sizes.len()]);
            ptrs.push((alloc(l), l));
        }
        black_box(&ptrs);
        for (p, l) in ptrs {
            dealloc(p, l);
        }
    }
}

// ---------------- Vec / String через глобальный аллокатор ----------------

#[divan::bench(args = [100, 10_000], sample_count = RUNS)]
fn vec_push(count: i32) {
    let mut v = Vec::new();
    for i in 0..count {
        v.push(i);
    }
    black_box(&v);
}

#[divan::bench(args = [100, 10_000], sample_count = RUNS)]
fn string_churn(count: usize) {
    let mut v = Vec::with_capacity(count);
    for _ in 0..count {
        v.push("stroka".to_string());
    }
    black_box(&v);
}

// ---------------- Полная рабочая нагрузка как в mimm ----------------

#[divan::bench(args = [40, 200], sample_count = RUNS)]
fn vecs_of_strings(n: usize) {
    let mut outer: Vec<Vec<String>> = Vec::with_capacity(n * n);
    for _ in 0..n {
        for _ in 0..n {
            let mut vec = Vec::with_capacity(400);
            for _ in 0..100 {
                vec.push("stroka".to_string());
            }
            outer.push(vec);
        }
    }
    black_box(&outer);
}

// ---------------- Multithreaded ----------------

fn mt_batch_mixed(count_per_thread: usize) {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(move || batch_mixed_sizes(count_per_thread));
        }
    });
}

#[divan::bench(args = [1024, 16384], sample_count = RUNS)]
fn mt_batch_mixed_1t(count: usize) {
    batch_mixed_sizes(count);
}

#[divan::bench(args = [1024, 16384], sample_count = RUNS)]
fn mt_batch_mixed_all_threads(count: usize) {
    mt_batch_mixed(count);
}

fn mt_string_churn(count_per_thread: usize) {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(move || string_churn(count_per_thread));
        }
    });
}

#[divan::bench(args = [10_000], sample_count = RUNS)]
fn mt_string_churn_all_threads(count: usize) {
    mt_string_churn(count);
}
