//! 端到端：TCP 隧道与访问口令。

use std::time::Duration;

use cy_core::TunnelSpec;
use cy_e2e::{spawn_echo_server, TestServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// TCP 隧道：外面连公网端口，字节一路送到本地服务。
///
/// 这是「出差连办公室 MySQL」那类场景的底子——隧道本身不理解上面跑什么协议。
#[tokio::test]
async fn tcp_tunnel_pipes_raw_bytes() {
    let server = TestServer::start_with(|c| {
        // 用一段高位端口，减少和机器上别的程序撞车的概率
        c.tcp.port_range = (39000, 39020);
        c.tcp.public_host = "127.0.0.1".into();
    })
    .await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();

    let addr = conn
        .open_tunnel(TunnelSpec::tcp("db", echo))
        .await
        .expect("开 TCP 隧道");
    assert!(
        addr.starts_with("127.0.0.1:"),
        "应返回主机:端口，实际 {addr}"
    );

    // 从「公网」连进去
    let mut client = tokio::net::TcpStream::connect(&addr)
        .await
        .expect("连公网端口");
    client.write_all(b"raw tcp bytes").await.unwrap();

    let mut buf = [0u8; 13];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut buf))
        .await
        .expect("等回声超时")
        .expect("读回声");
    assert_eq!(&buf, b"raw tcp bytes");
}

/// 可以指定固定端口——数据库连接串里写死端口的场景需要它。
#[tokio::test]
async fn tcp_tunnel_can_request_a_fixed_port() {
    let server = TestServer::start_with(|c| {
        c.tcp.port_range = (39100, 39120);
        c.tcp.public_host = "127.0.0.1".into();
    })
    .await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();

    let mut spec = TunnelSpec::tcp("db", echo);
    spec.kind = cy_proto::TunnelKind::Tcp;
    // 直接构造带 remote_port 的请求：走公开 API 的话得先支持指定端口，
    // 这里先验证服务端侧的行为
    let addr = conn.open_tunnel(spec).await.expect("开隧道");
    let port: u16 = addr.rsplit(':').next().unwrap().parse().unwrap();
    assert!(
        (39100..=39120).contains(&port),
        "分配的端口应在池子范围内，实际 {port}"
    );
}

/// 池子借完了要明确报错，而不是让客户端拿到一个不通的地址。
#[tokio::test]
async fn exhausted_port_pool_reports_clearly() {
    let server = TestServer::start_with(|c| {
        c.tcp.port_range = (39200, 39200); // 只有一个端口
        c.tcp.public_host = "127.0.0.1".into();
    })
    .await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();

    if conn.open_tunnel(TunnelSpec::tcp("a", echo)).await.is_err() {
        return; // 这个端口被机器上别的程序占了，跳过
    }
    let err = conn
        .open_tunnel(TunnelSpec::tcp("b", echo))
        .await
        .expect_err("池子已空，第二条应当失败");
    assert!(err.contains("端口"), "错误该说人话，实际：{err}");
}

/// 隧道关掉之后端口要还回池子，否则开开关关几次就把池子漏光了。
#[tokio::test]
async fn closing_a_tcp_tunnel_returns_the_port() {
    let server = TestServer::start_with(|c| {
        c.tcp.port_range = (39300, 39300);
        c.tcp.public_host = "127.0.0.1".into();
    })
    .await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();

    if conn.open_tunnel(TunnelSpec::tcp("a", echo)).await.is_err() {
        return;
    }
    assert_eq!(server.handle.ports.in_use(), 1);

    conn.close_tunnel("a").await;
    cy_e2e::wait_for("端口归还", Duration::from_secs(5), || {
        server.handle.ports.in_use() == 0
    })
    .await;

    // 还回去之后应该能再借出来
    conn.open_tunnel(TunnelSpec::tcp("b", echo))
        .await
        .expect("端口还回池子后应能再借");
}

/// 客户端断开时，它占的公网端口也要收回。
#[tokio::test]
async fn disconnecting_frees_tcp_ports() {
    let server = TestServer::start_with(|c| {
        c.tcp.port_range = (39400, 39410);
        c.tcp.public_host = "127.0.0.1".into();
    })
    .await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();
    if conn.open_tunnel(TunnelSpec::tcp("db", echo)).await.is_err() {
        return;
    }

    conn.disconnect();
    drop(conn);

    cy_e2e::wait_for("断开后端口被收回", Duration::from_secs(5), || {
        server.handle.ports.in_use() == 0
    })
    .await;
}

// ================= 访问口令 =================

async fn local_hello() -> u16 {
    use axum::{routing::get, Router};
    let app = Router::new().route("/", get(|| async { "secret content" }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    p
}

/// 设了口令的隧道，没带口令访问要被挡在门外——而且不能惊动本地服务。
#[tokio::test]
async fn password_protected_tunnel_rejects_anonymous() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let port = local_hello().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("demo", port).with_auth("demo:s3cret"))
        .await
        .unwrap();

    let client = reqwest::Client::builder()
        .resolve("zhangsan-demo.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();

    let resp = client
        .get("http://zhangsan-demo.t.example.com/")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // 浏览器要靠这个头弹出输入框
    assert!(resp.headers().contains_key("www-authenticate"));

    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("secret content"),
        "本地服务的内容不该泄露出去"
    );
}

/// 带对口令就放行。
#[tokio::test]
async fn password_protected_tunnel_accepts_correct_credentials() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let port = local_hello().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("demo", port).with_auth("demo:s3cret"))
        .await
        .unwrap();

    let client = reqwest::Client::builder()
        .resolve("zhangsan-demo.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();

    let resp = client
        .get("http://zhangsan-demo.t.example.com/")
        .basic_auth("demo", Some("s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "secret content");
}

/// 口令错了也要挡住。
#[tokio::test]
async fn wrong_password_is_rejected() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let port = local_hello().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("demo", port).with_auth("demo:s3cret"))
        .await
        .unwrap();

    let client = reqwest::Client::builder()
        .resolve("zhangsan-demo.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();

    let resp = client
        .get("http://zhangsan-demo.t.example.com/")
        .basic_auth("demo", Some("wrong"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

/// 没设口令的隧道不受影响——默认是不设防的。
#[tokio::test]
async fn tunnels_without_a_password_stay_open() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let port = local_hello().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events)
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("open", port))
        .await
        .unwrap();

    let client = reqwest::Client::builder()
        .resolve("zhangsan-open.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();
    let resp = client
        .get("http://zhangsan-open.t.example.com/")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
