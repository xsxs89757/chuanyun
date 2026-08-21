//! 接入：把同事的隧道映射成本地的一个端口。
//!
//! 隧道解决的是「把我的本地服务给别人用」，接入解决的是反过来的事：
//! **用别人的服务，但让我的代码以为它在本地**。
//!
//! 前端同事不想在本机跑一整套后端，想直接用张三机器上那份（连着他的测试数据）。
//! 最土的办法是把 proxy 目标改成张三的隧道地址——能用，但那个地址进了配置文件、
//! 进了 git，明天张三换个名字就全乱了。
//!
//! 接入的做法是在本地起一个小代理：
//!
//! ```text
//! 你的前端 ──▶ 127.0.0.1:8082 ──▶ https://zhangsan-api.t.example.com ──▶ 张三的机器
//! ```
//!
//! 消费方的配置永远指向 `127.0.0.1:8082`，不感知上游是谁。「连自己的 / 连张三的 /
//! 连测试环境」就只是开关哪条接入的区别，代码和配置一个字都不用改。
//!
//! 实现上这纯粹是客户端的事——不改协议，也不需要服务端配合。流量走的还是
//! 既有链路：你 → nginx → 服务端 → 张三的客户端 → 张三的本地服务。

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// 一条接入的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectSpec {
    /// 本地映射到哪个端口
    pub local_port: u16,
    /// 上游地址。可以是完整 URL，也可以只写隧道名（如 `zhangsan-api`），
    /// 后者会按当前服务器的域名后缀补全。
    pub from: String,
    /// 对方隧道设了访问口令的话填这里，`用户名:口令`
    pub auth: Option<String>,
}

impl ConnectSpec {
    pub fn new(local_port: u16, from: impl Into<String>) -> Self {
        Self {
            local_port,
            from: from.into(),
            auth: None,
        }
    }

    pub fn with_auth(mut self, auth: impl Into<String>) -> Self {
        let auth = auth.into();
        self.auth = (!auth.is_empty()).then_some(auth);
        self
    }

    /// 把 `from` 补成完整 URL。
    ///
    /// 日常写法是只写隧道名（`zhangsan-api`）——同一台服务器上的同事，
    /// 域名后缀都一样，让用户每次重复它没有意义。
    pub fn upstream_url(&self, domain_suffix: &str) -> Result<String, ConnectError> {
        if self.from.starts_with("http://") || self.from.starts_with("https://") {
            return Ok(self.from.trim_end_matches('/').to_string());
        }
        if self.from.contains('.') {
            // 写了完整域名但没写协议
            return Ok(format!("https://{}", self.from.trim_end_matches('/')));
        }
        if domain_suffix.is_empty() {
            // 短写法要靠域名后缀补全，而后缀是登录后服务端告诉我们的。
            // 没有它就拼不出地址——与其产出一个 "https://zhangsan-api." 这种
            // 一看就坏的东西，不如直接说清楚。
            return Err(ConnectError::UnknownSuffix(self.from.clone()));
        }
        Ok(format!("https://{}.{}", self.from, domain_suffix))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("本地端口 {0} 已被占用——多半是你自己也起了这个服务。停掉它，或者换个端口。")]
    PortInUse(u16),
    #[error("起监听失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("上游地址不对: {0}")]
    BadUpstream(String),
    #[error("还不知道隧道域名后缀，没法把 {0} 补成完整地址。先登录，或者把上游写成完整的 https:// 地址。")]
    UnknownSuffix(String),
}

/// 一条跑起来的接入。
#[derive(Debug)]
pub struct ActiveConnect {
    pub spec: ConnectSpec,
    pub upstream: String,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ActiveConnect {
    /// 停掉接入，并且**等到端口真的放开**再返回。
    ///
    /// 只发取消信号是不够的：监听器活在那个任务里，任务没退出之前端口还占着。
    /// 换上游（同一个端口重新接入）时就会撞上——先前那条还没让位，新的绑不上。
    pub async fn stop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    pub fn is_running(&self) -> bool {
        !self.cancel.is_cancelled()
    }
}

impl Drop for ActiveConnect {
    fn drop(&mut self) {
        // 没走 stop() 就被丢掉时的兜底。任务会自己退出，只是不保证时机。
        self.cancel.cancel();
    }
}

/// 起一条接入。
pub async fn start(spec: ConnectSpec, domain_suffix: &str) -> Result<ActiveConnect, ConnectError> {
    let upstream = spec.upstream_url(domain_suffix)?;
    let target = UpstreamTarget::parse(&upstream)?;

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", spec.local_port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // 这是最常见的情况，而且原因通常很具体：用户自己把服务跑起来了。
            // 说清楚比抛一个 AddrInUse 有用得多。
            return Err(ConnectError::PortInUse(spec.local_port));
        }
        Err(e) => return Err(ConnectError::Io(e)),
    };

    let cancel = CancellationToken::new();
    let target = Arc::new(target);
    let auth = spec.auth.clone();

    let task = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let (socket, _) = tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(error = %e, "接入 accept 失败");
                            continue;
                        }
                    },
                };
                let _ = socket.set_nodelay(true);

                let target = target.clone();
                let auth = auth.clone();
                tokio::spawn(async move {
                    if let Err(e) = proxy_once(socket, &target, auth.as_deref()).await {
                        tracing::debug!(error = %e, "接入转发结束");
                    }
                });
            }
            tracing::info!(port = spec.local_port, "接入已停止");
        })
    };

    tracing::info!(port = spec.local_port, %upstream, "接入已就绪");
    Ok(ActiveConnect {
        spec,
        upstream,
        cancel,
        task: Some(task),
    })
}

