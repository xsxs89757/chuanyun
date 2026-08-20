//! 穿云服务端。
//!
//! `main.rs` 只是薄壳：真正的启动入口是 [`Server::start`]，它返回携带**实际绑定地址**的
//! 句柄。这样集成测试可以让所有监听器都用 `:0` 端口，起真实服务端跑全链路，
//! 不必预分配端口或猜配置——端口冲突导致的偶发失败是测试里最没意思的一类噪音。

pub mod config;
pub mod control;
pub mod registry;
pub mod store;
pub mod tls;

pub use config::Config;
pub use cy_proto::PROTO_VERSION;
pub use store::Store;

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use registry::Registry;

/// 服务端版本，握手时告诉客户端。
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
    #[error("监听 {addr} 失败: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// 运行中的服务端。
pub struct ServerHandle {
    /// 控制通道的实际监听地址（配 `:0` 时这里是内核分配的真实端口）
    pub control_addr: SocketAddr,
    /// 控制通道证书指纹，客户端 pin 用
    pub fingerprint: String,
    pub store: Store,
    pub registry: Arc<Registry>,
    shutdown: CancellationToken,
    tasks: JoinSet<()>,
}

impl ServerHandle {
    /// 通知所有任务停止并等它们收尾。
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        while self.tasks.join_next().await.is_some() {}
    }

    /// 等待服务端结束（正常情况下不会返回，除非被 shutdown）。
    pub async fn wait(mut self) {
        while self.tasks.join_next().await.is_some() {}
    }
}

pub struct Server;

impl Server {
    /// 按配置启动服务端。
    pub async fn start(config: Config) -> Result<ServerHandle, ServerError> {
        config.validate()?;

        std::fs::create_dir_all(&config.storage.data_dir).map_err(tls::TlsError::Io)?;
        let store = Store::open(&config.storage.data_dir.join("chuanyun.db"))?;
        let identity = tls::Identity::load_or_create(&config.storage.data_dir)?;
        let fingerprint = identity.fingerprint.clone();
        let tls_config = identity.server_config()?;

        let registry = Arc::new(Registry::new());
        let shutdown = CancellationToken::new();
        let config = Arc::new(config);

        let listener = tokio::net::TcpListener::bind(config.control.listen)
            .await
            .map_err(|source| ServerError::Bind {
                addr: config.control.listen,
                source,
            })?;
        let control_addr = listener.local_addr().map_err(|source| ServerError::Bind {
            addr: config.control.listen,
            source,
        })?;

        let mut tasks = JoinSet::new();
        tasks.spawn(control::serve(
            listener,
            tls_config,
            config.clone(),
            store.clone(),
            registry.clone(),
            shutdown.clone(),
        ));

        tracing::info!(
            %control_addr,
            fingerprint = %fingerprint,
            "控制通道已就绪"
        );

        Ok(ServerHandle {
            control_addr,
            fingerprint,
            store,
            registry,
            shutdown,
            tasks,
        })
    }
}
