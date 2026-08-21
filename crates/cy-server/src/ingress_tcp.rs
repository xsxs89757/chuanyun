//! TCP 入口：给每条 TCP 隧道分配一个公网端口。
//!
//! 和 HTTP 入口的区别在于「怎么知道该转发给谁」。HTTP 靠 Host 头，一个 443
//! 端口就能服务所有隧道；TCP 没有这种带内信息，只能一条隧道占一个端口——
//! 谁连上这个端口，就转给谁。
//!
//! 所以端口是稀缺资源：池子配多大，就最多能同时开多少条 TCP 隧道。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::registry::Registry;

/// 公网端口池。
///
/// 隧道开通时借一个端口并起监听，关闭时还回去。监听器和隧道同生共死——
/// 端口一旦释放就该立刻停止接受连接，否则下一个借到这个端口的人会收到
/// 上一条隧道的流量。
pub struct PortPool {
    range: (u16, u16),
    public_host: String,
    /// 端口 → 停掉该端口监听器的开关
    leased: Mutex<HashMap<u16, CancellationToken>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LeaseError {
    /// 池子里没有空闲端口了
    Exhausted,
    /// 指定的端口已被占用
    Taken,
    /// 指定的端口不在池子范围内
    OutOfRange,
    /// 端口在系统层面绑不上（可能被机器上别的程序占了）
    BindFailed,
}

impl PortPool {
    pub fn new(range: (u16, u16), public_host: impl Into<String>) -> Self {
        Self {
            range,
            public_host: public_host.into(),
            leased: Mutex::new(HashMap::new()),
        }
    }

    /// 借一个端口并起监听。`wanted` 为 `None` 时自动挑一个空闲的。
    ///
    /// 返回对外可见的地址，如 `server.example.com:20017`。
    pub async fn lease(
        &self,
        wanted: Option<u16>,
        tunnel_id: String,
        host_key: String,
        registry: Arc<Registry>,
    ) -> Result<(u16, String), LeaseError> {
        let (lo, hi) = self.range;

        let candidates: Vec<u16> = match wanted {
            Some(p) if p < lo || p > hi => return Err(LeaseError::OutOfRange),
            Some(p) => vec![p],
            None => (lo..=hi).collect(),
        };

        for port in candidates {
            // 先在表里占位再尝试绑定，避免两个请求同时挑中同一个端口
            {
                let mut leased = self.leased.lock().unwrap_or_else(|e| e.into_inner());
                if leased.contains_key(&port) {
                    if wanted.is_some() {
                        return Err(LeaseError::Taken);
                    }
                    continue;
                }
                leased.insert(port, CancellationToken::new());
            }

            match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
                Ok(listener) => {
                    let cancel = {
                        let leased = self.leased.lock().unwrap_or_else(|e| e.into_inner());
                        leased.get(&port).cloned().expect("刚插入")
                    };
                    tokio::spawn(accept_loop(listener, tunnel_id, host_key, registry, cancel));
                    return Ok((port, format!("{}:{port}", self.public_host)));
                }
                Err(e) => {
                    // 绑不上就把占位撤掉，接着试下一个
                    self.leased
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&port);
                    tracing::debug!(port, error = %e, "端口绑定失败");
                    if wanted.is_some() {
                        return Err(LeaseError::BindFailed);
                    }
                }
            }
        }