/// 上游是什么：连哪里、用不用 TLS、Host 头写什么。
struct UpstreamTarget {
    host: String,
    port: u16,
    tls: bool,
}

impl UpstreamTarget {
    fn parse(url: &str) -> Result<Self, ConnectError> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(ConnectError::BadUpstream(format!(
                "{url} 缺少 http:// 或 https:// 前缀"
            )));
        };

        let authority = rest.split('/').next().unwrap_or(rest);
        if authority.is_empty() {
            return Err(ConnectError::BadUpstream(format!("{url} 没有域名")));
        }

        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse()
                    .map_err(|_| ConnectError::BadUpstream(format!("{url} 的端口不是数字")))?,
            ),
            None => (authority.to_string(), if tls { 443 } else { 80 }),
        };

        Ok(Self { host, port, tls })
    }
}

/// 把一条本地连接转给上游。
///
/// 这里要改写 Host 头：本地连进来的请求 Host 是 `127.0.0.1:8082`，
/// 而上游靠 Host 分辨该路由到哪条隧道——不改的话请求会被当成"找不到隧道"。
async fn proxy_once(
    mut local: tokio::net::TcpStream,
    target: &UpstreamTarget,
    auth: Option<&str>,
) -> anyhow::Result<()> {
    // 先把请求头读出来（到 \r\n\r\n 为止），改写之后再连上游
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !ends_with_blank_line(&head) {
        let n = local.read(&mut byte).await?;
        if n == 0 {
            anyhow::bail!("连接在请求头发完之前就断了");
        }
        head.push(byte[0]);
        if head.len() > 64 * 1024 {
            anyhow::bail!("请求头过大");
        }
    }

    let rewritten = rewrite_head(&head, &target.host, auth)?;

    let upstream = tokio::net::TcpStream::connect((target.host.as_str(), target.port)).await?;
    let _ = upstream.set_nodelay(true);

    if target.tls {
        let connector = tls_connector()?;
        let name = rustls::pki_types::ServerName::try_from(target.host.clone())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut upstream = connector.connect(name, upstream).await?;
        upstream.write_all(&rewritten).await?;
        tokio::io::copy_bidirectional(&mut local, &mut upstream).await?;
    } else {
        let mut upstream = upstream;
        upstream.write_all(&rewritten).await?;
        tokio::io::copy_bidirectional(&mut local, &mut upstream).await?;
    }
    Ok(())
}

