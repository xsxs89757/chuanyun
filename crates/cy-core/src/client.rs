//! 客户端连接：TLS 握手、控制循环、数据面转发。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use cy_proto::codec::JsonLines;
use cy_proto::mux::{MuxHandle, MuxStream};
use cy_proto::{ClientMsg, Endpoint, ServerMsg, StreamHeader, TunnelKind};
use futures::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::verifier::{PinnedCertVerifier, TofuCertVerifier};

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 怎么校验服务端证书。
#[derive(Debug, Clone)]
pub enum Verify {
    /// 只认这个指纹（内部版把它编译进安装包，用户无感）
    Pin(String),
    /// 首次连接时把看到的指纹交给用户确认
    Tofu,
    /// 走系统 CA——服务端给控制端口配了正规证书时用
    System,
}

#[derive(Debug, Clone)]
pub struct CoreConfig {
    /// 服务端控制通道地址，`host:port`
    pub server: String,
    pub token: String,
    pub verify: Verify,
    /// 退避起点与上限。测试里注入很短的值，好让重连断言不必真等几秒。
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl CoreConfig {
    pub fn new(server: impl Into<String>, token: impl Into<String>, verify: Verify) -> Self {
        Self {
            server: server.into(),
            token: token.into(),
            verify,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        }
    }
}

/// 一条隧道的期望状态（用户配的，与实际是否已开通无关）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpec {
    pub name: String,
    pub local_port: u16,
    pub kind: TunnelKind,
}

impl TunnelSpec {
    pub fn http(name: impl Into<String>, local_port: u16) -> Self {
        Self {
            name: name.into(),
            local_port,
            kind: TunnelKind::Http,
        }
    }
}

/// 客户端发生的事，供界面订阅。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Connected {
        user_visible_suffix: String,
    },
    Disconnected {
        reason: String,
    },
    TunnelOpened {
        name: String,
        url: String,
    },
    TunnelFailed {
        name: String,
        reason: String,
    },
    /// 被管理员踢下线——这种情况不该自动重连
    Kicked {
        reason: String,
    },
    /// 握手被拒（凭证无效/过期/吊销），重连也没用
    AuthRejected {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("连接服务端失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS 握手失败: {0}")]
    Tls(String),
    #[error("协议错误: {0}")]
    Protocol(String),
    #[error("{0}")]
    Rejected(String),
}

/// 已建立的连接。
#[derive(Debug)]
pub struct Connection {
    mux: MuxHandle,
    commands: mpsc::Sender<Command>,
    pub domain_suffix: String,
    pub session: String,
    cancel: CancellationToken,
}

enum Command {
    Open {
        spec: TunnelSpec,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Close {
        name: String,
    },
}

impl Connection {
    /// 开一条隧道，返回公网地址。
    pub async fn open_tunnel(&self, spec: TunnelSpec) -> Result<String, String> {
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(Command::Open { spec, reply })
            .await
            .map_err(|_| "连接已断开".to_string())?;
        rx.await.map_err(|_| "连接已断开".to_string())?
    }

    pub async fn close_tunnel(&self, name: impl Into<String>) {
        let _ = self
            .commands
            .send(Command::Close { name: name.into() })
            .await;
    }

    pub fn disconnect(&self) {
        self.cancel.cancel();
    }

