//! 控制通道：接受客户端连接、握手鉴权、处理隧道开关。
//!
//! 一条客户端连接的生命周期：
//!
//! ```text
//! TCP accept → TLS 握手 → yamux(Server) → 等对端开第一条流（控制流）
//!            → hello/welcome → 消息循环（open_tunnel / close_tunnel / ping）
//!            → 断开时清理该会话的全部隧道
//! ```

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cy_proto::codec::JsonLines;
use cy_proto::{ClientMsg, Endpoint, ServerMsg, TunnelKind};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::ingress_tcp::{LeaseError, PortPool};
use crate::registry::{OpenOutcome, Registry, Session, Tunnel};
use crate::store::{action, AuditEvent, Auth, Store};

/// 出站消息队列深度。控制消息很少，队列满说明客户端读不动了。
const OUTBOX: usize = 64;

/// 连续多少次心跳没回应就判定连接已死。
const MISSED_HEARTBEATS: u32 = 3;

/// 等客户端开控制流的时限。连上来却不开流的，多半不是我们的客户端。
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    config: Arc<Config>,
    store: Store,
    registry: Arc<Registry>,
    ports: Arc<PortPool>,
    shutdown: CancellationToken,
) {
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let failures = Arc::new(FailureTracker::new(
        config.limits.handshake_fail_count,
        Duration::from_secs(config.limits.handshake_fail_window_secs),
    ));

    loop {
        let (socket, peer) = tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "accept 失败");
                    continue;
                }
            },
        };

        if failures.is_locked(peer.ip()) {
            tracing::warn!(peer = %peer.ip(), "来源已被锁定，拒绝连接");
            continue;
        }

        let acceptor = acceptor.clone();
        let config = config.clone();
        let store = store.clone();
        let registry = registry.clone();
        let ports = ports.clone();
        let failures = failures.clone();
        let shutdown = shutdown.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(
                socket,
                peer.ip(),
                acceptor,
                config,
                store,
                registry,
                ports,
                failures,
                shutdown,
            )
            .await
            {
                tracing::debug!(peer = %peer.ip(), error = %e, "客户端连接结束");
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    socket: tokio::net::TcpStream,
    peer: IpAddr,
    acceptor: tokio_rustls::TlsAcceptor,
    config: Arc<Config>,
    store: Store,
    registry: Arc<Registry>,
    ports: Arc<PortPool>,
    failures: Arc<FailureTracker>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    // Nagle 会把小的控制消息攒着发，握手和心跳都因此变慢
    let _ = socket.set_nodelay(true);

    let tls_stream = acceptor.accept(socket).await?;
    let (mux, mut inbound) = cy_proto::mux::spawn(
        tokio_util::compat::TokioAsyncReadCompatExt::compat(tls_stream),
        yamux::Mode::Server,
    );

    // 控制流是客户端开过来的第一条流
    let control_stream = tokio::time::timeout(CONTROL_STREAM_TIMEOUT, inbound.recv())
        .await
        .map_err(|_| anyhow::anyhow!("等待控制流超时"))?
        .ok_or_else(|| anyhow::anyhow!("连接在开控制流前就断了"))?;

    let mut framed = Framed::new(control_stream, JsonLines::<ClientMsg, ServerMsg>::new());

    // ---- 握手 ----
    let hello = tokio::time::timeout(CONTROL_STREAM_TIMEOUT, framed.next())
        .await
        .map_err(|_| anyhow::anyhow!("等待 hello 超时"))?
        .ok_or_else(|| anyhow::anyhow!("控制流在 hello 之前就关了"))??;

    let (token, client_version, os) = match hello {
        ClientMsg::Hello {
            proto,
            client,
            os,
            token,
        } => {
            if proto != cy_proto::PROTO_VERSION {
                let _ = framed
                    .send(ServerMsg::Error {
                        code: cy_proto::error::code::VERSION.into(),
                        message: format!(
                            "服务端协议版本 {}，客户端 {proto}",
                            cy_proto::PROTO_VERSION
                        ),
                        id: None,
                    })
                    .await;
                anyhow::bail!("协议版本不匹配");
            }
            (token, client, os)
        }
        other => anyhow::bail!("第一条消息应该是 hello，收到 {other:?}"),
    };

    let auth = store.authenticate(&token).await?;
    let (user, max_tunnels) = match auth {
        Auth::Ok { user, max_tunnels } => (user, max_tunnels),
        rejected => {
            let code = match rejected {
                Auth::Expired => cy_proto::error::code::AUTH_EXPIRED,
                Auth::Revoked => cy_proto::error::code::AUTH_REVOKED,
                _ => cy_proto::error::code::AUTH_INVALID,
            };
            failures.record(peer);
            store
                .audit(
                    AuditEvent::new(crate::store::token_hint(&token), action::AUTH_FAIL)
                        .peer(peer.to_string()),
                )
                .await;
            let _ = framed
                .send(ServerMsg::Error {
                    code: code.into(),
                    message: cy_proto::error::human(code).into(),
                    id: None,
                })
                .await;
            anyhow::bail!("鉴权失败: {code}");
        }
    };
    failures.clear(peer);

    let session_id = registry.new_session_id();
    let cancel = CancellationToken::new();
    let (outbox_tx, mut outbox_rx) = tokio::sync::mpsc::channel::<ServerMsg>(OUTBOX);

    let session = Arc::new(Session {
        id: session_id.clone(),
        user: user.clone(),
        mux,
        cancel: cancel.clone(),
        outbox: outbox_tx.clone(),
    });
    registry.register(session.clone());

    tracing::info!(%user, session = %session_id, %peer, client = %client_version, %os, "客户端已登录");
    store
        .audit(AuditEvent::new(&user, action::LOGIN).peer(peer.to_string()))
        .await;

    let (mut sink, mut stream) = framed.split();

    // 所有出站消息走一个写任务：心跳和应答会并发产生，让它们排队而不是抢同一个 sink
    let writer = tokio::spawn(async move {
        while let Some(msg) = outbox_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let result = message_loop(
        &mut stream,
        &outbox_tx,
        &session,
        &registry,
        &ports,
        &store,
        &config,
        max_tunnels,
        &cancel,
        &shutdown,
    )
    .await;

    // ---- 清理 ----
    //
    // 顺序有讲究：出站通道有好几个克隆（我们手上一个、Session 里一个、
    // 正在处理请求的 ingress 可能还攥着 Session），必须都放手，写任务才会看到
    // 通道关闭而退出。漏掉任何一个，这个函数就会一直等在下面那个 await 上，
    // TLS 连接也就不会释放——对端要等到心跳超时才知道我们已经走了。
    cancel.cancel();
    // 先收端口再注销：注销之后就查不到这个会话占了哪些端口，
    // 那些监听器会一直挂着，池子慢慢就漏光了。
    for port in registry.tcp_ports_of_session(&session_id) {
        ports.release(port);
    }
    registry.unregister(&session_id);
    drop(outbox_tx);
    drop(session);

    // 即便如此也不能无限等：万一有请求还攥着 Session 的引用，通道就一直不关。
    // 给一点时间把最后的消息（比如 kick）送出去，然后就该走了。
    if tokio::time::timeout(Duration::from_millis(500), writer)
        .await
        .is_err()
    {
        tracing::debug!(session = %session_id, "写任务未在限时内收尾，直接放手");
    }
    tracing::info!(%user, session = %session_id, "客户端已断开");

    result
}

#[allow(clippy::too_many_arguments)]
async fn message_loop<S>(
    stream: &mut S,
    outbox: &tokio::sync::mpsc::Sender<ServerMsg>,
    session: &Arc<Session>,
    registry: &Arc<Registry>,
    ports: &Arc<PortPool>,
    store: &Store,
    config: &Config,
    max_tunnels: u32,
    cancel: &CancellationToken,
    shutdown: &CancellationToken,
) -> anyhow::Result<()>
where
    S: futures::Stream<Item = Result<ClientMsg, cy_proto::codec::CodecError>> + Unpin,
{
    outbox
        .send(ServerMsg::Welcome {
            session: session.id.clone(),
            server: crate::SERVER_VERSION.into(),
            heartbeat_secs: config.control.heartbeat_secs,
            domain_suffix: config.http.domain_suffix.clone(),
            // 每次握手读一下目录。连接一天也就几次，比起缓存还得想什么时候失效，
            // 直接读简单得多——管理员丢个新包进去，下一个连上来的人就能看到。
            latest_client: config.latest_client_version(),
            download_url: config.admin.download_url.clone(),
        })
        .await?;

    let mut heartbeat = tokio::time::interval(config.control.heartbeat());
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // interval 的第一下是立即触发的，跳过

    let mut seq = 0u64;
    let mut unanswered = 0u32;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = outbox.send(ServerMsg::Kick { reason: "管理员操作".into() }).await;
                // 给写任务一点时间把消息送出去，否则客户端只会看到连接莫名断掉
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Ok(());
            }
            _ = shutdown.cancelled() => return Ok(()),

            _ = heartbeat.tick() => {
                if unanswered >= MISSED_HEARTBEATS {
                    anyhow::bail!("连续 {MISSED_HEARTBEATS} 次心跳无应答");
                }
                seq += 1;
                unanswered += 1;
                if outbox.send(ServerMsg::Ping { seq }).await.is_err() {
                    return Ok(());
                }
            }

            incoming = stream.next() => {
                let Some(msg) = incoming else { return Ok(()) };
                match msg? {
                    ClientMsg::Pong { .. } => unanswered = 0,
                    ClientMsg::Ping { seq } => {
                        let _ = outbox.send(ServerMsg::Pong { seq }).await;
                    }
                    ClientMsg::OpenTunnel { id, kind, name, custom_domain, auth, remote_port } => {
                        // 客户端有消息进来就说明它还活着，不必等 pong
                        unanswered = 0;
                        let reply = open_tunnel(
                            &id, kind, &name, custom_domain, auth, remote_port,
                            session, registry, ports, store, config, max_tunnels,
                        ).await;
                        let _ = outbox.send(reply).await;
                    }
                    ClientMsg::CloseTunnel { id } => {
                        unanswered = 0;
                        if let Some(port) = registry.tcp_port_of(&session.id, &id) {
                            ports.release(port);
                        }
                        if let Some(host) = registry.close_tunnel(&session.id, &id) {
                            store.audit(
                                AuditEvent::new(&session.user, action::CLOSE).tunnel(&id, &host)
                            ).await;
                            tracing::info!(user = %session.user, %host, "隧道已关闭");
                        }
                    }
                    ClientMsg::Hello { .. } => anyhow::bail!("重复的 hello"),
                    ClientMsg::Unknown => {
                        // 新版本客户端发了我们还不认识的消息，跳过就好
                        unanswered = 0;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn open_tunnel(
    id: &str,
    kind: TunnelKind,
    name: &str,
    custom_domain: Option<String>,
    auth: Option<String>,
    remote_port: Option<u16>,
    session: &Arc<Session>,
    registry: &Arc<Registry>,
    ports: &Arc<PortPool>,
    store: &Store,
    config: &Config,
    max_tunnels: u32,
) -> ServerMsg {
    use cy_proto::error::code;

    let fail = |c: &str| ServerMsg::Error {
        code: c.to_string(),
        message: cy_proto::error::human(c).to_string(),
        id: Some(id.to_string()),
    };

    if let Err(c) = cy_proto::naming::validate_name(name) {
        return fail(c);
    }

    if kind == TunnelKind::Tcp {
        return open_tcp_tunnel(
            id,
            name,
            auth,
            remote_port,
            session,
            registry,
            ports,
            store,
            config,
            max_tunnels,
        )
        .await;
    }

    // 自定义域名必须事先由管理员登记给这个用户，否则谁都能声称自己是 pay.example.com
    let host = match &custom_domain {
        Some(domain) => {
            let domain = domain.to_ascii_lowercase();
            match store.custom_domain_owner(&domain).await {
                Ok(Some(owner)) if owner == session.user => domain,
                Ok(_) => return fail(code::SUBDOMAIN_TAKEN),
                Err(e) => {
                    tracing::error!(error = %e, "查自定义域名失败");
                    return fail(code::INTERNAL);
                }
            }
        }
        None => cy_proto::naming::host_for(&session.user, name, &config.http.domain_suffix),
    };

    let tunnel = Tunnel {
        session: session.clone(),
        tunnel_id: id.to_string(),
        name: name.to_string(),
        kind,
        auth,
        tcp_port: None,
    };

    match registry.open_tunnel(&host, tunnel, max_tunnels) {
        OpenOutcome::Opened => {
            let url = format!("{}://{host}", config.http.public_scheme);
            tracing::info!(user = %session.user, %host, "隧道已开通");
            store
                .audit(AuditEvent::new(&session.user, action::OPEN).tunnel(name, &url))
                .await;
            ServerMsg::TunnelOpened {
                id: id.to_string(),
                public: Endpoint::Url(url),
            }
        }
        OpenOutcome::Taken => fail(code::SUBDOMAIN_TAKEN),
        OpenOutcome::LimitReached => fail(code::LIMIT),
    }
}

/// 开一条 TCP 隧道：借一个公网端口，谁连上那个端口就转给这个客户端。
///
/// 和 HTTP 隧道不同，TCP 没有 Host 这种带内信息可以复用一个端口，所以
/// 一条隧道独占一个端口——池子有多大就最多能开多少条。
#[allow(clippy::too_many_arguments)]
async fn open_tcp_tunnel(
    id: &str,
    name: &str,
    auth: Option<String>,
    remote_port: Option<u16>,
    session: &Arc<Session>,
    registry: &Arc<Registry>,
    ports: &Arc<PortPool>,
    store: &Store,
    config: &Config,
    max_tunnels: u32,
) -> ServerMsg {
    use cy_proto::error::code;

    let fail = |c: &str| ServerMsg::Error {
        code: c.to_string(),
        message: cy_proto::error::human(c).to_string(),
        id: Some(id.to_string()),
    };

    // TCP 隧道也占一个「主机名」槽位，好让它和 HTTP 隧道共用同一套
    // 命名、限额与清理逻辑。这个名字不会被解析，只是内部的键。
    let host_key = cy_proto::naming::host_for(&session.user, name, &config.http.domain_suffix);

    // 先占住名字再借端口：反过来的话，撞名时借到的端口要再还回去，
    // 中间那一小段时间里池子是虚耗的。
    let tunnel = Tunnel {
        session: session.clone(),
        tunnel_id: id.to_string(),
        name: name.to_string(),
        kind: TunnelKind::Tcp,
        auth,
        tcp_port: None,
    };
    match registry.open_tunnel(&host_key, tunnel, max_tunnels) {
        OpenOutcome::Opened => {}
        OpenOutcome::Taken => return fail(code::SUBDOMAIN_TAKEN),
        OpenOutcome::LimitReached => return fail(code::LIMIT),
    }

    match ports
        .lease(
            remote_port,
            id.to_string(),
            host_key.clone(),
            registry.clone(),
        )
        .await
    {
        Ok((port, addr)) => {
            registry.set_tcp_port(&host_key, port);
            tracing::info!(user = %session.user, %addr, "TCP 隧道已开通");
            store
                .audit(AuditEvent::new(&session.user, action::OPEN).tunnel(name, &addr))
                .await;
            ServerMsg::TunnelOpened {
                id: id.to_string(),
                public: Endpoint::Addr(addr),
            }
        }
        Err(e) => {
            // 端口没借到，把刚占的名字还回去，别留一条永远不通的隧道
            registry.close_tunnel(&session.id, id);
            fail(match e {
                LeaseError::Exhausted => code::POOL_EXHAUSTED,
                LeaseError::Taken | LeaseError::BindFailed => code::PORT_TAKEN,
                LeaseError::OutOfRange => code::PORT_TAKEN,
            })
        }
    }
}

/// 按来源 IP 记握手失败次数，超阈值就锁一段时间。
///
/// 凭证是 128 位随机数，本来就爆破不动；这道闸主要是别让人白白消耗我们的 CPU
/// 和日志空间。
struct FailureTracker {
    limit: u32,
    window: Duration,
    entries: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl FailureTracker {
    fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn record(&self, ip: IpAddr) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = entries.entry(ip).or_insert((0, now));
        // 窗口过了就重新计数
        if now.duration_since(entry.1) > self.window {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.1 = now;

        // 顺手清理过期条目，免得这张表随着扫描流量无限长大
        entries.retain(|_, (_, at)| now.duration_since(*at) <= self.window);
    }

    fn is_locked(&self, ip: IpAddr) -> bool {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .get(&ip)
            .is_some_and(|(count, at)| *count >= self.limit && at.elapsed() <= self.window)
    }

    fn clear(&self, ip: IpAddr) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn locks_after_repeated_failures() {
        let t = FailureTracker::new(3, Duration::from_secs(60));
        let addr = ip("1.2.3.4");
        assert!(!t.is_locked(addr));
        for _ in 0..3 {
            t.record(addr);
        }
        assert!(t.is_locked(addr));
        // 不该殃及其他来源
        assert!(!t.is_locked(ip("5.6.7.8")));
    }

    #[test]
    fn success_clears_the_count() {
        let t = FailureTracker::new(3, Duration::from_secs(60));
        let addr = ip("1.2.3.4");
        t.record(addr);
        t.record(addr);
        t.clear(addr); // 登录成功
        t.record(addr);
        assert!(!t.is_locked(addr), "成功登录后不该还背着之前的失败次数");
    }

    #[test]
    fn lock_expires_with_the_window() {
        let t = FailureTracker::new(2, Duration::from_millis(50));
        let addr = ip("1.2.3.4");
        t.record(addr);
        t.record(addr);
        assert!(t.is_locked(addr));
        std::thread::sleep(Duration::from_millis(80));
        assert!(!t.is_locked(addr), "窗口过了就该放行");
    }

    #[test]
    fn stale_entries_do_not_accumulate() {
        let t = FailureTracker::new(5, Duration::from_millis(20));
        for i in 0..50u8 {
            t.record(ip(&format!("10.0.0.{i}")));
        }
        std::thread::sleep(Duration::from_millis(40));
        t.record(ip("10.0.1.1"));
        let n = t.entries.lock().unwrap().len();
        assert!(n <= 2, "过期条目应被清掉，现在还有 {n} 条");
    }
}
