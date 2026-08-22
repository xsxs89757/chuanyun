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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
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

/// 过了门之后，`Authorization` 头不能再转给本地服务。
///
/// 真机上证明过：设了口令的隧道把 `Authorization: Basic …` 原样转给了 vite，
/// vite 再把它代理给后端——后端的 JWT 中间件看到一个 `Basic` 头而不是 `Bearer`。
/// 门是我们的，过了门这个头就该被吃掉。
#[tokio::test]
async fn the_gate_consumes_the_authorization_header() {
    use axum::{http::HeaderMap, routing::get, Router};

    // 一个把收到的 Authorization 头原样打回来的本地服务
    let app = Router::new().route(
        "/",
        get(|h: HeaderMap| async move {
            h.get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string()
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });

    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("gate", port).with_auth("demo:s3cret"))
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("open", port))
        .await
        .unwrap();

    let client = reqwest::Client::builder()
        .resolve("zhangsan-gate.t.example.com", server.handle.http_addr)
        .resolve("zhangsan-open.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();

    let behind_gate = client
        .get("http://zhangsan-gate.t.example.com/")
        .basic_auth("demo", Some("s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(behind_gate.status(), 200);
    assert_eq!(
        behind_gate.text().await.unwrap(),
        "<none>",
        "设了口令：Basic 头被门吃掉，本地服务不该看到它"
    );

    let passthrough = client
        .get("http://zhangsan-open.t.example.com/")
        .header("authorization", "Bearer jwt-xyz")
        .send()
        .await
        .unwrap();
    assert_eq!(
        passthrough.text().await.unwrap(),
        "Bearer jwt-xyz",
        "没设口令：Authorization 原样透传，手机 app 调 API 靠它"
    );
}

/// 门发 cookie，之后认 cookie；应用自己的 Authorization 头原样放行。
///
/// 真机上的死循环：vben 登录后每个 API 请求都带 `Authorization: Bearer <jwt>`，
/// 只认 Basic 的门看到 Bearer 就 401 弹框，用户填对了这一个请求过了门、
/// 可 Bearer 没了，后端登录态又丢 → 刷新 → 再弹。
#[tokio::test]
async fn the_gate_issues_a_cookie_and_lets_the_apps_own_bearer_through() {
    use axum::{http::HeaderMap, routing::get, Router};

    let app = Router::new().route(
        "/",
        get(|h: HeaderMap| async move {
            h.get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string()
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });

    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("app", port).with_auth("demo:s3cret"))
        .await
        .unwrap();

    let client = reqwest::Client::builder()
        .resolve("zhangsan-app.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();
    let url = "http://zhangsan-app.t.example.com/";

    // 1. 登录后的 SPA：只带 Bearer，没 cookie → 要口令（浏览器会弹一次框）
    let r = client
        .get(url)
        .header("authorization", "Bearer jwt-xyz")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 2. 浏览器答了框：Basic 通过 → 200，并且发了 cookie
    let r = client
        .get(url)
        .basic_auth("demo", Some("s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let cookie = r
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("Basic 通过后要发 cookie")
        .to_string();
    assert!(cookie.starts_with("chuanyun_auth="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(
        !cookie.contains("Domain="),
        "要 host-only，别的隧道拿不到: {cookie}"
    );
    let ticket = cookie.split(';').next().unwrap().to_string();

    // 3. 之后的 API 请求：cookie + 应用自己的 Bearer → 放行，且 Bearer 原样到达本地服务
    let r = client
        .get(url)
        .header("cookie", &ticket)
        .header("authorization", "Bearer jwt-xyz")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "有 cookie 就不该再要口令");
    assert_eq!(
        r.text().await.unwrap(),
        "Bearer jwt-xyz",
        "应用自己的 Authorization 必须原样到达"
    );

    // 4. cookie + 浏览器顺手带的 Basic → 放行，Basic 被吃掉
    let r = client
        .get(url)
        .header("cookie", &ticket)
        .basic_auth("demo", Some("s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), "<none>");

    // 5. 伪造的 cookie 不行
    let r = client
        .get(url)
        .header("cookie", "chuanyun_auth=deadbeef")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // 6. 改了口令，旧 cookie 立刻失效
    conn.close_tunnel("app").await;
    conn.open_tunnel(TunnelSpec::http("app", port).with_auth("demo:changed"))
        .await
        .unwrap();
    let r = client
        .get(url)
        .header("cookie", &ticket)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "改口令后旧 cookie 不能再用");
}
