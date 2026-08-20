//! 穿云桌面客户端入口。
//!
//! UI 线程只跑 Slint 事件循环；`cy-core` 在独立线程的 tokio 运行时里跑，
//! 两侧通过事件订阅与命令通道交互（UI 内不做任何网络 IO）。

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    let window = AppWindow::new()?;
    window.set_status(format!("穿云 · 协议 v{}", cy_core::PROTO_VERSION).into());
    window.run()?;
    Ok(())
}