        Err(LeaseError::Exhausted)
    }

    /// 还回一个端口，同时停掉它的监听器。
    pub fn release(&self, port: u16) {
        if let Some(cancel) = self
            .leased
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&port)
        {
            cancel.cancel();
        }
    }

    pub fn in_use(&self) -> usize {
        self.leased.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 一个公网端口上的接受循环：每来一个连接就开一条数据流转给客户端。
async fn accept_loop(
    listener: tokio::net::TcpListener,
    tunnel_id: String,
    host_key: String,
    registry: Arc<Registry>,
    cancel: CancellationToken,
) {
    loop {
        let (socket, peer) = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "TCP 入口 accept 失败");
                    continue;
                }
            },
        };
        let _ = socket.set_nodelay(true);

        // 每次都重新查注册表，而不是把 Tunnel 捕获进闭包：客户端断线重连后
        // 会话是新的，捏着旧引用会把流量发进一条已经死掉的连接。
        let Some(tunnel) = registry.lookup(&host_key) else {
            tracing::debug!(%host_key, "隧道已不在，拒绝这个连接");
            continue;
        };

        let tunnel_id = tunnel_id.clone();
        tokio::spawn(async move {
            if let Err(e) = pipe(socket, tunnel, tunnel_id, peer.ip()).await {
                tracing::debug!(error = %e, "TCP 转发结束");
            }
        });
    }
    tracing::debug!(%host_key, "TCP 入口已停止");
}

async fn pipe(
    mut socket: tokio::net::TcpStream,
    tunnel: crate::registry::Tunnel,
    tunnel_id: String,
    peer: std::net::IpAddr,
) -> anyhow::Result<()> {
    let mut stream = tunnel.session.mux.open().await?;
    let header = cy_proto::StreamHeader::new(&tunnel_id, cy_proto::TunnelKind::Tcp)
        .with_peer(peer.to_string());
    stream
        .write_all(format!("{}\n", header.to_line()).as_bytes())
        .await?;

    // TCP 隧道就是纯字节管道，两头对拷即可——不需要理解上面跑的是什么协议
    tokio::io::copy_bidirectional(&mut socket, &mut stream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(range: (u16, u16)) -> PortPool {
        PortPool::new(range, "server.example.com")
    }

    async fn lease(p: &PortPool, wanted: Option<u16>) -> Result<(u16, String), LeaseError> {
        p.lease(
            wanted,
            "t1".into(),
            "zhangsan-db.t.example.com".into(),
            Arc::new(Registry::new()),
        )
        .await
    }

    #[tokio::test]
    async fn leases_and_releases() {
        let p = pool((0, 0)); // 端口 0 = 让内核随便给一个，测试里不占固定端口
        let (port, addr) = lease(&p, None).await.unwrap();
        assert!(addr.starts_with("server.example.com:"));
        assert_eq!(p.in_use(), 1);

        p.release(port);
        assert_eq!(p.in_use(), 0);
    }

    #[tokio::test]
    async fn rejects_ports_outside_the_pool() {
        let p = pool((20000, 20010));
        assert_eq!(lease(&p, Some(19999)).await, Err(LeaseError::OutOfRange));
        assert_eq!(lease(&p, Some(20011)).await, Err(LeaseError::OutOfRange));
    }

    #[tokio::test]
    async fn same_port_cannot_be_leased_twice() {
        // 用一段大概率空闲的高端口
        let p = pool((38000, 38000));
        let first = lease(&p, Some(38000)).await;
        if first.is_err() {
            return; // 这台机器上这个端口被别的程序占了，跳过
        }
        assert_eq!(lease(&p, Some(38000)).await, Err(LeaseError::Taken));
    }

    #[tokio::test]
    async fn exhausted_pool_reports_itself() {
        let p = pool((38001, 38002));
        // 借光池子
        let a = lease(&p, None).await;
        let b = lease(&p, None).await;
        if a.is_err() || b.is_err() {
            return; // 端口被别的程序占了，跳过
        }
        assert_eq!(lease(&p, None).await, Err(LeaseError::Exhausted));
    }

    #[tokio::test]
    async fn released_port_can_be_leased_again() {
        let p = pool((0, 0));
        let (port, _) = lease(&p, None).await.unwrap();
        p.release(port);
        // 端口 0 每次都会拿到不同的实际端口，这里验的是账目对得上
        lease(&p, None).await.unwrap();
        assert_eq!(p.in_use(), 1);
    }
}