    pub fn is_alive(&self) -> bool {
        !self.cancel.is_cancelled() && !self.mux.is_closed()
    }
}

/// 隧道 ID → 本地端口。数据面靠它知道该把流转发到哪。
type PortMap = Arc<RwLock<HashMap<String, u16>>>;

/// 已发出但还没收到回应的开隧道请求：隧道 ID → (名称, 本地端口, 回调)。
type PendingOpens = HashMap<String, (String, u16, oneshot::Sender<Result<String, String>>)>;

/// 建立一条连接：TCP → TLS → yamux → 控制流握手。
///
/// 成功返回后，控制循环和数据面都已在后台跑起来了。
pub async fn connect(
    config: &CoreConfig,
    events: broadcast::Sender<Event>,
) -> Result<Connection, ConnectError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| ConnectError::Tls(e.to_string()))?;

    let tls_config = match &config.verify {
        Verify::Pin(fp) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier::new(fp, provider)))
            .with_no_client_auth(),
        Verify::Tofu => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TofuCertVerifier::new(provider)))
            .with_no_client_auth(),
        Verify::System => {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };

    let socket = tokio::net::TcpStream::connect(&config.server).await?;
    // 控制消息都很小，攒着发只会让握手和心跳变慢
    let _ = socket.set_nodelay(true);

    // 证书按指纹校验，域名部分不参与判断；填一个固定值即可
    let server_name = rustls::pki_types::ServerName::try_from("chuanyun-control")
        .map_err(|e| ConnectError::Tls(e.to_string()))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_config))
        .connect(server_name, socket)
        .await
        .map_err(|e| ConnectError::Tls(e.to_string()))?;

    let (mux, inbound) = cy_proto::mux::spawn(
        tokio_util::compat::TokioAsyncReadCompatExt::compat(tls),
        yamux::Mode::Client,
    );

    // 控制流：由我们主动开，服务端把第一条流当控制流
    let control = mux
        .open()
        .await
        .map_err(|e| ConnectError::Protocol(e.to_string()))?;
    let mut framed = Framed::new(control, JsonLines::<ServerMsg, ClientMsg>::new());

    framed
        .send(ClientMsg::Hello {
            proto: cy_proto::PROTO_VERSION,
            client: CLIENT_VERSION.into(),
            os: std::env::consts::OS.into(),
            token: config.token.clone(),
        })
        .await
        .map_err(|e| ConnectError::Protocol(e.to_string()))?;

    let welcome = tokio::time::timeout(Duration::from_secs(15), framed.next())
        .await
        .map_err(|_| ConnectError::Protocol("等待服务端响应超时".into()))?
        .ok_or_else(|| ConnectError::Protocol("服务端没有响应就关闭了连接".into()))?
        .map_err(|e| ConnectError::Protocol(e.to_string()))?;

    let (session, domain_suffix) = match welcome {
        ServerMsg::Welcome {
            session,
            domain_suffix,
            ..
        } => (session, domain_suffix),
        ServerMsg::Error { code, message, .. } => {
            let text = if message.is_empty() {
                cy_proto::error::human(&code).to_string()
            } else {
                message
            };
            return Err(ConnectError::Rejected(text));
        }
        other => {
            return Err(ConnectError::Protocol(format!(
                "期待 welcome，收到 {other:?}"
            )))
        }
    };

    let cancel = CancellationToken::new();
    let ports: PortMap = Arc::new(RwLock::new(HashMap::new()));
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(32);

    tokio::spawn(data_plane(inbound, ports.clone(), cancel.clone()));
    tokio::spawn(control_loop(
        framed,
        cmd_rx,
        ports,
        events.clone(),
        cancel.clone(),
    ));

    let _ = events.send(Event::Connected {
        user_visible_suffix: domain_suffix.clone(),
    });

    Ok(Connection {
        mux,
        commands: cmd_tx,
        domain_suffix,
        session,
        cancel,
    })
}

