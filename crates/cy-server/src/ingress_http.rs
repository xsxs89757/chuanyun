//! HTTP 入口：把外部请求按 Host 送进对应客户端的隧道。
//!
//! # 为什么是「按请求」路由而不是「按连接」
//!
//! 一个看起来更省事的做法是：嗅探连接上第一个请求的 Host，然后把整条 TCP 连接
//! 原样透传给客户端。这在 nginx 前置的场景下是错的——nginx 开了 upstream keepalive
//! 之后，同一条到我们的连接会**混载不同子域名的请求**，按连接路由会把张三的请求
//! 送进李四的隧道。
//!
//! 所以这里让 hyper 完整解析每个请求再逐个路由。顺带还拿到了三样东西：能返回
//! 友好的错误页、能正确追加 `X-Forwarded-For`、以后做请求观测有现成的元数据。
//!
//! # 一个请求的走向
//!
//! ```text
//! nginx ─▶ hyper server ─▶ 查注册表 ─▶ 开一条 yamux 流 ─▶ 写流头
//!                                                       └▶ 在这条流上跑 hyper client
//!                                                          ─▶ 客户端 ─▶ 本地服务
//! ```
//!
//! 转发时不手写 HTTP 字节，而是把已解析的 `Request` 交给 hyper 的客户端连接——
//! chunked 编码、Content-Length、连接头的处理都有现成的正确实现，自己拼字节
//! 迟早在某个边角上出错。

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, CONNECTION, HOST, UPGRADE};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::registry::{Registry, Tunnel};

type Body = BoxBody<Bytes, hyper::Error>;

struct Ctx {
    config: Arc<Config>,
    registry: Arc<Registry>,
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    config: Arc<Config>,
    registry: Arc<Registry>,
    shutdown: CancellationToken,
) {
    // 直连模式：自己终止 TLS。默认走 nginx 前置，证书归 nginx 管，这里就是明文。
    let acceptor = match load_direct_tls(&config) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "直连模式的证书加载失败，HTTP 入口未启动");
            return;
        }
    };

    let ctx = Arc::new(Ctx { config, registry });

    loop {
        let (socket, peer) = tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "HTTP 入口 accept 失败");
                    continue;
                }
            },
        };
        let _ = socket.set_nodelay(true);

        let ctx = ctx.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let ctx = ctx.clone();
                async move { Ok::<_, Infallible>(handle(req, peer, ctx).await) }
            });

            // `with_upgrades` 是 WebSocket 能透传的前提：没有它，101 之后的字节
            // 就没人接管了。
            let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            let result = match acceptor {
                Some(acceptor) => match acceptor.accept(socket).await {
                    Ok(tls) => {
                        builder
                            .serve_connection_with_upgrades(TokioIo::new(tls), service)
                            .await
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "TLS 握手失败");
                        return;
                    }
                },
                None => {
                    builder
                        .serve_connection_with_upgrades(TokioIo::new(socket), service)
                        .await
                }
            };
            if let Err(e) = result {
                tracing::debug!(error = %e, "HTTP 连接结束");
            }
        });
    }
}

/// 直连模式下加载证书；nginx 前置模式返回 `None`（明文即可）。
fn load_direct_tls(
    config: &Config,
) -> Result<Option<tokio_rustls::TlsAcceptor>, crate::tls::TlsError> {
    use crate::config::HttpMode;

    if config.http.mode != HttpMode::Direct {
        return Ok(None);
    }
    // validate() 已经确认过 direct 模式下这两项都在
    let cert = config.http.cert.as_ref().expect("direct 模式必有 cert");
    let key = config.http.key.as_ref().expect("direct 模式必有 key");

    let identity = crate::tls::Identity::from_pem_files(cert, key)?;
    Ok(Some(tokio_rustls::TlsAcceptor::from(
        identity.server_config()?,
    )))
}

