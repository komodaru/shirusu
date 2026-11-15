fn main() {
    // 本体の file を #[path] で読み込むときの目印 (std 前提のテストを外す)。
    println!("cargo:rustc-cfg=shirusu_loom");
}
