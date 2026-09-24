//! 单实例：同一份配置只许一个穿云在跑。
//!
//! 关窗只是收进托盘，进程和隧道都还在跑。以前这时再双击快捷方式，Windows 会再起一个
//! 进程：同一张凭证登录成第二个会话、去开同名隧道，服务端当然拒绝——新窗口里的隧道
//! 全是「已被占用」，而托盘图标又常被折叠在任务栏的 `^` 里，同事根本不知道还有一个
//! 在跑，杀掉的往往还是前台那个。macOS 双击已在运行的 app 只会把它唤回来，所以在
//! Mac 上测不出来。
//!
//! 现在启动时先做两件事：
//!
//! 1. 锁住配置目录里的 `instance.lock`。锁不上，说明已经有一个（0.1.14 起的）在跑：
//!    版本不比它新，就请它把窗口叫出来、自己退出；比它新（刚装了新版），就请它让位。
//! 2. 占住本地 API 的端口。占不上、而上面是个老版本穿云（0.1.13 及以前，它们不认
//!    锁文件）：先请它断开、放掉隧道名，再结束它的进程，然后接班。
//!
//! 锁由操作系统管：进程不管怎么结束（包括崩溃、被任务管理器结束），锁都会自动放开。

use std::fs::{File, OpenOptions, TryLockError};
use std::net::TcpListener;
use std::path::Path;
use std::time::{Duration, Instant};

use cy_core::peer::{self, Peer};

const OUR_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 启动时的判定结果。
pub enum Startup {
    /// 就这一个，接着启动。
    Primary {
        /// 锁要一直拿着，进程结束时由系统放开
        lock: Option<File>,
        /// 已经占住的本地 API 端口；占不上就是 `None`——脚本接入用不了，界面照常用
        listener: Option<TcpListener>,
    },
    /// 已经有一个在跑，也已经请它把窗口叫出来了。这个进程该退出了。
    AlreadyRunning,
}

/// 拿到「唯一在跑的那个」的身份，拿不到就说明该让位。
pub fn claim(rt: &tokio::runtime::Runtime, lock_path: Option<&Path>, port: u16) -> Startup {
    let lock = match lock_path.map(try_lock) {
        Some(Ok(Some(file))) => Some(file),
        Some(Ok(None)) => match resolve_running(rt, lock_path.expect("上面刚判过"), port) {
            Some(file) => Some(file),
            None => return Startup::AlreadyRunning,
        },
        Some(Err(e)) => {
            // 锁文件打不开（权限、奇怪的文件系统）：没有锁也能用，只是少一道保险
            tracing::warn!(error = %e, "单实例锁文件打不开，跳过这一步");
            None
        }
        None => None,
    };
    let listener = bind_local_api(rt, port);
    Startup::Primary { lock, listener }
}

/// 锁住就返回文件，别人拿着就返回 `None`。
fn try_lock(path: &Path) -> std::io::Result<Option<File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// 锁在别人手里：看看它是谁，决定请它出来还是请它让位。
///
/// 返回 `Some` 表示已经接班、锁到手了；`None` 表示它留下、我们退出。
fn resolve_running(rt: &tokio::runtime::Runtime, lock_path: &Path, port: u16) -> Option<File> {
    // 它可能刚拿到锁、本地 API 还没起来，多等一会儿
    let Some(running) = wait_for_peer(rt, port, Duration::from_secs(3)) else {
        tracing::warn!(port, "已经有一个穿云拿着锁，但本地 API 叫不应");
        alert("穿云已经在运行，但没有响应。\n\n请在任务管理器里结束 chuanyun.exe 之后再打开。");
        return None;
    };

    match plan(&running) {
        Plan::AskItToShow => {
            tracing::info!(version = %running.version, "穿云已经在运行，把它的窗口叫出来");
            allow_foreground(running.pid);
            if let Err(e) = rt.block_on(peer::show(port)) {
                tracing::warn!(error = %e, "没能叫出已在运行的窗口");
            }
            None
        }
        Plan::TakeOver => {
            tracing::info!(version = %running.version, "旧版本还在运行，请它让位");
            if let Err(e) = rt.block_on(peer::quit(port)) {
                tracing::warn!(error = %e, "请旧版本退出没成功，稍后直接结束它");
            }
            // 它要先断开连接（最多两秒），看门狗五秒后兜底
            if let Some(file) = wait_for_lock(lock_path, Duration::from_secs(8)) {
                return Some(file);
            }
            if let Some(pid) = running.pid {
                tracing::warn!(pid, "旧版本迟迟不退，直接结束它");
                kill_pid(pid);
            }
            let file = wait_for_lock(lock_path, Duration::from_secs(3));
            if file.is_none() {
                alert("旧版本的穿云没能退出。\n\n请在任务管理器里结束 chuanyun.exe 之后再打开。");
            }
            file
        }
    }
}

