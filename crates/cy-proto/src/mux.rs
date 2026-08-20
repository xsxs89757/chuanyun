//! yamux 多路复用驱动（feature `mux`）。
//!
//! rust-yamux 从 0.12 起去掉了 `Control` 句柄，`Connection` 必须由一个任务独占轮询：
//! 收对端开的流用 `poll_next_inbound`，自己开流用 `poll_new_outbound`，两者都要 `&mut`。
//! 所以这里起一个 driver 任务独占 `Connection`，外部通过通道跟它打交道。
//!
//! 服务端和客户端用的是同一套样板——放在协议层共享，而不是各抄一份。并发代码
//! 抄两份的下场是某天在一边修了 bug、另一边没修。

use std::collections::VecDeque;
use std::future::poll_fn;
use std::task::Poll;

use futures::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt};

/// 一条 yamux 流，已包装成 tokio 的 `AsyncRead`/`AsyncWrite`。
///
/// yamux 本身用的是 futures-io 那套 trait，而我们下游（hyper、`copy_bidirectional`）
/// 要的是 tokio 的，所以在边界上一次性转换好，别让 compat 泄漏到业务代码里。
pub type MuxStream = Compat<yamux::Stream>;

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("多路复用连接已关闭")]
    Closed,
    #[error("yamux 错误: {0}")]
    Yamux(#[from] yamux::ConnectionError),
}

type OpenRequest = oneshot::Sender<Result<yamux::Stream, yamux::ConnectionError>>;

/// 主动开流的句柄，可克隆、可跨任务共享。
#[derive(Clone, Debug)]
pub struct MuxHandle {
    open_tx: mpsc::Sender<OpenRequest>,
}

impl MuxHandle {
    /// 开一条新流。连接已断时返回 [`MuxError::Closed`]。
    pub async fn open(&self) -> Result<MuxStream, MuxError> {
        let (tx, rx) = oneshot::channel();
        self.open_tx.send(tx).await.map_err(|_| MuxError::Closed)?;
        let stream = rx.await.map_err(|_| MuxError::Closed)??;
        Ok(stream.compat())
    }

    /// 连接是否已经断开。
    pub fn is_closed(&self) -> bool {
        self.open_tx.is_closed()
    }
}

/// 同时排队的开流请求上限。
///
/// HTTP 场景下一个网页可能瞬间要几十条流，队列太浅会让请求白等；但也不能无界，
/// 否则对端卡住时我们会把内存吃光。
const OPEN_QUEUE: usize = 256;

