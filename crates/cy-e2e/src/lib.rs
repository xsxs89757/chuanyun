//! 端到端测试专用 crate（不发布、无生产代码）。
//!
//! 它同时依赖 `cy-server` 与 `cy-core`，好在一个进程里起真实服务端 + 真实客户端跑全链路；
//! 生产依赖图不受影响——两者本身仍然互不依赖，只通过协议对话。
//!
//! 所有监听器都绑 `:0` 让内核分配端口，再从句柄回读实际地址。端口冲突导致的
//! 偶发失败是测试里最没意思的一类噪音。

use std::time::Duration;

use cy_core::{CoreConfig, Verify};
use cy_server::{Config, Server, ServerHandle, Store};
use tempfile::TempDir;

/// 一套跑起来的服务端 + 它的数据目录。
pub struct TestServer {
    pub handle: ServerHandle,
    /// 拿着它别让临时目录被回收
    pub _data_dir: TempDir,
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    /// 起一个服务端，可以先改改配置。
    pub async fn start_with(tweak: impl FnOnce(&mut Config)) -> Self {
        let data_dir = tempfile::tempdir().expect("建临时目录");

        let mut config = Config {
            storage: cy_server::config::StorageConfig {
                data_dir: data_dir.path().to_path_buf(),
            },
            ..Default::default()
        };
        config.http.domain_suffix = "t.example.com".into();
        // 端口全交给内核分配
        config.control.listen = "127.0.0.1:0".parse().unwrap();
        config.http.listen = "127.0.0.1:0".parse().unwrap();
        config.admin.listen = "127.0.0.1:0".parse().unwrap();
        // 心跳调快，好让"掉线检测"类断言不必真等十几秒
        config.control.heartbeat_secs = 1;
        tweak(&mut config);

        let handle = Server::start(config).await.expect("启动服务端");
        Self {
            handle,
            _data_dir: data_dir,
        }
    }

    pub fn store(&self) -> &Store {
        &self.handle.store
    }

    /// 新建一个用户，拿到凭证。
    pub async fn add_user(&self, name: &str) -> String {
        self.handle
            .store
            .add_user(name, None, 10)
            .await
            .expect("新建用户")
    }

    /// 配一个连向本服务端的客户端（指纹已经填好）。
    pub fn client_config(&self, token: &str) -> CoreConfig {
        let mut config = CoreConfig::new(
            self.handle.control_addr.to_string(),
            token,
            Verify::Pin(self.handle.fingerprint.clone()),
        );
        config.backoff_base = Duration::from_millis(50);
        config.backoff_max = Duration::from_millis(200);
        config
    }
}

/// 起一个本地 TCP 回声服务，返回它的端口。
///
/// 数据面本身不理解 HTTP，所以最小可信的验证就是原样回声：字节进得去、出得来，
/// 说明流头解析、本地连接、双向拷贝这条链是通的。
pub async fn spawn_echo_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定回声服务");
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (mut r, mut w) = socket.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });

    port
}

/// 等一个条件成立，超时就失败。
///
/// 分布式的时序断言用 sleep 写会两头不讨好：写短了偶发失败，写长了测试变慢。
pub async fn wait_for<F>(what: &str, timeout: Duration, mut cond: F)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待「{what}」超时（{timeout:?}）");
}
