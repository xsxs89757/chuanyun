//! 把品牌配置的路径传给编译期的 `include_str!`。
//!
//! 默认用仓库里的 `brand/default.toml`（通用示例）。部署方用
//! `CHUANYUN_BRAND=brand/company.toml cargo build` 换成自己的，
//! 产出的就是「同事装上就能登录」的内部版。

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/cy-core 应当在仓库的两层之下");

    let brand = match std::env::var("CHUANYUN_BRAND") {
        Ok(p) => {
            let path = PathBuf::from(&p);
            if path.is_absolute() {
                path
            } else {
                repo_root.join(path)
            }
        }
        Err(_) => repo_root.join("brand/default.toml"),
    };

    if !brand.exists() {
        panic!(
            "找不到品牌配置 {}。\n\
             检查 CHUANYUN_BRAND 的路径，或者用默认的 brand/default.toml。",
            brand.display()
        );
    }

    println!("cargo:rustc-env=CHUANYUN_BRAND_FILE={}", brand.display());
    println!("cargo:rerun-if-env-changed=CHUANYUN_BRAND");
    println!("cargo:rerun-if-changed={}", brand.display());
}