/// 启动 driver 任务，返回「开流句柄」和「对端开过来的流」的接收端。
///
/// driver 在连接断开时结束，届时句柄的 [`MuxHandle::is_closed`] 变真、
/// 入站接收端返回 `None`。
pub fn spawn<T>(socket: T, mode: yamux::Mode) -> (MuxHandle, mpsc::Receiver<MuxStream>)
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (open_tx, mut open_rx) = mpsc::channel::<OpenRequest>(OPEN_QUEUE);
    let (inbound_tx, inbound_rx) = mpsc::channel::<MuxStream>(OPEN_QUEUE);

    let mut config = yamux::Config::default();
    // 每个 HTTP 请求一条流，突发时并发很高，默认上限（256）容易顶到。
    config.set_max_num_streams(4096);

    let mut conn = yamux::Connection::new(socket, config, mode);

    tokio::spawn(async move {
        let mut pending: VecDeque<OpenRequest> = VecDeque::new();

        loop {
            // 三件事在同一个 poll 里做完：收开流请求、开流、收入站流。
            // 用一个 poll_fn 而不是 select!，是因为 Connection 只能有一个 &mut——
            // 同一次唤醒里三个来源都注册了 waker，不会漏事件。
            let step = poll_fn(|cx| {
                // 1) 收开流请求（队列没满时才收，形成背压）
                while pending.len() < OPEN_QUEUE {
                    match open_rx.poll_recv(cx) {
                        Poll::Ready(Some(req)) => pending.push_back(req),
                        // 所有句柄都没了，但入站可能还有用，继续跑
                        Poll::Ready(None) => break,
                        Poll::Pending => break,
                    }
                }

                // 2) 尽量满足排队的开流请求
                while !pending.is_empty() {
                    match conn.poll_new_outbound(cx) {
                        Poll::Ready(result) => {
                            let req = pending.pop_front().expect("刚判过非空");
                            let failed = result.is_err();
                            // 请求方可能已经不等了（超时/取消），忽略发送失败
                            let _ = req.send(result);
                            if failed {
                                return Poll::Ready(Step::Done);
                            }
                        }
                        Poll::Pending => break,
                    }
                }

                // 3) 收对端开过来的流
                match conn.poll_next_inbound(cx) {
                    Poll::Ready(Some(Ok(stream))) => Poll::Ready(Step::Inbound(stream)),
                    Poll::Ready(Some(Err(e))) => Poll::Ready(Step::Error(e)),
                    Poll::Ready(None) => Poll::Ready(Step::Done),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await;

            match step {
                Step::Inbound(stream) => {
                    if inbound_tx.send(stream.compat()).await.is_err() {
                        // 没人接收入站流了，收摊
                        break;
                    }
                }
                Step::Error(e) => {
                    tracing::debug!(error = %e, "yamux 连接出错，driver 退出");
                    break;
                }
                Step::Done => break,
            }
        }

        // 退出前把还在排队的请求告知失败，别让调用方一直挂着
        for req in pending {
            let _ = req.send(Err(yamux::ConnectionError::Closed));
        }
    });

    (MuxHandle { open_tx }, inbound_rx)
}

enum Step {
    Inbound(yamux::Stream),
    Error(yamux::ConnectionError),
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::compat::TokioAsyncReadCompatExt;

    /// 用一对内存管道把两端接起来，跑一轮双向开流。
    #[tokio::test]
    async fn both_sides_can_open_streams() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (client, mut client_inbound) = spawn(a.compat(), yamux::Mode::Client);
        let (server, mut server_inbound) = spawn(b.compat(), yamux::Mode::Server);

        // 客户端开流 → 服务端收到
        let mut cs = client.open().await.unwrap();
        cs.write_all(b"ping").await.unwrap();
        cs.flush().await.unwrap();
        let mut ss = server_inbound.recv().await.expect("服务端应收到一条流");
        let mut buf = [0u8; 4];
        ss.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        // 服务端也能主动开流 → 这是数据面的关键：外部请求到达时由服务端发起
        let mut ss2 = server.open().await.unwrap();
        ss2.write_all(b"pong").await.unwrap();
        ss2.flush().await.unwrap();
        let mut cs2 = client_inbound.recv().await.expect("客户端应收到一条流");
        let mut buf = [0u8; 4];
        cs2.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn many_concurrent_streams() {
        let (a, b) = tokio::io::duplex(256 * 1024);
        let (client, _client_inbound) = spawn(a.compat(), yamux::Mode::Client);
        let (_server, mut server_inbound) = spawn(b.compat(), yamux::Mode::Server);

        // 服务端把收到的每条流回声一遍
        tokio::spawn(async move {
            while let Some(mut s) = server_inbound.recv().await {
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let _ = s.read_to_end(&mut buf).await;
                    let _ = s.write_all(&buf).await;
                    let _ = s.shutdown().await;
                });
            }
        });

        // 一次开 64 条，模拟网页并发拉资源
        let mut tasks = Vec::new();
        for i in 0..64u32 {
            let client = client.clone();
            tasks.push(tokio::spawn(async move {
                let mut s = client.open().await.unwrap();
                let msg = format!("stream-{i}");
                s.write_all(msg.as_bytes()).await.unwrap();
                s.shutdown().await.unwrap();
                let mut echoed = String::new();
                s.read_to_string(&mut echoed).await.unwrap();
                assert_eq!(echoed, msg);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    }

    #[tokio::test]
    async fn open_fails_after_peer_disappears() {
        let (a, b) = tokio::io::duplex(1024);
        let (client, _inbound) = spawn(a.compat(), yamux::Mode::Client);
        drop(b); // 对端没了

        // 可能立刻失败，也可能要等 driver 察觉——两种都算正确，不该挂死
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if client.open().await.is_err() {
                    return;
                }
            }
        })
        .await;
        assert!(result.is_ok(), "对端消失后开流应当失败而不是一直挂着");
    }
}
