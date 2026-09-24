//! 退出应用。
//!
//! 关窗不是退出（有托盘）。真正的退出只有两个入口：托盘菜单的「退出」，和新版本
//! 接班时经本地 API 发来的 `/api/quit`。两个入口走同一条路。
//!
//! 以前的退法是让事件循环返回、`main` 正常 return：投一条停引擎的命令（不等它执行），
//! 然后析构一堆东西——tokio 运行时析构时会无限期地等后台的阻塞任务，命令没轮到执行
//! 的话连接也没好好关掉。同事那边看到的就是托盘里点了「退出」进程却还在，或者退掉
//! 马上重开还是撞名。
//!
//! 现在：
//!
//! 1. 一发起就开始断开引擎——不依赖界面线程，界面卡住也照样断；
//! 2. 事件循环返回后，最多再等两秒让连接关干净，收掉托盘图标，直接结束进程，
//!    不走析构；
//! 3. 看门狗兜底：不管哪一步卡住，到点都结束进程。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cy_core::Engine;

/// 引擎断开最多等多久。连着的时候几毫秒就完；正卡在连服务器的握手里就不值得多等——
/// 进程一结束，系统会替我们把连接断掉。
const ENGINE_GRACE: Duration = Duration::from_secs(2);

/// 看门狗：从发起退出算起，到点无论如何结束进程。
const WATCHDOG: Duration = Duration::from_secs(5);

static QUITTING: AtomicBool = AtomicBool::new(false);

/// 发起退出。可以从任何线程调，重复调用无害。
pub fn request(engine: &Engine, runtime: &tokio::runtime::Handle) {
    begin(engine, runtime);
    // 让 main 里的事件循环返回，收尾在 finish 里做
    let _ = slint::quit_event_loop();
}

/// 事件循环返回之后的收尾。不会返回。
pub fn finish(runtime: &tokio::runtime::Runtime, engine: &Engine, tray: crate::TrayIcon) -> ! {
    // 事件循环也可能是自己结束的（窗口和托盘图标都没了），同样要走完退出流程
    begin(engine, runtime.handle());
    runtime.block_on(async {
        if tokio::time::timeout(ENGINE_GRACE, engine.shutdown())
            .await
            .is_err()
        {
            tracing::warn!("引擎没在限时内断开，直接退出");
        }
    });
    // 托盘图标得自己收：直接结束进程的话 Windows 不会替我们删，
    // 它会一直挂在那儿，等鼠标划过才消失
    let _ = tray.hide();
    drop(tray);
    tracing::info!("已退出");
    std::process::exit(0)
}

fn begin(engine: &Engine, runtime: &tokio::runtime::Handle) {
    if QUITTING.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!("正在退出");

    let engine = engine.clone();
    runtime.spawn(async move { engine.shutdown().await });

    std::thread::spawn(|| {
        std::thread::sleep(WATCHDOG);
        tracing::warn!("退出流程超时，直接结束进程");
        std::process::exit(0);
    });
}
