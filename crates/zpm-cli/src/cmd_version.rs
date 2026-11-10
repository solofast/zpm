//! `zpm version`：打印版本号。

pub fn run() {
    println!("zpm v{}", env!("CARGO_PKG_VERSION"));
}
