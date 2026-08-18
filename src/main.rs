use bumpalo::Bump;
use bumpalo::collections::String as BumpString;
use bumpalo::collections::Vec as BumpVec;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    let all_cores = core_affinity::get_core_ids().unwrap();
    let logical_cores = all_cores.len();
    let physical_cores = num_cpus::get_physical();
    println!("Логических ядер: {}", logical_cores);
    println!("Физических ядер: {}", physical_cores);
    println!();

    // ---------- PGO для полной версии ----------
    let mut pgo_full_res: Vec<u128> = Vec::new();
    for _ in 0..3 {
        let pgo = profile_bump_chunk_size_full() as u128;
        pgo_full_res.push(pgo);
    }
    let pgo_full = median(&pgo_full_res) as usize;
    println!(
        "PGO full version recommended chunk size: {} MB",
        pgo_full / (1024 * 1024)
    );

    // ---------- PGO для лайт версии ----------
    let mut pgo_light_res: Vec<u128> = Vec::new();
    for _ in 0..3 {
        let pgo = profile_bump_chunk_size_light() as u128;
        pgo_light_res.push(pgo);
    }
    let pgo_light = median(&pgo_light_res) as usize;
    println!(
        "PGO light version recommended chunk size: {} MB",
        pgo_light / (1024 * 1024)
    );

    // ========== Запуск с SMT (все логические ядра) ==========
    run_benchmarks(true, pgo_full, pgo_light);

    // ========== Запуск без SMT (только физические ядра) ==========
    run_benchmarks(false, pgo_full, pgo_light);
}

/// Выполняет полный цикл бенчмарков (прогрев + замеры) для заданного режима SMT.
fn run_benchmarks(smt: bool, pgo_full: usize, pgo_light: usize) {
    let mode_str = if smt {
        "SMT (all logical cores)"
    } else {
        "NO SMT (physical cores only)"
    };
    println!("\n########## Режим: {} ##########\n", mode_str);

    // ---------- Прогрев полной версии ----------
    for _ in 0..5 {
        mimm(smt);
        bump_scope_m(pgo_full, smt);
    }

    // ---------- Прогрев лайт версии ----------
    for _ in 0..5 {
        mimm_light(smt);
        bump_scope_m_light(pgo_light, smt);
    }

    // ---------- Тест полной версии ----------
    println!("=== FULL VERSION ({}) ===", mode_str);
    for round in 0..3 {
        let mimm_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                mimm(smt);
                start.elapsed().as_micros()
            })
            .collect();
        let mimm_median = median(&mimm_times);

        let bump_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                bump_scope_m(pgo_full, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let bump_median = median(&bump_times);

        println!(
            "Round {}: MIMALOC median = {} µs, Bump median = {} µs",
            round + 1,
            mimm_median,
            bump_median
        );
    }

    // ---------- Тест лайт версии ----------
    println!("\n=== LIGHT VERSION ({}) ===", mode_str);
    for round in 0..3 {
        let mimm_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                mimm_light(smt);
                start.elapsed().as_micros()
            })
            .collect();
        let mimm_median = median(&mimm_times);

        let bump_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                bump_scope_m_light(pgo_light, smt);
                start.elapsed().as_micros()
            })
            .collect();
        let bump_median = median(&bump_times);

        println!(
            "Round {}: MIMALOC median = {} µs, Bump median = {} µs",
            round + 1,
            mimm_median,
            bump_median
        );
    }
}

// ==================== Утилиты ====================

