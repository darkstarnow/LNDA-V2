use std::path::Path;

fn main() {
    let tables = Path::new("Source").join("BcPackTables.bin");
    println!("cargo::rustc-check-cfg=cfg(leonida_tables)");
    println!("cargo::rerun-if-changed={}", tables.display());
    if tables.is_file() {
        println!("cargo::rustc-cfg=leonida_tables");
    }
}
