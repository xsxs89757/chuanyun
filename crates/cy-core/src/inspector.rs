//! 请求观测与重放。
//!
//! # 为什么值得单独做一层
//!
//! 微信支付的回调只会推送有限几次，推完就没了。调试时最难受的不是看不到请求，
//! 而是**看到了但复现不了**——改一行代码想再试一次，得重新下一单。
//!
//! 所以这里把穿过隧道的请求原样留一份在本地，随时可以重放到本地服务：
//! 同样的报文、同样的签名、同样的时间戳，想调几次调几次。
//!
//! # 边界
//!
//! 记录只存在客户端内存里，不上传、不落盘——里面可能有订单号、手机号、
//! 签名密钥这类东西，它们不该离开这台机器。应用一关就没了。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

/// 单条隧道最多留多少条记录。
///
/// 调试时真正会回头看的就是最近几十条；留太多只是白占内存——
/// 一个上传接口几下就能把几百兆塞进来。
const PER_TUNNEL_LIMIT: usize = 100;

/// 单条记录里报文最多留多少字节。
///
/// 回调报文都很小，几 KB 顶天。超过这个大小的多半是文件上传，
/// 留全文没意义，留个头部足够看清是什么请求。
const BODY_LIMIT: usize = 256 * 1024;

/// 一次穿过隧道的请求。
#[derive(Debug, Clone)]
pub struct Record {
    pub id: u64,
    pub tunnel: String,
    pub at: SystemTime,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 报文被截断了多少字节；0 表示完整
    pub body_truncated: usize,
    /// 响应状态码，还没收到响应时为 None
    pub status: Option<u16>,
    pub duration: Option<Duration>,
    pub peer: Option<String>,
}

