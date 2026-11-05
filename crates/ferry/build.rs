//! 把编译目标三元组注入 env（update.rs 用作 manifest 平台 key）。
//! 与 release.yml 的资产命名 ferry-<tag>-<target> 一一对应。

fn main() {
    let target = std::env::var("TARGET").expect("cargo always sets TARGET");
    println!("cargo:rustc-env=FERRY_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
