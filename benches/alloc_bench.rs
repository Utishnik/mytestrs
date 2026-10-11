include!("alloc_workload.rs");

#[hotpath::main]
fn main() {
    println!("Глобальный аллокатор: {}", r3::allocator_label());
    divan::main();
}
