use core::alloc;
use std::alloc::alloc;

use rallocator::*;
use mimalloc::MiMalloc;
use allocation_hints::heap::*;
use allocation_hints::with_hint;
use bump_scope::{Bump,BumpString};
use allocator_api2::vec::Vec as BumpVec;

//rallocator::rallocator!();
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() {
    for _ in 0..3{
        let mut dalay_vec: Vec<_> = Vec::new();
        for _ in 0..100{
            let start = std::time::Instant::now();
            mimm();
            let delay = start.elapsed().as_micros();
            dalay_vec.push(delay);
        }
        let avg = dalay_vec.iter().sum::<u128>() / (dalay_vec.len() as u128);
        let max = dalay_vec.iter().max().unwrap();
        let min = dalay_vec.iter().min().unwrap();
        println!("MIMALOC:\navg: {}\nmax: {}\nmin: {}",avg,max,min);
    
        let mut dalay_vec: Vec<_> = Vec::new();
        for _ in 0..100{
            let start = std::time::Instant::now();
            bump_scope_m();
            let delay = start.elapsed().as_micros();
            dalay_vec.push(delay);
        }
        let avg = dalay_vec.iter().sum::<u128>() / (dalay_vec.len() as u128);
        let max = dalay_vec.iter().max().unwrap();
        let min = dalay_vec.iter().min().unwrap();
        println!("STR bump:\navg: {}\nmax: {}\nmin: {}",avg,max,min);
        println!("\n-----------------------------------\n");
    }
}

fn bump_scope_m() {
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().unwrap().into() {
            s.spawn(|| {
                // Каждый поток создаёт свой bump-аллокатор
                let mut bump = Bump::with_size(120 * 1024 * 1024);
                
                for _ in 0..3 {
                    // Вектор векторов – используем &bump как аллокатор
                    let capacity = 100 * 100; // 10 000

                    let mut vectr: BumpVec<BumpVec<BumpString<&Bump>, &Bump>, &Bump> =
                        BumpVec::with_capacity_in(capacity, &bump);
                    
                    for _ in 0..100 {
                        for _ in 0..100 {
                            let mut vec: BumpVec<BumpString<&Bump>, &Bump> =
                                BumpVec::with_capacity_in(100, &bump);
                            for _ in 0..100 {
                                vec.push(BumpString::from_str_in("stroka", &bump));
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(vectr);
                    bump.reset();
                }
                // По выходу из потока bump освободится автоматически
            });
        }
    });
}

fn bump_scope_m_heapstr() {
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().unwrap().into() {
            s.spawn(|| {
                // Каждый поток создаёт свой bump-аллокатор
                let bump = Bump::new();
                
                for _ in 0..3 {
                    // Вектор векторов – используем &bump как аллокатор
                    let capacity = 100 * 100; // 10 000

                    let mut vectr: BumpVec<BumpVec<String, &Bump>, &Bump> =
                        BumpVec::with_capacity_in(capacity, &bump);
                    
                    for _ in 0..100 {
                        for _ in 0..100 {
                            let mut vec: BumpVec<String, &Bump> =
                                BumpVec::with_capacity_in(100, &bump);
                            for _ in 0..100 {
                                vec.push("stroka".to_string());
                            }
                            vectr.push(vec);
                        }
                    }
                    core::hint::black_box(vectr);
                }
                // По выходу из потока bump освободится автоматически
            });
        }
    });
}

fn mimm(){
    //let heap = Heap::
    std::thread::scope(|s|{
        for _ in 0..std::thread::available_parallelism().unwrap().into(){
            s.spawn(||{
                for _ in 0..3{
                    let mut vectr:Vec<Vec<String>> = Vec::with_capacity(10000);
                    for _ in 0..100 {
                        for _ in 0..100{
                            let mut vec =  Vec::with_capacity(100);
                            for _ in 0..100{
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

fn r(){
    rallocator::initialize();
    //let heap = Heap::
    let start = std::time::Instant::now();
    std::thread::scope(|s|{
        for _ in 0..std::thread::available_parallelism().unwrap().into(){
            s.spawn(||{
                //let heap = Heap::bump(bump::Options::new());
                let heap = Heap::from_thread_pool(bump::Options::new());
                for _ in 0..3{
                    let mut vectr:Vec<Vec<String>> = with_hint(&heap,|| Vec::with_capacity(32));
                    for _ in 0..100 {
                        for _ in 0..100{
                            let mut vec = with_hint(&heap,|| Vec::with_capacity(128));
                            for _ in 0..100{
                                with_hint(&heap,|| vec.push("stroka".to_string()));
                            }
                            with_hint(&heap,|| vectr.push(vec));
                        }
                    }
                    core::hint::black_box(vectr);
                }
            });
        }
    });
    let end = start.elapsed().as_micros();
    println!("\n{}\n",end);
}