fn tls_connector() -> anyhow::Result<tokio_rustls::TlsConnector> {
    // 上游是公网地址（经 nginx 出来的正规证书），走系统信任链即可——
    // 这里和控制通道的自签证书是两回事，别把 pin 那套搬过来。
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

fn ends_with_blank_line(buf: &[u8]) -> bool {
    buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n"
}

/// 改写请求头：换 Host，必要时补上访问口令。
fn rewrite_head(head: &[u8], host: &str, auth: Option<&str>) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;

    let text = std::str::from_utf8(head)?;
    let mut out = String::with_capacity(text.len() + 64);
    let mut host_written = false;

    for (i, line) in text.split("\r\n").enumerate() {
        if i == 0 {
            out.push_str(line);
            out.push_str("\r\n");
            continue;
        }
        if line.is_empty() {
            break; // 头结束
        }
        if line.to_ascii_lowercase().starts_with("host:") {
            out.push_str(&format!("Host: {host}\r\n"));
            host_written = true;
            continue;
        }
        // 已有的 Authorization 让位给我们要注入的那个
        if auth.is_some() && line.to_ascii_lowercase().starts_with("authorization:") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }

    if !host_written {
        out.push_str(&format!("Host: {host}\r\n"));
    }
    if let Some(auth) = auth {
        let encoded = base64::engine::general_purpose::STANDARD.encode(auth);
        out.push_str(&format!("Authorization: Basic {encoded}\r\n"));
    }
    out.push_str("\r\n");
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_form_is_completed_with_the_domain_suffix() {
        // 日常写法：只写隧道名
        let s = ConnectSpec::new(8082, "zhangsan-api");
        assert_eq!(
            s.upstream_url("t.example.com").unwrap(),
            "https://zhangsan-api.t.example.com"
        );
    }

    #[test]
    fn full_urls_are_left_alone() {
        // 连外部环境（测试服、第三方）时写完整地址
        let s = ConnectSpec::new(8082, "https://staging.example.com");
        assert_eq!(
            s.upstream_url("t.example.com").unwrap(),
            "https://staging.example.com"
        );

        let s = ConnectSpec::new(8082, "http://192.168.1.5:8080");
        assert_eq!(
            s.upstream_url("t.example.com").unwrap(),
            "http://192.168.1.5:8080"
        );
    }

    #[test]
    fn bare_domains_get_https() {
        let s = ConnectSpec::new(8082, "demo.example.com");
        assert_eq!(
            s.upstream_url("t.example.com").unwrap(),
            "https://demo.example.com"
        );
    }

    #[test]
    fn trailing_slash_is_trimmed() {
        let s = ConnectSpec::new(8082, "https://a.example.com/");
        assert_eq!(
            s.upstream_url("t.example.com").unwrap(),
            "https://a.example.com"
        );
    }

    #[test]
    fn short_form_without_a_suffix_says_what_to_do() {
        // 还没登录就想用短写法——地址拼不出来，得说清楚而不是产出半截 URL
        let s = ConnectSpec::new(8082, "zhangsan-api");
        let err = s.upstream_url("").unwrap_err().to_string();
        assert!(err.contains("先登录"), "实际：{err}");
        assert!(err.contains("完整的 https://"), "该给出第二条路：{err}");
    }

    #[test]
    fn full_urls_work_even_without_a_suffix() {
        // 写全了就不需要后缀，没登录也能用
        let s = ConnectSpec::new(8082, "https://staging.example.com");
        assert_eq!(s.upstream_url("").unwrap(), "https://staging.example.com");
    }

    #[test]
    fn upstream_parsing() {
        let t = UpstreamTarget::parse("https://a.example.com").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.tls),
            ("a.example.com", 443, true)
        );

        let t = UpstreamTarget::parse("http://a.example.com").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.tls),
            ("a.example.com", 80, false)
        );

        let t = UpstreamTarget::parse("http://127.0.0.1:9000/path").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.tls), ("127.0.0.1", 9000, false));

        assert!(UpstreamTarget::parse("a.example.com").is_err());
        assert!(UpstreamTarget::parse("https://a.example.com:notaport").is_err());
    }

    #[test]
    fn host_header_is_rewritten() {
        // 本地连进来的 Host 是 127.0.0.1:8082，上游靠 Host 找隧道，不改就找不到
        let head = b"GET /api/users HTTP/1.1\r\nHost: 127.0.0.1:8082\r\nAccept: */*\r\n\r\n";
        let out = rewrite_head(head, "zhangsan-api.t.example.com", None).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.starts_with("GET /api/users HTTP/1.1\r\n"));
        assert!(text.contains("Host: zhangsan-api.t.example.com\r\n"));
        assert!(!text.contains("127.0.0.1:8082"), "旧的 Host 不该留下");
        assert!(text.contains("Accept: */*\r\n"), "其他头要原样保留");
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn auth_is_injected_when_configured() {
        let head = b"GET / HTTP/1.1\r\nHost: 127.0.0.1:8082\r\n\r\n";
        let out = rewrite_head(head, "a.example.com", Some("demo:s3cret")).unwrap();
        let text = String::from_utf8(out).unwrap();
        // ZGVtbzpzM2NyZXQ= 是 demo:s3cret 的 base64
        assert!(
            text.contains("Authorization: Basic ZGVtbzpzM2NyZXQ="),
            "实际：{text}"
        );
    }

    #[test]
    fn injected_auth_replaces_any_existing_one() {
        let head = b"GET / HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer old\r\n\r\n";
        let out = rewrite_head(head, "a.example.com", Some("demo:s3cret")).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("Bearer old"), "旧的凭证不该同时发出去");
        assert_eq!(text.matches("Authorization:").count(), 1);
    }

    #[test]
    fn missing_host_header_is_added() {
        let head = b"GET / HTTP/1.0\r\nAccept: */*\r\n\r\n";
        let out = rewrite_head(head, "a.example.com", None).unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("Host: a.example.com"));
    }

    #[tokio::test]
    async fn port_conflict_says_something_useful() {
        // 用户自己把服务跑起来了，占了同一个端口
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let err = start(ConnectSpec::new(port, "zhangsan-api"), "t.example.com")
            .await
            .expect_err("端口被占应当失败");
        let msg = err.to_string();
        assert!(msg.contains("已被占用"), "实际：{msg}");
        assert!(msg.contains("停掉它"), "该告诉用户怎么办：{msg}");
    }
}
