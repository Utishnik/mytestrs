fn main() {
    if std::env::var("R3_PGO_TRAIN").is_ok() {
        r3::pgo_train();
    } else {
        r3::run();
    }
}
