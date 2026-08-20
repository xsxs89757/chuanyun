//! `chuanyun-server` 命令行入口（薄壳，逻辑在 `cy_server` 库内）。

fn main() -> anyhow::Result<()> {
    println!("chuanyun-server (proto v{})", cy_server::PROTO_VERSION);
    Ok(())
}
