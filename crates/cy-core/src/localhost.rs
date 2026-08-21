//! 连本机端口。
//!
//! 看着是一行 `TcpStream::connect` 的事，实际不是：**很多 dev server 只绑一种
//! 地址族**。Vite 在 macOS 上默认只监听 `[::1]`，只连 `127.0.0.1` 会直接被拒；
//! 反过来也有服务只绑 IPv4。
//!
//! 用户看到的会是「隧道那头没有响应」，而他明明看着 dev server 在跑——
//! 这种问题极难自查。所以两种都试。

use std::io;

use tokio::net::TcpStream;

/// 连接本机的某个端口，IPv4 与 IPv6 都试。
///
/// 先试 IPv4（更常见），不通再试 IPv6。两个都不通时返回后一个错误。
pub async fn connect(port: u16) -> io::Result<TcpStream> {
    let v4 = TcpStream::connect(("127.0.0.1", port)).await;
    if v4.is_ok() {
        return v4;
    }
    match TcpStream::connect(("::1", port)).await {
        Ok(s) => {
            tracing::debug!(port, "本地服务只在 IPv6 上监听");
            Ok(s)
        }
        // 两边都不通，多半是服务根本没起——把这个更常见的原因（IPv4 那次）报上去
        Err(_) => v4,
    }
}

/// 连不上时给用户看的话。
pub fn unreachable_message(port: u16) -> String {
    format!(
        "本地 {port} 端口连不上（IPv4 和 IPv6 都试过了）。\
         检查服务是否已启动、端口是否填对。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn echo_on(addr: &str) -> u16 {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 16];
                    if let Ok(n) = socket.read(&mut buf).await {
                        let _ = socket.write_all(&buf[..n]).await;
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn reaches_ipv4_only_services() {
        let port = echo_on("127.0.0.1:0").await;
        let mut s = connect(port).await.expect("应当连得上 IPv4 服务");
        s.write_all(b"hi").await.unwrap();
        let mut buf = [0u8; 2];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
    }

    /// Vite 在 macOS 上默认就是这样——只绑 `[::1]`。
    ///
    /// 早先只连 127.0.0.1，结果所有 Vite 项目穿透之后都是「隧道那头没有响应」，
    /// 而用户明明看着 dev server 在跑。
    #[tokio::test]
    async fn reaches_ipv6_only_services() {
        let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
            return; // 这台机器没开 IPv6，跳过
        };
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 16];
                    if let Ok(n) = socket.read(&mut buf).await {
                        let _ = socket.write_all(&buf[..n]).await;
                    }
                });
            }
        });

        let mut s = connect(port)
            .await
            .expect("只绑 IPv6 的服务也该连得上（Vite 默认就是这样）");
        s.write_all(b"v6").await.unwrap();
        let mut buf = [0u8; 2];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"v6");
    }

    #[tokio::test]
    async fn reports_failure_when_nothing_listens() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(connect(port).await.is_err());
        assert!(unreachable_message(port).contains("都试过了"));
    }
}