/// 碰到一个已在运行的穿云，该怎么办。
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// 它不比我们旧：请它把窗口叫出来，我们退出
    AskItToShow,
    /// 我们是新装的版本：请它让位
    TakeOver,
}

fn plan(running: &Peer) -> Plan {
    if cy_core::update::is_newer(OUR_VERSION, &running.version) {
        Plan::TakeOver
    } else {
        Plan::AskItToShow
    }
}

/// 占住本地 API 的端口。被老版本穿云占着就请它让位。
fn bind_local_api(rt: &tokio::runtime::Runtime, port: u16) -> Option<TcpListener> {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => return Some(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
        Err(e) => {
            tracing::warn!(port, error = %e, "本地 API 端口绑不上；脚本接入功能不可用");
            return None;
        }
    }

    match rt.block_on(peer::probe(port)) {
        Some(running) if running.is_legacy() => {
            take_over_legacy(rt, port, &running);
            let listener = wait_for_bind(port, Duration::from_secs(3));
            if listener.is_none() {
                tracing::warn!(port, "老版本让位后端口还是没空出来；脚本接入功能不可用");
            }
            listener
        }
        Some(running) => {
            // 0.1.14 起的实例都认锁文件，锁在我们手里还碰上它，说明它用的是另一份配置
            // （CHUANYUN_CONFIG_DIR），那是别人有意为之，别去动它
            tracing::warn!(
                port,
                version = %running.version,
                "本地 API 端口被另一份配置的穿云占着；脚本接入功能不可用"
            );
            None
        }
        None => {
            tracing::warn!(port, "本地 API 端口被别的程序占着；脚本接入功能不可用");
            None
        }
    }
}

/// 请一个老版本（0.1.13 及以前）让位。
///
/// 它们不认锁文件，也没有 `/api/quit`。`/api/shutdown` 能让它断开连接——控制流一关，
/// 服务端当场放掉它占着的隧道名——但它只停引擎、不退进程，托盘图标和本地 API 端口
/// 都还攥着，所以之后还得替它把进程结束掉。
fn take_over_legacy(rt: &tokio::runtime::Runtime, port: u16, running: &Peer) {
    tracing::info!(version = %running.version, "发现一个老版本穿云还在运行，请它断开后接班");
    if let Err(e) = rt.block_on(peer::shutdown(port)) {
        tracing::warn!(error = %e, "请老版本断开没成功");
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match rt.block_on(peer::probe(port)) {
            Some(p) if p.connected => std::thread::sleep(Duration::from_millis(100)),
            _ => break,
        }
    }
    kill_other_instances();
}