/// 控制循环：把用户命令翻译成协议消息，把服务端的回应翻译成事件。
async fn control_loop(
    framed: Framed<MuxStream, JsonLines<ServerMsg, ClientMsg>>,
    mut commands: mpsc::Receiver<Command>,
    ports: PortMap,
    events: broadcast::Sender<Event>,
    cancel: CancellationToken,
) {
    let (mut sink, mut stream) = framed.split();
    let next_id = AtomicU64::new(1);

    let mut pending: PendingOpens = HashMap::new();
    // 隧道名 → id，关隧道时要用
    let mut by_name: HashMap<String, String> = HashMap::new();

    let reason = loop {
        tokio::select! {
            _ = cancel.cancelled() => break "已断开连接".to_string(),

            cmd = commands.recv() => {
                let Some(cmd) = cmd else { break "客户端已退出".to_string() };
                match cmd {
                    Command::Open { spec, reply } => {
                        let id = format!("t{}", next_id.fetch_add(1, Ordering::Relaxed));
                        let msg = ClientMsg::OpenTunnel {
                            id: id.clone(),
                            kind: spec.kind,
                            name: spec.name.clone(),
                            custom_domain: None,
                            auth: None,
                            remote_port: None,
                        };
                        if sink.send(msg).await.is_err() {
                            let _ = reply.send(Err("连接已断开".into()));
                            break "连接已断开".to_string();
                        }
                        pending.insert(id, (spec.name, spec.local_port, reply));
                    }
                    Command::Close { name } => {
                        if let Some(id) = by_name.remove(&name) {
                            ports.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
                            let _ = sink.send(ClientMsg::CloseTunnel { id }).await;
                        }
                    }
                }
            }

            incoming = stream.next() => {
                let Some(msg) = incoming else { break "服务端关闭了连接".to_string() };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => break format!("控制流出错: {e}"),
                };
                match msg {
                    ServerMsg::Ping { seq } => {
                        if sink.send(ClientMsg::Pong { seq }).await.is_err() {
                            break "连接已断开".to_string();
                        }
                    }
                    ServerMsg::Pong { .. } => {}
                    ServerMsg::TunnelOpened { id, public } => {
                        let url = match public {
                            Endpoint::Url(u) => u,
                            Endpoint::Addr(a) => a,
                        };
                        if let Some((name, port, reply)) = pending.remove(&id) {
                            // 先登记端口再回复：回复一发出，调用方就可能立刻收到请求了
                            ports.write().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), port);
                            by_name.insert(name.clone(), id);
                            let _ = events.send(Event::TunnelOpened { name, url: url.clone() });
                            let _ = reply.send(Ok(url));
                        }
                    }
                    ServerMsg::Error { code, message, id } => {
                        let text = if message.is_empty() {
                            cy_proto::error::human(&code).to_string()
                        } else {
                            message
                        };
                        match id.and_then(|i| pending.remove(&i)) {
                            Some((name, _, reply)) => {
                                let _ = events.send(Event::TunnelFailed {
                                    name,
                                    reason: text.clone(),
                                });
                                let _ = reply.send(Err(text));
                            }
                            None => {
                                // 连接级错误：多半是凭证出了问题，重连也没用
                                let _ = events.send(Event::AuthRejected { reason: text.clone() });
                                break text;
                            }
                        }
                    }
                    ServerMsg::Kick { reason } => {
                        let _ = events.send(Event::Kicked { reason: reason.clone() });
                        break format!("被管理员断开: {reason}");
                    }
                    ServerMsg::Welcome { .. } => {} // 重复的 welcome，忽略
                    ServerMsg::Unknown => {}
                }
            }
        }
    };

    cancel.cancel();
    // 还在等回应的请求得有个交代，别让调用方一直挂着
    for (_, (_, _, reply)) in pending {
        let _ = reply.send(Err(reason.clone()));
    }
    let _ = events.send(Event::Disconnected { reason });
}

/// 数据面：服务端每来一条流，就连一次本地端口，然后纯字节对拷。
async fn data_plane(
    mut inbound: mpsc::Receiver<MuxStream>,
    ports: PortMap,
    cancel: CancellationToken,
) {
    loop {
        let stream = tokio::select! {
            _ = cancel.cancelled() => break,
            s = inbound.recv() => match s {
                Some(s) => s,
                None => break,
            },
        };

        let ports = ports.clone();
        tokio::spawn(async move {
            if let Err(e) = forward(stream, ports).await {
                tracing::debug!(error = %e, "数据流转发结束");
            }
        });
    }
}

async fn forward(mut stream: MuxStream, ports: PortMap) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // 流头是一行 JSON，读完它剩下的全是裸字节
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        anyhow::bail!("流在发出流头前就关了");
    }
    let header = StreamHeader::from_line(line.trim_end())?;

    let port = ports
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&header.tunnel_id)
        .copied();
    let Some(port) = port else {
        anyhow::bail!("收到未知隧道 {} 的流", header.tunnel_id);
    };

    let mut local = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(e) => {
            // 最常见的情况：本地服务还没启动。让这条错误说人话，
            // 用户看到的会是"本地服务未启动"而不是一串 ECONNREFUSED。
            tracing::warn!(port, error = %e, "连接本地服务失败");
            let _ = stream.shutdown().await;
            anyhow::bail!("本地 127.0.0.1:{port} 连不上（服务没启动，或端口不对）");
        }
    };
    let _ = local.set_nodelay(true);

    // BufReader 可能已经预读了一部分请求体，先把它交给本地服务
    let buffered = reader.buffer().to_vec();
    if !buffered.is_empty() {
        local.write_all(&buffered).await?;
    }

    tokio::io::copy_bidirectional(&mut stream, &mut local).await?;
    Ok(())
}
