//! 穿云桌面客户端。
//!
//! # 两个线程怎么配合
//!
//! Slint 的事件循环必须待在主线程，而隧道引擎要一个 tokio 运行时。两者互不驱动，
//! 所以各跑各的：
//!
//! - **主线程**：Slint 事件循环，只做界面。
//! - **后台线程**：tokio 运行时，跑 [`cy_core::Engine`] 和本地 API。
//!
//! 界面 → 引擎：回调里往命令通道 `try_send`，绝不在 UI 线程上 await。
//! 引擎 → 界面：`slint::invoke_from_event_loop` 把闭包投递回主线程再改属性。
//!
//! 这条边界要守住：一旦在 UI 回调里做网络 IO，界面就会在最不该卡的时候卡住。

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod bridge;
mod tray;

use std::sync::Arc;

use cy_core::Engine;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    let state_path = cy_core::State::default_path();

    // 配置文件藏在系统的应用支持目录里，路径还带反写域名前缀，没人猜得到。
    // 排查问题时第一句话往往就是「你的配置在哪」，给它一个能直接回答的出口。
    if std::env::args().any(|a| a == "--print-state-path") {
        match &state_path {
            Some(p) => println!("{}", p.display()),
            None => {
                eprintln!("定位不到配置目录");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cy_core=info".into()),
        )
        .init();

    if let Some(p) = &state_path {
        tracing::info!(path = %p.display(), "配置文件");
    }

    // 后台线程跑 tokio；引擎和本地 API 都在里面
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?,
    );

    let brand = cy_core::brand::embedded();
    let engine = runtime.block_on(async { Engine::start(state_path, brand.clone()) });

    // 本地 API：项目脚本靠它注册端口、查地址
    {
        let engine = engine.clone();
        let port = cy_core::local_api::DEFAULT_PORT;
        runtime.spawn(async move {
            if let Err(e) = cy_core::local_api::serve(engine, port).await {
                // 端口被占是常见情况（比如开了两个穿云），说清楚就好，别让应用起不来
                tracing::warn!(port, error = %e, "本地 API 没能启动；脚本接入功能不可用");
            }
        });
    }

    let window = AppWindow::new()?;
    let tray = tray::setup(&window)?;

    bridge::wire(&window, &tray, engine.clone(), runtime.clone());

    window.run()?;

    // 关窗不等于退出（有托盘），真正退出时才收尾
    runtime.block_on(async { engine.shutdown().await });
    Ok(())
}