fn median(times: &[u128]) -> u128 {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

/// Возвращает список ядер в зависимости от режима SMT.
fn get_cores(smt: bool) -> Vec<core_affinity::CoreId> {
    let all = core_affinity::get_core_ids().unwrap();
    if smt {
        all
    } else {
        // Предполагаем, что физические ядра идут первыми (чётные индексы)
        all.into_iter().step_by(2).collect()
    }
}

// ==================== Полная версия ====================

/// PGO для полной версии: одна итерация, замер объёма, запас 20%.
fn profile_bump_chunk_size_full() -> usize {
    let mut bump = Bump::new();

    let capacity = 4 * 100 * 100; // 40 000
    let mut vectr: BumpVec<BumpVec<BumpString>> = BumpVec::with_capacity_in(capacity, &bump);

    for _ in 0..200 {
        for _ in 0..200 {
            let mut vec: BumpVec<BumpString> = BumpVec::with_capacity_in(400, &bump);
            for _ in 0..100 {
                vec.push(BumpString::from_str_in("stroka", &bump));
            }
            vectr.push(vec);
        }
    }

    let used = bump.allocated_bytes();
    core::hint::black_box(vectr);
    bump.reset();

    let recommended = used * 120 / 100;
    let rounded = ((recommended + (1024 * 1024 - 1)) / (1024 * 1024)) * (1024 * 1024);
    rounded
}

fn bump_scope_m(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                let mut bump = Bump::with_capacity(chunk_size);

                for _ in 0..3 {
                    let capacity = 4 * 100 * 100; // 40 000
                    let mut vectr: BumpVec<BumpVec<BumpString>> =
                        BumpVec::with_capacity_in(capacity, &bump);

                    for _ in 0..200 {
                        for _ in 0..200 {
                            let mut vec: BumpVec<BumpString> =
                                BumpVec::with_capacity_in(400, &bump);
                            for _ in 0..100 {
                                vec.push(BumpString::from_str_in("stroka", &bump));
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(vectr);
                    bump.reset();
                }
            });
        }
    });
}

fn mimm(smt: bool) {
    let core_ids = get_cores(smt);
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..3 {
                    let mut vectr: Vec<Vec<String>> = Vec::with_capacity(40000);
                    for _ in 0..200 {
                        for _ in 0..200 {
                            let mut vec = Vec::with_capacity(400);
                            for _ in 0..100 {
                                vec.push("stroka".to_string());
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(vectr);
                }
            });
        }
    });
}

// ==================== Лайт версия ====================

/// PGO для лайт версии: циклы 100×100, capacity внешнего вектора 10 000.
fn profile_bump_chunk_size_light() -> usize {
    let mut bump = Bump::new();

    let capacity = 100 * 100; // 10 000
    let mut vectr: BumpVec<BumpVec<BumpString>> = BumpVec::with_capacity_in(capacity, &bump);

    for _ in 0..100 {
        for _ in 0..100 {
            let mut vec: BumpVec<BumpString> = BumpVec::with_capacity_in(400, &bump);
            for _ in 0..100 {
                vec.push(BumpString::from_str_in("stroka", &bump));
            }
            vectr.push(vec);
        }
    }

    let used = bump.allocated_bytes();
    core::hint::black_box(vectr);
    bump.reset();

    let recommended = used * 120 / 100;
    let rounded = ((recommended + (1024 * 1024 - 1)) / (1024 * 1024)) * (1024 * 1024);
    rounded
}

fn bump_scope_m_light(chunk_size: usize, smt: bool) {
    let core_ids = get_cores(smt);
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                let mut bump = Bump::with_capacity(chunk_size);

                for _ in 0..3 {
                    let capacity = 100 * 100; // 10 000
                    let mut vectr: BumpVec<BumpVec<BumpString>> =
                        BumpVec::with_capacity_in(capacity, &bump);

                    for _ in 0..100 {
                        for _ in 0..100 {
                            let mut vec: BumpVec<BumpString> =
                                BumpVec::with_capacity_in(400, &bump);
                            for _ in 0..100 {
                                vec.push(BumpString::from_str_in("stroka", &bump));
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(vectr);
                    bump.reset();
                }
            });
        }
    });
}

fn mimm_light(smt: bool) {
    let core_ids = get_cores(smt);
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                for _ in 0..3 {
                    let mut vectr: Vec<Vec<String>> = Vec::with_capacity(10_000);
                    for _ in 0..100 {
                        for _ in 0..100 {
                            let mut vec = Vec::with_capacity(400);
                            for _ in 0..100 {
                                vec.push("stroka".to_string());
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(vectr);
                }
            });
        }
    });
}