async fn handle(mut req: Request<Incoming>, peer: SocketAddr, ctx: Arc<Ctx>) -> Response<Body> {
    let Some(host) = request_host(&req) else {
        return error_page(
            StatusCode::BAD_REQUEST,
            "请求缺少 Host",
            "穿云按域名区分隧道，没有 Host 头就无从判断该转发到哪里。",
        );
    };

    let Some(tunnel) = ctx.registry.lookup(&host) else {
        // 找不到就是找不到——不区分"从没存在过"和"刚下线了"。
        // 两者给不同的回应，等于告诉扫描的人哪些名字是有效的。
        return error_page(
            StatusCode::NOT_FOUND,
            "这个地址上没有正在运行的隧道",
            "可能是隧道已经关闭，或者地址拼错了。如果是你自己的隧道，\
             打开穿云客户端确认它还开着。",
        );
    };

    // 隧道设了口令就先过这一关。校验放在开数据流之前——没通过的请求
    // 根本不该打扰到用户的本地服务。
    if let Some(expected) = &tunnel.auth {
        if !basic_auth_ok(&req, expected) {
            return unauthorized();
        }
    }

    let client_ip = real_client_ip(&req, peer.ip(), &ctx.config);
    set_forwarded_headers(&mut req, client_ip, &ctx.config);

    match forward(req, &tunnel, client_ip).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(%host, error = %e, "转发失败");
            error_page(
                StatusCode::BAD_GATEWAY,
                "隧道那头没有响应",
                "隧道还在，但请求没能送达。可能是客户端正在重连，或者本地服务没有启动。",
            )
        }
    }
}

async fn forward(
    mut req: Request<Incoming>,
    tunnel: &Tunnel,
    client_ip: IpAddr,
) -> anyhow::Result<Response<Body>> {
    // 每个请求单开一条流。流头告诉客户端这属于哪条隧道，读完这一行之后
    // 剩下的字节它照原样转发给本地端口，不需要理解 HTTP。
    let mut stream = tunnel.session.mux.open().await?;
    let header = cy_proto::StreamHeader::new(&tunnel.tunnel_id, tunnel.kind)
        .with_peer(client_ip.to_string());
    stream
        .write_all(format!("{}\n", header.to_line()).as_bytes())
        .await?;

    // 升级类请求（WebSocket）要在发出去之前把 upgrade future 取出来，
    // 因为 send_request 会拿走 req 的所有权。
    let wants_upgrade = req.headers().contains_key(UPGRADE);
    let downstream_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut req));

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;

    if wants_upgrade {
        tokio::spawn(async move {
            if let Err(e) = conn.with_upgrades().await {
                tracing::debug!(error = %e, "隧道连接（升级）结束");
            }
        });
    } else {
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(error = %e, "隧道连接结束");
            }
        });
    }

    let mut resp = sender.send_request(req).await?;

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        // 101 之后两边都不再是 HTTP 了，把两条升级后的连接对拷起来即可。
        // WebSocket、以及任何别的升级协议，到这一步都只是字节。
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        if let Some(downstream_upgrade) = downstream_upgrade {
            tokio::spawn(async move {
                match tokio::try_join!(downstream_upgrade, upstream_upgrade) {
                    Ok((down, up)) => {
                        let mut down = TokioIo::new(down);
                        let mut up = TokioIo::new(up);
                        if let Err(e) = tokio::io::copy_bidirectional(&mut down, &mut up).await {
                            tracing::debug!(error = %e, "升级后的连接结束");
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "协议升级失败"),
                }
            });
        }
    }

    Ok(resp.map(|b| b.boxed()))
}

/// 取请求的目标主机名（不含端口）。
///
/// HTTP/1.1 看 Host 头，HTTP/2 看 `:authority`；nginx 反代过来的是前者。
fn request_host<B>(req: &Request<B>) -> Option<String> {
    let raw = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())?;
    let host = cy_proto::naming::host_without_port(raw).trim();
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// 判断请求真正的来源 IP。
///
/// `X-Forwarded-For` 谁都能伪造，所以只有当直连对端在信任列表里（也就是前置的
/// nginx）时才采信它；否则一律用 TCP 层看到的地址。不加这道判断的话，
/// 审计日志里的来源 IP 就成了任人填写的字段。
fn real_client_ip<B>(req: &Request<B>, peer: IpAddr, config: &Config) -> IpAddr {
    if !config.http.trusted_proxies.contains(&peer) {
        return peer;
    }
    req.headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer)
}

