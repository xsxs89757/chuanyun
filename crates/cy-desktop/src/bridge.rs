//! 界面与引擎之间的桥。
//!
//! 所有跨线程的往来都收在这一个文件里，别让 `slint::invoke_from_event_loop`
//! 和 `runtime.spawn` 散落到界面代码各处。

use std::rc::Rc;
use std::sync::Arc;

use cy_core::{Engine, Status};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{AppWindow, ConnectUi, RequestUi, TunnelUi};

/// 把界面的回调接到引擎上，并让引擎的状态变化反映到界面。
pub fn wire(
    window: &AppWindow,
    tray: &crate::TrayIcon,
    engine: Engine,
    runtime: Arc<tokio::runtime::Runtime>,
) {
    window.set_version(env!("CARGO_PKG_VERSION").into());
    window.set_default_server(cy_core::brand::embedded().default_server.into());

    let tunnels: Rc<VecModel<TunnelUi>> = Rc::new(VecModel::default());
    window.set_tunnels(ModelRc::from(tunnels.clone()));
    let requests: Rc<VecModel<RequestUi>> = Rc::new(VecModel::default());
    window.set_requests(ModelRc::from(requests.clone()));
    let connects: Rc<VecModel<ConnectUi>> = Rc::new(VecModel::default());
    window.set_connects(ModelRc::from(connects.clone()));

    wire_callbacks(window, &engine, &runtime);
    wire_tray(window, tray, &engine, &runtime);
    spawn_status_pump(window, tray, engine, runtime);
}

fn wire_callbacks(window: &AppWindow, engine: &Engine, runtime: &Arc<tokio::runtime::Runtime>) {
    // 登录：按钮先进入「正在连接」，结果回来再解除
    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        let weak = window.as_weak();
        window.on_login(move |server, token, pin| {
            let engine = engine.clone();
            let weak = weak.clone();
            if let Some(w) = weak.upgrade() {
                w.set_login_busy(true);
                w.set_login_error("".into());
            }
            runtime.spawn(async move {
                let result = engine
                    .login(server.to_string(), token.to_string(), pin.to_string())
                    .await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_login_busy(false);
                        if let Err(e) = result {
                            w.set_login_error(e.into());
                        }
                    }
                });
            });
        });
    }

    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        window.on_logout(move || {
            let engine = engine.clone();
            runtime.spawn(async move { engine.logout().await });
        });
    }

    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        let weak = window.as_weak();
        window.on_add_tunnel(move |name, port| {
            let engine = engine.clone();
            let weak = weak.clone();
            let name = name.to_string();
            // 端口从输入框来，可能是空的或超范围——在这里挡住，别把垃圾送进引擎
            let Ok(port) = u16::try_from(port.max(0)) else {
                if let Some(w) = weak.upgrade() {
                    w.set_login_error("端口要在 1 到 65535 之间".into());
                }
                return;
            };
            runtime.spawn(async move {
                if let Err(e) = engine.add_tunnel(&name, port).await {
                    tracing::warn!(%name, error = %e, "新建隧道失败");
                    // 失败原因会随状态刷新出现在那条隧道的卡片上
                }
            });
        });
    }

    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        window.on_toggle_tunnel(move |name, on| {
            let engine = engine.clone();
            let name = name.to_string();
            runtime.spawn(async move { engine.set_enabled(name, on).await });
        });
    }

    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        window.on_remove_tunnel(move |name| {
            let engine = engine.clone();
            let name = name.to_string();
            runtime.spawn(async move { engine.remove_tunnel(name).await });
        });
    }

    window.on_copy_url(move |url| {
        if let Err(e) = copy_to_clipboard(&url) {
            tracing::warn!(error = %e, "复制到剪贴板失败");
        }
    });

    // 二维码：手机扫一下就能打开，调试微信 H5 时省去在手机上敲长地址
    {
        let weak = window.as_weak();
        window.on_show_qr(move |url| {
            if let Some(w) = weak.upgrade() {
                match render_qr(&url) {
                    Ok(image) => {
                        w.set_qr_image(image);
                        w.set_qr_url(url);
                    }
                    Err(e) => tracing::warn!(error = %e, "生成二维码失败"),
                }
            }
        });
    }

    {
        let weak = window.as_weak();
        window.on_close_qr(move || {
            if let Some(w) = weak.upgrade() {
                w.set_qr_url("".into());
            }
        });
    }

    window.on_set_autostart(move |on| {
        if let Err(e) = set_autostart(on) {
            tracing::warn!(error = %e, "设置开机自启失败");
        }
    });

    // 观测：重放与清空
    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        window.on_replay(move |id| {
            let engine = engine.clone();
            runtime.spawn(async move {
                let Some(record) = engine.inspector().get(id as u64) else {
                    return;
                };
                let Some(port) = engine.status().tunnel(&record.tunnel).map(|t| t.local_port)
                else {
                    tracing::warn!(tunnel = %record.tunnel, "隧道已不在，无法确定重放到哪个端口");
                    return;
                };
                match cy_core::inspector::replay(&record, port).await {
                    Ok((status, _)) => tracing::info!(id, status, "已重放"),
                    Err(e) => tracing::warn!(id, error = %e, "重放失败"),
                }
            });
        });
    }

    {
        let engine = engine.clone();
        window.on_clear_requests(move || engine.inspector().clear(None));
    }

    // 接入
    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        window.on_add_connect(move |from, port| {
            let engine = engine.clone();
            let from = from.to_string();
            let Ok(port) = u16::try_from(port.max(0)) else {
                return;
            };
            runtime.spawn(async move {
                if let Err(e) = engine
                    .add_connect(cy_core::connect::ConnectSpec::new(port, from))
                    .await
                {
                    // 失败原因会随状态刷新显示在那条接入的卡片上
                    tracing::warn!(error = %e, "接入失败");
                }
            });
        });
    }

    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        window.on_remove_connect(move |port| {
            let engine = engine.clone();
            let Ok(port) = u16::try_from(port.max(0)) else {
                return;
            };
            runtime.spawn(async move { engine.remove_connect(port).await });
        });
    }

    window.set_autostart(autostart_enabled());
}

