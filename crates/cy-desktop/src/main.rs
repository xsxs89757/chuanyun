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
#[cfg(target_os = "macos")]
mod dock;
mod instance;
mod quit;
mod tray;

use std::sync::Arc;

use cy_core::local_api::AppHooks;
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

    // 同一份配置只许一个穿云在跑。关窗只是收进托盘，再双击快捷方式时，要把已经在跑
    // 的那个叫出来，而不是再起一个去撞同名隧道。见 instance 模块。
    let port = state_path
        .as_deref()
        .map(|p| cy_core::State::load(p).settings.local_api_port)
        .unwrap_or(cy_core::local_api::DEFAULT_PORT);
    let lock_path = state_path
        .as_ref()
        .map(|p| p.with_file_name("instance.lock"));
    let startup = instance::claim(&runtime, lock_path.as_deref(), port);
    // 锁要攥到进程结束，由系统放开
    let (_instance_lock, api_listener) = match startup {
        instance::Startup::Primary { lock, listener } => (lock, listener),
        instance::Startup::AlreadyRunning => std::process::exit(0),
    };

    let brand = cy_core::brand::embedded();
    let engine = runtime.block_on(async { Engine::start(state_path, brand.clone()) });

    let window = AppWindow::new()?;
    let tray = tray::setup(&window)?;

    bridge::wire(&window, &tray, engine.clone(), runtime.clone());

    // 本地 API：项目脚本靠它注册端口、查地址；另一个穿云启动时也靠它找到我们。
    // 放在窗口建好之后，「把窗口叫出来」才有窗口可叫。
    match api_listener {
        Some(listener) => {
            let hooks = AppHooks {
                show: Some({
                    let weak = window.as_weak();
                    Arc::new(move || {
                        tracing::info!("另一个穿云启动了，把窗口叫到前面");
                        let _ = weak.upgrade_in_event_loop(|w| bridge::bring_to_front(&w));
                    })
                }),
                quit: Some({
                    let engine = engine.clone();
                    let handle = runtime.handle().clone();
                    Arc::new(move || {
                        tracing::info!("新版本要接班，退出");
                        quit::request(&engine, &handle);
                    })
                }),
            };
            let engine = engine.clone();
            runtime.spawn(async move {
                if let Err(e) = cy_core::local_api::serve_on(listener, engine, hooks).await {
                    tracing::warn!(error = %e, "本地 API 停了；脚本接入功能不可用");
                }
            });
        }
        None => tracing::warn!(port, "本地 API 没能启动；脚本接入功能不可用"),
    }

    // 关窗之后点 Dock 图标要能把窗口叫回来。winit 把 NSApplication 的 delegate
    // 装在事件循环启动那一刻，所以要先 show 一次把 AppKit 初始化完，再去挂。
    window.show()?;
    #[cfg(target_os = "macos")]
    dock::install(&window);

    // 调试用：收到 SIGUSR1 就把窗口藏起来，等价于点关闭按钮。
    // 没有辅助功能权限时没法用脚本点按钮，验证「关窗后点 Dock 能叫回来」要靠它。
    #[cfg(all(debug_assertions, unix))]
    {
        let weak = window.as_weak();
        runtime.spawn(async move {
            let mut sig =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                    .expect("注册 SIGUSR1");
            while sig.recv().await.is_some() {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let _ = w.hide();
                    }
                });
            }
        });
    }

    slint::run_event_loop()?;

    // 关窗不等于退出（有托盘），走到这里才是真退出：断开连接、收托盘图标、结束进程
    quit::finish(&runtime, &engine, tray)
}