fn set_forwarded_headers<B>(req: &mut Request<B>, client_ip: IpAddr, config: &Config) {
    let headers = req.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&client_ip.to_string()) {
        headers.insert("x-forwarded-for", v);
    }
    if let Ok(v) = HeaderValue::from_str(&config.http.public_scheme) {
        headers.insert("x-forwarded-proto", v);
    }
    // Host 保持原样：本地服务看到的是隧道域名，它据此生成的绝对地址才是对的。
    // 前端 dev server 的 allowedHosts 检查也依赖这一点。
}

/// 校验 Basic 认证头是否匹配隧道设置的口令。
///
/// 期望值是 `用户名:口令` 的明文形式——它由隧道所有者在客户端上设置、只存在于
/// 内存里，不落库也不写日志。
fn basic_auth_ok<B>(req: &Request<B>, expected: &str) -> bool {
    use base64::Engine as _;

    let Some(header) = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(encoded) = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))
    else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };

    // 常量时间比较：口令是秘密，别让比较耗时泄露前缀对了几位
    constant_time_eq(&decoded, expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 401 + 认证质询，浏览器会据此弹出输入框。
fn unauthorized() -> Response<Body> {
    let mut resp = error_page(
        StatusCode::UNAUTHORIZED,
        "需要口令",
        "这条隧道设置了访问口令。向隧道的主人要一份，或者让他关掉口令保护。",
    );
    resp.headers_mut().insert(
        hyper::header::WWW_AUTHENTICATE,
        // realm 里不要放隧道名之类的信息——这个头在没通过认证时就会发出去
        HeaderValue::from_static("Basic realm=\"chuanyun\", charset=\"UTF-8\""),
    );
    resp
}

fn error_page(status: StatusCode, title: &str, detail: &str) -> Response<Body> {
    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title} · 穿云</title>
<style>
  body {{ margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
         background:#f6f8f7; color:#1b2723;
         font-family:-apple-system,"PingFang SC","Microsoft YaHei",sans-serif; }}
  main {{ max-width:32rem; padding:2.5rem; text-align:center; }}
  h1 {{ font-size:1.35rem; font-weight:600; margin:0 0 .75rem; }}
  p {{ margin:0; color:#5c6b65; line-height:1.9; }}
  .mark {{ font-size:.75rem; letter-spacing:.18em; color:#0e7a5a; margin-bottom:1.5rem; }}
</style>
<main>
  <div class="mark">穿云</div>
  <h1>{title}</h1>
  <p>{detail}</p>
</main>
</html>"#
    );

    Response::builder()
        .status(status)
        .header("content-type", "text/html; charset=utf-8")
        // 出错页不该被 CDN 或浏览器缓存住：隧道随时可能重新开起来
        .header("cache-control", "no-store")
        .body(full(html))
        .expect("错误页是固定结构")
}

fn full(body: impl Into<Bytes>) -> Body {
    Full::new(body.into())
        .map_err(|never| match never {})
        .boxed()
}

/// 转发前要不要保留某个逐跳头。
///
/// 目前只用于测试与文档；hyper 自己会处理连接级的头，我们不手动删。
#[allow(dead_code)]
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
    ) || name.eq_ignore_ascii_case(CONNECTION.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config_with_trusted(proxies: &[&str]) -> Config {
        let mut c = Config::default();
        c.http.domain_suffix = "t.example.com".into();
        c.http.trusted_proxies = proxies.iter().map(|p| p.parse().unwrap()).collect();
        c
    }

    /// 这几个辅助函数只看 header 与 URI，所以测试里给个空 body 就够了。
    fn request(host: &str, xff: Option<&str>) -> Request<()> {
        let mut builder = Request::builder().uri("/").header(HOST, host);
        if let Some(xff) = xff {
            builder = builder.header("x-forwarded-for", xff);
        }
        builder.body(()).unwrap()
    }

    #[test]
    fn host_is_normalised() {
        assert_eq!(
            request_host(&request("ZhangSan-WX.T.Example.Com:443", None)),
            Some("zhangsan-wx.t.example.com".into())
        );
        assert_eq!(
            request_host(&request("zhangsan-wx.t.example.com", None)),
            Some("zhangsan-wx.t.example.com".into())
        );
    }

    #[test]
    fn trusted_proxy_xff_is_believed() {
        let config = config_with_trusted(&["127.0.0.1"]);
        let req = request("a.t.example.com", Some("203.0.113.7, 10.0.0.1"));
        let ip = real_client_ip(&req, "127.0.0.1".parse().unwrap(), &config);
        assert_eq!(ip.to_string(), "203.0.113.7", "应采信 nginx 传来的原始来源");
    }

    #[test]
    fn untrusted_peer_cannot_forge_its_source() {
        let config = config_with_trusted(&["127.0.0.1"]);
        // 直连进来的请求自称来自别处——不能信
        let req = request("a.t.example.com", Some("203.0.113.7"));
        let ip = real_client_ip(&req, "198.51.100.9".parse().unwrap(), &config);
        assert_eq!(
            ip.to_string(),
            "198.51.100.9",
            "非信任来源的 XFF 必须忽略，否则审计日志可以随便伪造"
        );
    }

    #[test]
    fn malformed_xff_falls_back_to_the_peer() {
        let config = config_with_trusted(&["127.0.0.1"]);
        let req = request("a.t.example.com", Some("not-an-ip"));
        let ip = real_client_ip(&req, "127.0.0.1".parse().unwrap(), &config);
        assert_eq!(ip.to_string(), "127.0.0.1");
    }

    #[test]
    fn error_page_is_not_cacheable() {
        let resp = error_page(StatusCode::NOT_FOUND, "没有隧道", "详情");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get("cache-control").unwrap(),
            "no-store",
            "隧道随时可能重开，错误页被缓存住会很难解释"
        );
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use base64::Engine as _;

    fn req_with_auth(header: Option<&str>) -> Request<()> {
        let mut b = Request::builder().uri("/").header(HOST, "a.t.example.com");
        if let Some(h) = header {
            b = b.header(hyper::header::AUTHORIZATION, h);
        }
        b.body(()).unwrap()
    }

    fn basic(creds: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(creds)
        )
    }

    #[test]
    fn correct_password_passes() {
        let req = req_with_auth(Some(&basic("demo:s3cret")));
        assert!(basic_auth_ok(&req, "demo:s3cret"));
    }

    #[test]
    fn wrong_password_is_rejected() {
        let req = req_with_auth(Some(&basic("demo:wrong")));
        assert!(!basic_auth_ok(&req, "demo:s3cret"));
    }

    #[test]
    fn missing_header_is_rejected() {
        assert!(!basic_auth_ok(&req_with_auth(None), "demo:s3cret"));
    }

    #[test]
    fn malformed_header_does_not_panic() {
        // 各种畸形输入都只该返回 false，不该炸
        for h in [
            "Basic",
            "Basic !!!not-base64!!!",
            "Bearer token",
            "",
            "Basic ",
        ] {
            assert!(
                !basic_auth_ok(&req_with_auth(Some(h)), "demo:s3cret"),
                "{h}"
            );
        }
    }

    #[test]
    fn scheme_is_case_insensitive() {
        // RFC 7235 说认证方案不区分大小写，有些客户端会发小写
        let req = req_with_auth(Some(&basic("demo:s3cret").replace("Basic", "basic")));
        assert!(basic_auth_ok(&req, "demo:s3cret"));
    }

    #[test]
    fn challenge_lets_browsers_prompt() {
        let resp = unauthorized();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let challenge = resp
            .headers()
            .get(hyper::header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(challenge.starts_with("Basic realm="), "实际：{challenge}");
        // realm 里不该泄露隧道信息——这个头在认证之前就发出去了
        assert!(!challenge.contains("zhangsan"));
    }
}