fn wire_tray(
    window: &AppWindow,
    tray: &crate::TrayIcon,
    engine: &Engine,
    runtime: &Arc<tokio::runtime::Runtime>,
) {
    {
        let weak = window.as_weak();
        tray.on_show_window(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.show();
                w.window().set_minimized(false);
            }
        });
    }

    {
        let engine = engine.clone();
        let runtime = runtime.clone();
        tray.on_stop_all(move || {
            let engine = engine.clone();
            runtime.spawn(async move {
                for t in engine.status().tunnels {
                    engine.set_enabled(t.name, false).await;
                }
            });
        });
    }

    tray.on_quit(move || {
        let _ = slint::quit_event_loop();
    });
}

/// 定时把引擎状态刷到界面上。
///
/// 用轮询而不是逐个事件更新：状态本来就是个整体快照，一秒重画四次的开销可以忽略，
/// 但换来的是界面永远不会因为漏了某个事件而停在错误的状态上。
fn spawn_status_pump(
    window: &AppWindow,
    tray: &crate::TrayIcon,
    engine: Engine,
    runtime: Arc<tokio::runtime::Runtime>,
) {
    let weak = window.as_weak();
    let tray_weak = tray.as_weak();

    runtime.spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
        let mut last: Option<Status> = None;
        let mut last_request_count = usize::MAX;

        loop {
            ticker.tick().await;
            let status = engine.status();
            let request_count = engine.inspector().len();
            // 没变就不打扰界面线程
            if last.as_ref() == Some(&status) && last_request_count == request_count {
                continue;
            }
            last = Some(status.clone());
            last_request_count = request_count;

            let records = engine.inspector().list(None);
            let weak = weak.clone();
            let tray_weak = tray_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    apply_status(&w, &status);
                    apply_requests(&w, &records);
                }
                if let Some(t) = tray_weak.upgrade() {
                    t.set_connected(status.connected);
                    t.set_tunnel_count(
                        status.tunnels.iter().filter(|t| t.url.is_some()).count() as i32
                    );
                }
            });
        }
    });
}

