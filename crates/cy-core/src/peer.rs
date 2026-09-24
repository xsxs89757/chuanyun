//! 和本机上另一个正在跑的穿云说话。
//!
//! 关窗只是收进托盘，进程和隧道都还在跑。这时再双击快捷方式，Windows 会老老实实
//! 再起一个进程：两个进程用同一张凭证登录、开同名隧道，后起的那个只会看到
//! 「隧道名已被占用」——托盘图标又常常被折叠在任务栏的 `^` 里，用户根本不知道还有
//! 一个在跑。
//!
//! 所以新起来的那个先经本地 API 问一声：版本不比它新，就请它把窗口叫出来，自己
//! 退出；比它新（刚装了新版），就请它让位，自己接班。

use std::time::Duration;

use serde::Deserialize;

/// 另一个实例报上来的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub version: String,
    /// 进程号。0.1.14 起才报；更早的版本没有这个字段
    pub pid: Option<u32>,
    pub connected: bool,
}

impl Peer {
    /// 0.1.14 之前的版本：没有 `/api/show`、`/api/quit`，只能用 `/api/shutdown`
    /// 让它断开连接，进程得另想办法结束。
    pub fn is_legacy(&self) -> bool {
        self.pid.is_none()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("连不上本地 API: {0}")]
    Http(#[from] reqwest::Error),
    #[error("对方不认识这个请求（HTTP {0}）")]
    Refused(u16),
}

#[derive(Deserialize)]
struct StatusBody {
    version: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    connected: bool,
}

fn client() -> Result<reqwest::Client, PeerError> {
    Ok(reqwest::Client::builder()
        // 本机地址，别让系统代理或 HTTP_PROXY 截走
        .no_proxy()
        // Windows 上连一个没人监听的本机端口要重试好几次 SYN，默认要一两秒才报错
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(5))
        .build()?)
}

fn url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

/// 看看这个端口上是不是一个穿云。不是（或者没人）就返回 `None`。
pub async fn probe(port: u16) -> Option<Peer> {
    let resp = client()
        .ok()?
        .get(url(port, "/api/status"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    // 端口上可能是别的程序：解析不出我们的字段就当不是
    let body: StatusBody = resp.json().await.ok()?;
    Some(Peer {
        version: body.version,
        pid: body.pid,
        connected: body.connected,
    })
}

async fn post(port: u16, path: &str) -> Result<(), PeerError> {
    let resp = client()?.post(url(port, path)).send().await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(PeerError::Refused(resp.status().as_u16()))
    }
}

/// 请它把窗口叫到前面。
pub async fn show(port: u16) -> Result<(), PeerError> {
    post(port, "/api/show").await
}

/// 请它整个退出（0.1.14 起）。只是发起，退没退完要另外等。
pub async fn quit(port: u16) -> Result<(), PeerError> {
    post(port, "/api/quit").await
}

/// 请它断开连接、放掉隧道名。老版本也有这个接口，但它只停引擎、不退进程。
pub async fn shutdown(port: u16) -> Result<(), PeerError> {
    post(port, "/api/shutdown").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Brand, Engine};
    use crate::local_api::{serve_on, AppHooks};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 起一个真的本地 API（端口交给内核分配），返回端口和两个钩子被调了几次。
    async fn running_instance() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let shown = Arc::new(AtomicUsize::new(0));
        let quits = Arc::new(AtomicUsize::new(0));
        let hooks = AppHooks {
            show: Some({
                let n = shown.clone();
                Arc::new(move || {
                    n.fetch_add(1, Ordering::SeqCst);
                })
            }),
            quit: Some({
                let n = quits.clone();
                Arc::new(move || {
                    n.fetch_add(1, Ordering::SeqCst);
                })
            }),
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let engine = Engine::start(None, Brand::default());
        tokio::spawn(serve_on(listener, engine, hooks));
        (port, shown, quits)
    }

    #[tokio::test]
    async fn probe_reads_version_and_pid() {
        let (port, _, _) = running_instance().await;
        let peer = probe(port).await.expect("应认出是穿云");
        assert_eq!(peer.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(peer.pid, Some(std::process::id()));
        assert!(!peer.is_legacy());
    }

    #[tokio::test]
    async fn nobody_listening_is_not_a_peer() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert_eq!(probe(port).await, None);
    }

    #[tokio::test]
    async fn some_other_program_on_the_port_is_not_a_peer() {
        // 端口被别的 HTTP 服务占着：能连上、也回 200，但不是我们
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().route(
            "/api/status",
            axum::routing::get(|| async { "hello from someone else" }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await });
        assert_eq!(probe(port).await, None);
    }

    #[tokio::test]
    async fn show_and_quit_reach_the_running_instance() {
        let (port, shown, quits) = running_instance().await;
        show(port).await.expect("show");
        assert_eq!(shown.load(Ordering::SeqCst), 1);
        quit(port).await.expect("quit");
        assert_eq!(quits.load(Ordering::SeqCst), 1);
    }

    /// 0.1.13 及更早的版本：status 里没有 pid，也没有 show / quit。
    #[tokio::test]
    async fn legacy_instances_are_recognised() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().route(
            "/api/status",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "connected": true,
                    "domain_suffix": "t.example.com",
                    "needs_login": false,
                    "reconnect_attempt": 0,
                    "version": "0.1.11"
                }))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await });

        let peer = probe(port).await.expect("老版本也要认得出");
        assert_eq!(peer.version, "0.1.11");
        assert!(peer.is_legacy());
        assert!(matches!(show(port).await, Err(PeerError::Refused(404))));
    }
}
