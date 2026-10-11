#[global_allocator]
static GLOBAL: rustfs_mimalloc::MiMalloc = rustfs_mimalloc::MiMalloc;

include!("../../benches/alloc_workload.rs");

#[hotpath::main]
fn main() {
    let v = rustfs_mimalloc::MiMalloc::version();
    println!(
        "Глобальный аллокатор: rustfs-mimalloc (mimalloc v{}.{}.{})",
        v / 10000,
        (v / 100) % 100,
        v % 100
    );
    divan::main();
}