fn apply_status(window: &AppWindow, status: &Status) {
    window.set_logged_in(!status.needs_login);
    window.set_connected(status.connected);
    window.set_reconnect_attempt(status.reconnect_attempt as i32);
    window.set_domain_suffix(status.domain_suffix.clone().into());
    window.set_status_text(status_text(status).into());

    let rows: Vec<TunnelUi> = status
        .tunnels
        .iter()
        .map(|t| TunnelUi {
            name: t.name.clone().into(),
            local_port: t.local_port as i32,
            kind: "http".into(),
            protected: false,
            url: t.url.clone().unwrap_or_default().into(),
            enabled: t.enabled,
            error: t.error.clone().unwrap_or_default().into(),
        })
        .collect();

    // 整体替换而不是逐条 diff：几十条的量级，重建比对齐简单也不会闪
    let model = window.get_tunnels();
    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<TunnelUi>>() {
        vec_model.set_vec(rows);
    }

    let connects: Vec<ConnectUi> = status
        .connects
        .iter()
        .map(|c| ConnectUi {
            local_port: c.local_port as i32,
            from: c.from.clone().into(),
            upstream: c.upstream.clone().into(),
            running: c.running,
            error: c.error.clone().unwrap_or_default().into(),
        })
        .collect();
    if let Some(model) = window
        .get_connects()
        .as_any()
        .downcast_ref::<VecModel<ConnectUi>>()
    {
        model.set_vec(connects);
    }
}

fn apply_requests(window: &AppWindow, records: &[cy_core::inspector::Record]) {
    let rows: Vec<RequestUi> = records
        .iter()
        .map(|r| RequestUi {
            id: r.id as i32,
            method: r.method.clone().into(),
            path: r.path.clone().into(),
            status: r.status.unwrap_or(0) as i32,
            duration_ms: r.duration.map(|d| d.as_millis() as i32).unwrap_or(0),
            tunnel: r.tunnel.clone().into(),
        })
        .collect();
    if let Some(model) = window
        .get_requests()
        .as_any()
        .downcast_ref::<VecModel<RequestUi>>()
    {
        model.set_vec(rows);
    }
}

/// 状态栏那一行字。用户看的是这句话，不是状态枚举。
fn status_text(status: &Status) -> String {
    if status.connected {
        return "已连接".into();
    }
    if status.needs_login {
        return "请先登录".into();
    }
    if status.reconnect_attempt > 0 {
        return match &status.last_error {
            Some(e) => format!("正在重连（第 {} 次）· {e}", status.reconnect_attempt),
            None => format!("正在重连（第 {} 次）", status.reconnect_attempt),
        };
    }
    match &status.last_error {
        Some(e) => format!("未连接 · {e}"),
        None => "未连接".into(),
    }
}

fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text.to_string())?;
    Ok(())
}

/// 把地址渲染成二维码图片。
fn render_qr(url: &str) -> anyhow::Result<slint::Image> {
    use qrcode::QrCode;

    let code = QrCode::new(url.as_bytes())?;
    let modules = code.to_colors();
    let size = (modules.len() as f64).sqrt() as u32;

    // 每个模块画成 1 像素，界面上再放大（image-rendering: pixelated），
    // 这样不管窗口多大都不会糊
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(size, size);
    let pixels = buffer.make_mut_slice();
    for (i, color) in modules.iter().enumerate() {
        let dark = *color == qrcode::Color::Dark;
        let v = if dark { 0 } else { 255 };
        pixels[i] = slint::Rgb8Pixel { r: v, g: v, b: v };
    }
    Ok(slint::Image::from_rgb8(buffer))
}

fn autolaunch() -> anyhow::Result<auto_launch::AutoLaunch> {
    let exe = std::env::current_exe()?;
    let path = exe.to_string_lossy().to_string();
    Ok(auto_launch::AutoLaunchBuilder::new()
        .set_app_name("穿云")
        .set_app_path(&path)
        .build()?)
}

fn set_autostart(on: bool) -> anyhow::Result<()> {
    let al = autolaunch()?;
    if on {
        al.enable()?;
    } else {
        al.disable()?;
    }
    Ok(())
}

fn autostart_enabled() -> bool {
    autolaunch()
        .and_then(|a| Ok(a.is_enabled()?))
        .unwrap_or(false)
}
