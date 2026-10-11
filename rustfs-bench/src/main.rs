#[global_allocator]
static GLOBAL: rustfs_mimalloc::MiMalloc = rustfs_mimalloc::MiMalloc;

fn mi_version_label() -> String {
    let v = rustfs_mimalloc::MiMalloc::version();
    format!(
        "rustfs-mimalloc (mimalloc v{}.{}.{})",
        v / 10000,
        (v / 100) % 100,
        v % 100
    )
}

fn main() {
    std::env::set_var("R3_ALLOCATOR_LABEL", mi_version_label());
    if std::env::var("R3_PGO_TRAIN").is_ok() {
        r3::pgo_train();
    } else {
        r3::run();
    }
}
