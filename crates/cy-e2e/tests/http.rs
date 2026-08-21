//! 端到端：外部 HTTP 请求经入口、隧道，一路打到本地服务再原路返回。
//!
//! 断言用的 HTTP 客户端把隧道域名钉到入口的真实端口（`reqwest` 的 `resolve`），
//! 这样 Host 头保持真实语义、路由逻辑被真正走到——比手工塞一个 Host 头干净得多。

use std::time::Duration;

use cy_core::TunnelSpec;
use cy_e2e::TestServer;
use futures::StreamExt;

/// 起一个假的"本地服务"，把常见的几种响应形态都覆盖到。
async fn spawn_local_app() -> u16 {
    use axum::response::sse::{Event as SseEvent, Sse};
    use axum::routing::{get, post};
    use axum::Router;

    let app = Router::new()
        .route("/hello", get(|| async { "你好，穿云" }))
        // 把收到的 Host 原样回显——用来验证本地服务看到的是隧道域名而不是 127.0.0.1
        .route(
            "/host",
            get(|headers: axum::http::HeaderMap| async move {
                headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            }),
        )
        .route(
            "/client-ip",
            get(|headers: axum::http::HeaderMap| async move {
                headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            }),
        )
        .route("/echo", post(|body: axum::body::Bytes| async move { body }))
        // 吞掉整个请求体，只回报长度——用来验证大文件是流式转发的
        .route(
            "/upload",
            post(|body: axum::body::Body| async move {
                use futures::StreamExt;
                let mut total = 0usize;
                let mut stream = body.into_data_stream();
                while let Some(Ok(chunk)) = stream.next().await {
                    total += chunk.len();
                }
                total.to_string()
            }),
        )
        .route(
            "/sse",
            get(|| async {
                let stream = futures::stream::iter((1..=3).map(|i| {
                    Ok::<_, std::convert::Infallible>(SseEvent::default().data(format!("tick-{i}")))
                }));
                Sse::new(stream)
            }),
        )
        .route("/ws", get(ws_handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

async fn ws_handler(ws: axum::extract::WebSocketUpgrade) -> axum::response::Response {
    ws.on_upgrade(|mut socket| async move {
        use axum::extract::ws::Message;
        while let Some(Ok(msg)) = socket.recv().await {
            if let Message::Text(t) = msg {
                if socket.send(Message::Text(t)).await.is_err() {
                    break;
                }
            }
        }
    })
}

/// 起服务端 + 客户端 + 本地服务，返回一个已经把域名钉好的 HTTP 客户端。
async fn setup() -> (TestServer, cy_core::Connection, reqwest::Client, String) {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let local_port = spawn_local_app().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .expect("客户端连接");
    let url = conn
        .open_tunnel(TunnelSpec::http("app", local_port))
        .await
        .expect("开隧道");

    let host = url.trim_start_matches("https://").to_string();
    let client = reqwest::Client::builder()
        .resolve(&host, server.handle.http_addr)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    // 入口是明文 HTTP（TLS 由前置 nginx 终止），所以测试打 http://
    let base = format!("http://{host}");
    (server, conn, client, base)
}

#[tokio::test]
async fn plain_get_round_trips() {
    let (_server, _conn, client, base) = setup().await;

    let resp = client.get(format!("{base}/hello")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "你好，穿云");
}

/// 本地服务应该看到隧道域名，而不是 127.0.0.1。
///
/// 这不是细节：应用据此生成绝对地址（微信回调、OAuth 跳转都要用），
/// 前端 dev server 的 allowedHosts 检查也依赖它。
#[tokio::test]
async fn local_service_sees_the_tunnel_hostname() {
    let (_server, _conn, client, base) = setup().await;

    let host = client
        .get(format!("{base}/host"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(host, "zhangsan-app.t.example.com");
}

#[tokio::test]
async fn client_ip_is_forwarded() {
    let (_server, _conn, client, base) = setup().await;

    let ip = client
        .get(format!("{base}/client-ip"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!ip.is_empty(), "本地服务应能拿到来源 IP");
    assert!(
        ip.parse::<std::net::IpAddr>().is_ok(),
        "应是个合法 IP：{ip}"
    );
}

#[tokio::test]
async fn post_body_round_trips() {
    let (_server, _conn, client, base) = setup().await;

    let payload = "微信回调的 XML 报文".repeat(100);
    let echoed = client
        .post(format!("{base}/echo"))
        .body(payload.clone())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(echoed, payload);
}

/// 大文件：50MB 流式上传。
///
/// 这条测试守的是"全程流式、不在中间落盘或全量缓冲"——真缓冲的话，
/// 内存会顶不住，而且大文件调试场景（客户演示传视频）会直接不可用。
#[tokio::test]
async fn large_streaming_upload() {
    let (_server, _conn, client, base) = setup().await;

    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 800; // 50 MiB
    let stream = futures::stream::iter(
        (0..CHUNKS).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from_static(&[b'x'; CHUNK]))),
    );

    let total = client
        .post(format!("{base}/upload"))
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(
        total.parse::<usize>().unwrap(),
        CHUNK * CHUNKS,
        "上传的字节数对不上"
    );
}

/// SSE：服务端推送要能实时穿过隧道。
#[tokio::test]
async fn server_sent_events_stream_through() {
    let (_server, _conn, client, base) = setup().await;

    let resp = client.get(format!("{base}/sse")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));

    let mut body = resp.bytes_stream();
    let mut text = String::new();
    while let Some(Ok(chunk)) = body.next().await {
        text.push_str(&String::from_utf8_lossy(&chunk));
        if text.contains("tick-3") {
            break;
        }
    }
    assert!(
        text.contains("tick-1") && text.contains("tick-3"),
        "收到的是：{text}"
    );
}

/// WebSocket：升级之后两边就只是字节，隧道要能原样对拷。
#[tokio::test]
async fn websocket_upgrade_and_echo() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let local_port = spawn_local_app().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("app", local_port))
        .await
        .unwrap();

    // tokio-tungstenite 不认 resolve，所以直接连入口地址，用 Host 头指明目标隧道
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{}/ws", server.handle.http_addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("host", "zhangsan-app.t.example.com".parse().unwrap());

    let (mut ws, resp) = tokio_tungstenite::connect_async(request)
        .await
        .expect("WebSocket 握手应当成功");
    assert_eq!(resp.status(), 101);

    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    ws.send(Message::Text("ping through the tunnel".into()))
        .await
        .unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("等回声超时")
        .expect("连接断了")
        .expect("读消息");
    assert_eq!(reply.into_text().unwrap(), "ping through the tunnel");
}

/// 没有隧道的域名要给出友好错误页，而且不能区分"没开过"和"刚关掉"。
#[tokio::test]
async fn unknown_host_gets_a_friendly_page() {
    let server = TestServer::start().await;
    let client = reqwest::Client::builder()
        .resolve("nobody-here.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();

    let resp = client
        .get("http://nobody-here.t.example.com/")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");

    let body = resp.text().await.unwrap();
    assert!(body.contains("穿云"), "该是品牌错误页");
    assert!(body.contains("没有正在运行的隧道"));
}

/// 隧道关掉之后，同一个地址立刻回到"查无此隧道"。
#[tokio::test]
async fn closing_a_tunnel_takes_the_route_down() {
    let (_server, conn, client, base) = setup().await;

    assert_eq!(
        client
            .get(format!("{base}/hello"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    conn.close_tunnel("app").await;
    cy_e2e::wait_for("路由撤下", Duration::from_secs(5), || true).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = client.get(format!("{base}/hello")).send().await.unwrap();
    assert_eq!(resp.status(), 404, "隧道关了之后不该还能访问");
}

/// 多条隧道并存时，各走各的路由，不能串线。
///
/// 这条守的是"按请求路由"这个决定：nginx 开 upstream keepalive 后，
/// 同一条连接上会混载不同子域名的请求，按连接路由会把请求送错人。
#[tokio::test]
async fn concurrent_tunnels_do_not_cross_talk() {
    let server = TestServer::start().await;
    let a_token = server.add_user("zhangsan").await;
    let b_token = server.add_user("lisi").await;

    // 两个内容不同的本地服务
    let make_app = |text: &'static str| async move {
        use axum::{routing::get, Router};
        let app = Router::new().route("/who", get(move || async move { text }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        p
    };
    let a_port = make_app("我是张三的服务").await;
    let b_port = make_app("我是李四的服务").await;

    let (ev_a, _ra) = tokio::sync::broadcast::channel(64);
    let (ev_b, _rb) = tokio::sync::broadcast::channel(64);
    let a = cy_core::connect(&server.client_config(&a_token), ev_a, Default::default())
        .await
        .unwrap();
    let b = cy_core::connect(&server.client_config(&b_token), ev_b, Default::default())
        .await
        .unwrap();
    a.open_tunnel(TunnelSpec::http("app", a_port))
        .await
        .unwrap();
    b.open_tunnel(TunnelSpec::http("app", b_port))
        .await
        .unwrap();

    // 同一个 reqwest 客户端（会复用连接）交替打两个域名
    let client = reqwest::Client::builder()
        .resolve("zhangsan-app.t.example.com", server.handle.http_addr)
        .resolve("lisi-app.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();

    for _ in 0..5 {
        let a_text = client
            .get("http://zhangsan-app.t.example.com/who")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let b_text = client
            .get("http://lisi-app.t.example.com/who")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(a_text, "我是张三的服务");
        assert_eq!(b_text, "我是李四的服务");
    }
}
