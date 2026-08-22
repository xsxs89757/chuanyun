//! macOS：点 Dock 图标把窗口叫回来。
//!
//! 关窗只是把窗口藏起来（隧道要继续跑），用户自然会去点 Dock 图标想把它叫回来。
//! macOS 发的是 `applicationShouldHandleReopen:hasVisibleWindows:`——而 winit 0.30
//! 不处理这个回调，Slint 也就没法暴露它。于是点 Dock 毫无反应，只能从托盘菜单
//! 把窗口找回来。
//!
//! 解法：winit 已经给 NSApplication 装了自己的 delegate 类，我们不能换掉它，
//! 但可以在运行时往那个类上**追加**这一个方法（`class_addMethod`）。Objective-C
//! 的方法分发是按选择子动态查的，追加之后 AppKit 就能调到。
//!
//! 这段代码依赖 winit 的内部行为（它装 delegate 这件事），升级 Slint/winit 时
//! 要回头看一眼 Dock 点击还灵不灵。

#![cfg(target_os = "macos")]

use std::sync::OnceLock;

use objc2::ffi::class_addMethod;
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{sel, MainThreadMarker};
use objc2_app_kit::NSApplication;
use slint::ComponentHandle;

use crate::AppWindow;

/// 窗口的弱引用，给 C ABI 的回调用——它拿不到闭包环境，只能走全局。
static WINDOW: OnceLock<slint::Weak<AppWindow>> = OnceLock::new();

/// 装上 Dock 点击的处理。要在 Slint 窗口创建之后、事件循环跑起来之前调用。
pub fn install(window: &AppWindow) {
    let _ = WINDOW.set(window.as_weak());

    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("不在主线程，跳过 Dock 点击处理");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(delegate) = app.delegate() else {
        // winit 还没装 delegate——说明调用时机不对，或者 winit 改了行为
        tracing::warn!("NSApplication 还没有 delegate，Dock 点击不会有反应");
        return;
    };

    let sel = sel!(applicationShouldHandleReopen:hasVisibleWindows:);
    let obj: &AnyObject = delegate.as_ref();
    let class = obj.class();
    if class.responds_to(sel) {
        // winit 哪天自己实现了，就别再往上叠
        tracing::debug!("delegate 已经处理 reopen，不再追加");
        return;
    }

    // 签名：BOOL (id self, SEL _cmd, NSApplication *sender, BOOL hasVisibleWindows)
    // 类型编码 "B@:@B"：返回 BOOL，self，_cmd，对象参数，BOOL 参数
    extern "C-unwind" fn reopen(
        _this: &AnyObject,
        _cmd: Sel,
        _sender: &AnyObject,
        _has_visible: Bool,
    ) -> Bool {
        tracing::debug!("Dock 图标被点击，把窗口叫回来");
        if let Some(weak) = WINDOW.get() {
            if let Some(w) = weak.upgrade() {
                let _ = w.show();
                w.window().set_minimized(false);
            }
        }
        // 返回 NO：我们自己处理了，别让 AppKit 再去做默认的「新建空窗口」
        Bool::NO
    }

    // Imp 是无类型的函数指针，按 Objective-C 惯例在调用时按实际签名解释。
    // 签名由上面的类型编码字符串 "B@:@B" 告诉运行时。
    let imp: objc2::runtime::Imp = unsafe {
        std::mem::transmute::<
            extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, Bool) -> Bool,
            objc2::runtime::Imp,
        >(reopen)
    };
    let added =
        unsafe { class_addMethod(class as *const _ as *mut _, sel, imp, c"B@:@B".as_ptr()) };
    if added.as_bool() {
        tracing::debug!("已接管 Dock 点击");
    } else {
        tracing::warn!("往 delegate 类上追加 reopen 方法失败，Dock 点击不会有反应");
    }
}