fn wait_for_peer(rt: &tokio::runtime::Runtime, port: u16, max: Duration) -> Option<Peer> {
    let deadline = Instant::now() + max;
    loop {
        if let Some(p) = rt.block_on(peer::probe(port)) {
            return Some(p);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_lock(path: &Path, max: Duration) -> Option<File> {
    let deadline = Instant::now() + max;
    loop {
        if let Ok(Some(file)) = try_lock(path) {
            return Some(file);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_bind(port: u16, max: Duration) -> Option<TcpListener> {
    let deadline = Instant::now() + max;
    loop {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            return Some(listener);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 结束除自己以外所有同名的穿云进程。老版本不报进程号，只能按程序名找。
///
/// 只在 Windows 上做：macOS 双击已在运行的 app 不会再起一个进程，老版本在那边
/// 不会成双。
#[cfg(windows)]
fn kill_other_instances() {
    let image = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "chuanyun.exe".into());
    let own = format!("PID ne {}", std::process::id());
    run_taskkill(&["/F", "/IM", &image, "/FI", &own]);
}

#[cfg(not(windows))]
fn kill_other_instances() {
    tracing::info!("老版本已断开；这个平台上不替它结束进程");
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    run_taskkill(&["/F", "/PID", &pid.to_string()]);
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

#[cfg(windows)]
fn run_taskkill(args: &[&str]) {
    use std::os::windows::process::CommandExt;
    // 不带窗口：否则会闪一下黑框
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // 写全路径，别被 PATH 里同名的东西截胡
    let exe = std::env::var_os("SystemRoot")
        .map(|root| {
            std::path::PathBuf::from(root)
                .join("System32")
                .join("taskkill.exe")
        })
        .unwrap_or_else(|| "taskkill.exe".into());
    match std::process::Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
    {
        Ok(status) => tracing::info!(?args, %status, "taskkill"),
        Err(e) => tracing::warn!(error = %e, "taskkill 没能运行"),
    }
}

/// Windows 只允许前台进程把窗口提到最前。用户刚双击启动的是我们，把这个权利让给
/// 已经在跑的那个，它才能把窗口顶上来，而不是只在任务栏上闪一下。
#[cfg(windows)]
fn allow_foreground(pid: Option<u32>) {
    const ASFW_ANY: u32 = u32::MAX;
    unsafe {
        win::AllowSetForegroundWindow(pid.unwrap_or(ASFW_ANY));
    }
}

#[cfg(not(windows))]
fn allow_foreground(_pid: Option<u32>) {}

/// 弹一个提示框。这时主界面还没起来，只能用系统自带的。
#[cfg(windows)]
fn alert(text: &str) {
    const MB_ICONINFORMATION: u32 = 0x40;
    let text = win::wide(text);
    let caption = win::wide("穿云");
    unsafe {
        win::MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONINFORMATION,
        );
    }
}

#[cfg(not(windows))]
fn alert(text: &str) {
    eprintln!("{text}");
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        pub fn MessageBoxW(
            hwnd: *mut c_void,
            text: *const u16,
            caption: *const u16,
            utype: u32,
        ) -> i32;
        pub fn AllowSetForegroundWindow(pid: u32) -> i32;
    }

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(version: &str) -> Peer {
        Peer {
            version: version.into(),
            pid: Some(1),
            connected: true,
        }
    }

    #[test]
    fn a_second_launch_brings_up_the_running_window() {
        assert_eq!(plan(&peer(OUR_VERSION)), Plan::AskItToShow);
    }

    #[test]
    fn a_newly_installed_version_takes_over() {
        assert_eq!(plan(&peer("0.1.11")), Plan::TakeOver);
    }

    #[test]
    fn an_older_build_does_not_push_out_a_newer_one() {
        // 桌面上还留着旧版的快捷方式，点了它不该把新版挤掉
        assert_eq!(plan(&peer("99.0.0")), Plan::AskItToShow);
    }

    #[test]
    fn only_one_process_gets_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("instance.lock");

        let first = try_lock(&path).unwrap().expect("第一个应该拿到锁");
        assert!(try_lock(&path).unwrap().is_none(), "第二个不该再拿到");

        // 进程结束（这里用关闭文件模拟）后锁就放开了
        drop(first);
        assert!(try_lock(&path).unwrap().is_some(), "放开之后应能再拿到");
    }
}