impl Record {
    /// 报文的可读形式。不是文本就说明它不是。
    pub fn body_text(&self) -> String {
        match std::str::from_utf8(&self.body) {
            Ok(s) => {
                if self.body_truncated > 0 {
                    format!("{s}\n\n…（还有 {} 字节未记录）", self.body_truncated)
                } else {
                    s.to_string()
                }
            }
            Err(_) => format!("（{} 字节二进制内容）", self.body.len()),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// 请求记录本。可克隆，跨任务共享。
#[derive(Clone, Default)]
pub struct Inspector {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    records: RwLock<VecDeque<Record>>,
    next_id: AtomicU64,
}

impl Inspector {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记下一个请求，返回它的编号（收到响应时用来回填状态码）。
    pub fn record_request(
        &self,
        tunnel: &str,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
        peer: Option<String>,
    ) -> u64 {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (kept, truncated) = if body.len() > BODY_LIMIT {
            (&body[..BODY_LIMIT], body.len() - BODY_LIMIT)
        } else {
            (body, 0)
        };

        let record = Record {
            id,
            tunnel: tunnel.to_string(),
            at: SystemTime::now(),
            method: method.to_string(),
            path: path.to_string(),
            headers: redact(headers),
            body: kept.to_vec(),
            body_truncated: truncated,
            status: None,
            duration: None,
            peer,
        };

        let mut records = self
            .inner
            .records
            .write()
            .unwrap_or_else(|e| e.into_inner());
        records.push_back(record);
        // 按隧道计数：一条隧道刷屏不该把别的隧道的记录挤掉
        while count_for(&records, tunnel) > PER_TUNNEL_LIMIT {
            if let Some(pos) = records.iter().position(|r| r.tunnel == tunnel) {
                records.remove(pos);
            } else {
                break;
            }
        }
        id
    }

    /// 回填响应结果。
    pub fn record_response(&self, id: u64, status: u16, duration: Duration) {
        let mut records = self
            .inner
            .records
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(r) = records.iter_mut().find(|r| r.id == id) {
            r.status = Some(status);
            r.duration = Some(duration);
        }
    }

    /// 某条隧道的记录，新的在前。
    pub fn list(&self, tunnel: Option<&str>) -> Vec<Record> {
        let records = self.inner.records.read().unwrap_or_else(|e| e.into_inner());
        records
            .iter()
            .rev()
            .filter(|r| tunnel.is_none_or(|t| r.tunnel == t))
            .cloned()
            .collect()
    }

    pub fn get(&self, id: u64) -> Option<Record> {
        self.inner
            .records
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    pub fn clear(&self, tunnel: Option<&str>) {
        let mut records = self
            .inner
            .records
            .write()
            .unwrap_or_else(|e| e.into_inner());
        match tunnel {
            Some(t) => records.retain(|r| r.tunnel != t),
            None => records.clear(),
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .records
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn count_for(records: &VecDeque<Record>, tunnel: &str) -> usize {
    records.iter().filter(|r| r.tunnel == tunnel).count()
}

/// 抹掉不该留在记录里的头。
///
/// 观测面板是给人看的，也可能被截图发到群里。Cookie 和 Authorization
/// 拿到手就能冒充本人，留全文没有调试价值，风险却是实打实的。
fn redact(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    const SENSITIVE: &[&str] = &[
        "authorization",
        "cookie",
        "set-cookie",
        "proxy-authorization",
    ];
    headers
        .into_iter()
        .map(|(k, v)| {
            if SENSITIVE.iter().any(|s| k.eq_ignore_ascii_case(s)) {
                (k, "（已隐去）".to_string())
            } else {
                (k, v)
            }
        })
        .collect()
}

/// 把一条记录重放到本地端口。
///
/// 原样重发：同样的方法、路径、头、报文。微信支付的回调带签名，
/// 改动任何一个字节都会验签失败——重放的意义就在于**一字不改**。
pub async fn replay(record: &Record, local_port: u16) -> anyhow::Result<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", local_port)).await?;

    let mut raw = format!("{} {} HTTP/1.1\r\n", record.method, record.path);
    for (k, v) in &record.headers {
        // 被隐去的头不能原样发出去，否则本地服务会收到"（已隐去）"这种鬼值
        if v == "（已隐去）" {
            continue;
        }
        // 重放是一次性的，别让本地服务等着复用连接
        if k.eq_ignore_ascii_case("connection") {
            continue;
        }
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str("Connection: close\r\n");
    raw.push_str(&format!("Content-Length: {}\r\n", record.body.len()));
    raw.push_str("\r\n");

    socket.write_all(raw.as_bytes()).await?;
    socket.write_all(&record.body).await?;
    socket.flush().await?;

    let mut response = Vec::new();
    socket.read_to_end(&mut response).await?;

    let text = String::from_utf8_lossy(&response).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn records_and_lists_newest_first() {
        let i = Inspector::new();
        i.record_request("wx", "POST", "/cb", headers(&[]), b"first", None);
        i.record_request("wx", "GET", "/health", headers(&[]), b"", None);

        let list = i.list(Some("wx"));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path, "/health", "新的应该在前面");
    }

    #[test]
    fn response_is_filled_back_in() {
        let i = Inspector::new();
        let id = i.record_request("wx", "POST", "/cb", headers(&[]), b"x", None);
        assert!(i.get(id).unwrap().status.is_none());

        i.record_response(id, 200, Duration::from_millis(42));
        let r = i.get(id).unwrap();
        assert_eq!(r.status, Some(200));
        assert_eq!(r.duration, Some(Duration::from_millis(42)));
    }

    #[test]
    fn sensitive_headers_are_redacted() {
        // 观测面板会被截图发群里，Cookie 拿到手就能冒充本人
        let i = Inspector::new();
        let id = i.record_request(
            "wx",
            "GET",
            "/",
            headers(&[
                ("Cookie", "session=abc123"),
                ("Authorization", "Bearer secret"),
                ("Content-Type", "application/json"),
            ]),
            b"",
            None,
        );
        let r = i.get(id).unwrap();
        assert_eq!(r.header("Cookie"), Some("（已隐去）"));
        assert_eq!(r.header("Authorization"), Some("（已隐去）"));
        // 普通的头要留着，不然就没法调试了
        assert_eq!(r.header("Content-Type"), Some("application/json"));
    }

    #[test]
    fn oversized_bodies_are_truncated_not_dropped() {
        let i = Inspector::new();
        let big = vec![b'x'; BODY_LIMIT + 5000];
        let id = i.record_request("up", "POST", "/upload", headers(&[]), &big, None);

        let r = i.get(id).unwrap();
        assert_eq!(r.body.len(), BODY_LIMIT);
        assert_eq!(r.body_truncated, 5000);
        assert!(r.body_text().contains("还有 5000 字节未记录"));
    }

    #[test]
    fn per_tunnel_limit_does_not_starve_other_tunnels() {
        let i = Inspector::new();
        i.record_request("quiet", "GET", "/once", headers(&[]), b"", None);
        // 另一条隧道刷屏
        for n in 0..PER_TUNNEL_LIMIT + 20 {
            i.record_request("noisy", "GET", &format!("/{n}"), headers(&[]), b"", None);
        }

        assert_eq!(i.list(Some("noisy")).len(), PER_TUNNEL_LIMIT);
        assert_eq!(
            i.list(Some("quiet")).len(),
            1,
            "一条隧道刷屏不该把别的隧道的记录挤掉"
        );
    }

    #[test]
    fn clear_can_target_one_tunnel() {
        let i = Inspector::new();
        i.record_request("a", "GET", "/", headers(&[]), b"", None);
        i.record_request("b", "GET", "/", headers(&[]), b"", None);

        i.clear(Some("a"));
        assert!(i.list(Some("a")).is_empty());
        assert_eq!(i.list(Some("b")).len(), 1);

        i.clear(None);
        assert!(i.is_empty());
    }

    #[test]
    fn binary_bodies_are_described_not_mangled() {
        let i = Inspector::new();
        let id = i.record_request("up", "POST", "/", headers(&[]), &[0xff, 0xfe, 0x00], None);
        let text = i.get(id).unwrap().body_text();
        assert!(text.contains("二进制"), "实际：{text}");
    }

    /// 重放要一字不改地重发——微信支付回调带签名，改一个字节就验签失败。
    #[tokio::test]
    async fn replay_resends_the_exact_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 起一个把收到的请求原样回显的服务
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(RwLock::new(String::new()));
        let sink = received.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            *sink.write().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
        });

        let i = Inspector::new();
        let body = br#"{"out_trade_no":"X1","sign":"ABC"}"#;
        let id = i.record_request(
            "wx",
            "POST",
            "/notify",
            headers(&[("Content-Type", "application/json"), ("X-Wx-Sign", "ABC")]),
            body,
            Some("1.2.3.4".into()),
        );

        let (status, _) = replay(&i.get(id).unwrap(), port).await.unwrap();
        assert_eq!(status, 200);

        let raw = received.read().unwrap().clone();
        assert!(raw.starts_with("POST /notify HTTP/1.1"), "实际：{raw}");
        assert!(raw.contains("X-Wx-Sign: ABC"), "签名头必须原样带上");
        assert!(
            raw.contains(r#"{"out_trade_no":"X1","sign":"ABC"}"#),
            "报文必须一字不改"
        );
    }

    #[tokio::test]
    async fn replay_omits_redacted_headers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(RwLock::new(String::new()));
        let sink = received.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            *sink.write().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        });

        let i = Inspector::new();
        let id = i.record_request("a", "GET", "/", headers(&[("Cookie", "s=1")]), b"", None);
        replay(&i.get(id).unwrap(), port).await.unwrap();

        let raw = received.read().unwrap().clone();
        assert!(
            !raw.contains("（已隐去）"),
            "隐去的头不该原样发给本地服务：{raw}"
        );
    }
}
