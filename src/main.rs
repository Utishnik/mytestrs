use bumpalo::Bump;
use bumpalo::collections::String as BumpString;
use bumpalo::collections::Vec as BumpVec;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    // --- PGO: определяем оптимальный размер чанка ---
    let mut pgo_res: Vec<_> = Vec::new();
    for _ in 0..3 {
        let pgo = profile_bump_chunk_size() as u128;
        pgo_res.push(pgo);
    }
    println!(
        "PGO recommended bump chunk size: {} MB",
        median(pgo_res.as_slice()) / (1024 * 1024)
    );
    let pgo = median(pgo_res.as_slice()) as usize;

    // Прогрев
    for _ in 0..5 {
        mimm();
        bump_scope_m(pgo);
    }

    for round in 0..3 {
        let mimm_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                mimm();
                start.elapsed().as_micros()
            })
            .collect();
        let mimm_median = median(&mimm_times);

        let bump_times: Vec<u128> = (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                bump_scope_m(pgo);
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

fn median(times: &[u128]) -> u128 {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

/// Выполняет одну итерацию нагрузки с Bump::new() и возвращает
/// рекомендуемый размер чанка (реальное использование + 20% запас,
/// округлённый до мегабайта).
fn profile_bump_chunk_size() -> usize {
    let mut bump = Bump::new();

    // Одна итерация (аналогично телу bump_scope_m, но без reset)
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

    // Фиксируем использованный объём до сброса
    let used = bump.allocated_bytes();
    core::hint::black_box(vectr); // предотвращаем оптимизацию

    bump.reset(); // освобождаем память

    // Добавляем 20% запаса и округляем до 1 МБ
    let recommended = used * 120 / 100;
    let rounded = ((recommended + (1024 * 1024 - 1)) / (1024 * 1024)) * (1024 * 1024);
    rounded
}

fn bump_scope_m(chunk_size: usize) {
    let core_ids = core_affinity::get_core_ids().unwrap();
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(move || {
                core_affinity::set_for_current(*core_id);
                // Используем переданный размер чанка
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

fn mimm() {
    let core_ids = core_affinity::get_core_ids().unwrap();
    std::thread::scope(|s| {
        for core_id in core_ids.iter() {
            s.spawn(|| {
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
