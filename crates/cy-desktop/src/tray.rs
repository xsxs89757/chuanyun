//! 系统托盘。
//!
//! Slint 1.17 起内置了 `SystemTrayIcon`，不用再自己接第三方托盘库和事件循环。
//!
//! 两个平台差异值得记住：macOS 上一旦给图标挂了菜单，「点击图标」的回调就不再
//! 触发；Windows 会忽略 `title`。所以交互一律走菜单项，不依赖点击——反正我们
//! 要的三个动作本来就适合放菜单里。

use slint::ComponentHandle;

use crate::{AppWindow, TrayIcon};

/// 建好托盘图标并让它显示出来。
pub fn setup(window: &AppWindow) -> anyhow::Result<TrayIcon> {
    let tray = TrayIcon::new()?;
    tray.show()?;

    // 点关闭按钮 = 收进托盘，不是退出。
    //
    // 隧道是长期开着的东西，误点一下关闭就把同事的联调地址弄没了太糟糕。
    // 真要退出走托盘菜单里的「退出」。
    let weak = window.as_weak();
    window.window().on_close_requested(move || {
        if let Some(w) = weak.upgrade() {
            let _ = w.hide();
        }
        slint::CloseRequestResponse::KeepWindowShown
    });

    Ok(tray)
}
