//! 端到端：真服务端 + 真客户端，跑通「开隧道 → 数据流 → 本地服务」这条链。

use std::time::Duration;

use cy_core::{Event, TunnelSpec};
use cy_e2e::{spawn_echo_server, TestServer};
use cy_proto::{StreamHeader, TunnelKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 最小可信链路：客户端连上、开隧道、服务端按主机名找到它、
/// 从注册表里开一条数据流，字节一路送到本地服务再回来。
///
/// 这里刻意绕开 HTTP 入口（那是下一阶段的事），直接用注册表 + mux——
/// 先证明底层管道是通的，再往上叠 HTTP。
#[tokio::test]
async fn bytes_flow_all_the_way_to_the_local_service() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo_port = spawn_echo_server().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .expect("客户端应能连上");

    let url = conn
        .open_tunnel(TunnelSpec::http("wx", echo_port))
        .await
        .expect("开隧道应成功");
    assert_eq!(url, "https://zhangsan-wx.t.example.com");

    // 服务端这边应该已经能按主机名路由到这条隧道了
    let tunnel = server
        .handle
        .registry
        .lookup("zhangsan-wx.t.example.com")
        .expect("注册表里应有这条隧道");
    assert_eq!(tunnel.session.user, "zhangsan");

    // 走一遍数据面：开流 → 写流头 → 发字节 → 收回声
    let mut stream = tunnel.session.mux.open().await.expect("开数据流");
    let header = StreamHeader::new(&tunnel.tunnel_id, TunnelKind::Http).with_peer("1.2.3.4");
    stream
        .write_all(format!("{}\n", header.to_line()).as_bytes())
        .await
        .unwrap();

    stream.write_all(b"hello chuanyun").await.unwrap();
    stream.flush().await.unwrap();

    let mut buf = [0u8; 14];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .expect("等回声超时")
        .expect("读回声");
    assert_eq!(&buf, b"hello chuanyun");
}

/// 隧道名不合法、重名、超限——服务端要给出可辨认的错误，而不是让客户端干等。
#[tokio::test]
async fn duplicate_tunnel_name_is_rejected() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo_port = spawn_echo_server().await;
    let (events, _rx) = tokio::sync::broadcast::channel(64);

    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("wx", echo_port))
        .await
        .unwrap();

    let err = conn
        .open_tunnel(TunnelSpec::http("wx", echo_port))
        .await
        .expect_err("同名隧道应被拒绝");
    assert!(err.contains("占用"), "错误该说人话，实际是：{err}");
}

#[tokio::test]
async fn invalid_tunnel_name_is_rejected() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let (events, _rx) = tokio::sync::broadcast::channel(64);

    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();
    let err = conn
        .open_tunnel(TunnelSpec::http("Bad_Name", 8080))
        .await
        .expect_err("非法名称应被拒绝");
    assert!(err.contains("小写字母"), "错误该说人话，实际是：{err}");
}

/// 凭证被吊销之后再连就该连不上——而且要说清楚是为什么。
#[tokio::test]
async fn revoked_credentials_are_rejected_with_a_clear_reason() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    server.store().revoke_user("zhangsan").await.unwrap();

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let err = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .expect_err("吊销后不该连得上");
    assert!(err.to_string().contains("吊销"), "实际错误：{err}");
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let server = TestServer::start().await;
    server.add_user("zhangsan").await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let err = cy_core::connect(
        &server.client_config("cy_zhangsan_deadbeef"),
        events,
        Default::default(),
    )
    .await
    .expect_err("错误凭证不该连得上");
    assert!(err.to_string().contains("凭证"), "实际错误：{err}");
}

/// 指纹对不上就该连不上——这是防中间人的最后一道闸。
#[tokio::test]
async fn wrong_fingerprint_is_rejected() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;

    let mut config = server.client_config(&token);
    config.verify = cy_core::Verify::Pin("00".repeat(32));

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let err = cy_core::connect(&config, events, Default::default())
        .await
        .expect_err("指纹不符不该连得上");
    assert!(err.to_string().contains("指纹"), "实际错误：{err}");
}

/// 管理员踢人：连接立刻断，而且客户端知道自己是被踢的（不该傻乎乎地重连）。
#[tokio::test]
async fn kicked_client_learns_why() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo_port = spawn_echo_server().await;

    let (events, mut rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("wx", echo_port))
        .await
        .unwrap();

    assert_eq!(server.handle.registry.kick_user("zhangsan"), 1);

    let kicked = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(Event::Kicked { reason }) => return reason,
                Ok(_) => continue,
                Err(e) => panic!("事件流断了: {e}"),
            }
        }
    })
    .await
    .expect("应当收到被踢事件");
    assert!(!kicked.is_empty());

    // 隧道也该跟着下线
    cy_e2e::wait_for("隧道从注册表移除", Duration::from_secs(5), || {
        server
            .handle
            .registry
            .lookup("zhangsan-wx.t.example.com")
            .is_none()
    })
    .await;
}

/// 客户端断开后，它开的隧道不能留在注册表里当"幽灵路由"。
#[tokio::test]
async fn tunnels_disappear_when_the_client_goes_away() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo_port = spawn_echo_server().await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("wx", echo_port))
        .await
        .unwrap();
    assert_eq!(server.handle.registry.tunnel_count(), 1);

    conn.disconnect();
    drop(conn);

    cy_e2e::wait_for("会话与隧道被清理", Duration::from_secs(5), || {
        server.handle.registry.tunnel_count() == 0 && server.handle.registry.session_count() == 0
    })
    .await;
}

/// 本地服务没启动时，转发要干净地失败，不能把客户端拖死。
#[tokio::test]
async fn missing_local_service_fails_cleanly() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;

    // 占一个端口再立刻释放，拿到一个大概率没人监听的端口号
    let dead_port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();
    conn.open_tunnel(TunnelSpec::http("dead", dead_port))
        .await
        .unwrap();

    let tunnel = server
        .handle
        .registry
        .lookup("zhangsan-dead.t.example.com")
        .unwrap();
    let mut stream = tunnel.session.mux.open().await.unwrap();
    let header = StreamHeader::new(&tunnel.tunnel_id, TunnelKind::Http);
    stream
        .write_all(format!("{}\n", header.to_line()).as_bytes())
        .await
        .unwrap();
    stream.write_all(b"anyone home?").await.unwrap();

    // 客户端应当关掉这条流，而不是挂着
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    assert!(
        read.is_ok(),
        "本地服务不可用时，数据流应当被关掉而不是一直挂着"
    );

    // 而且连接本身还活着，别的隧道不受影响
    assert!(conn.is_alive());
}

/// 心跳：客户端要按时回 pong，否则会被服务端判死。
/// 测试里心跳设成 1 秒，这里等几个周期看连接还在不在。
#[tokio::test]
async fn connection_survives_several_heartbeats() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;

    let (events, _rx) = tokio::sync::broadcast::channel(64);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(conn.is_alive(), "心跳应答正常的话连接不该断");
    assert_eq!(server.handle.registry.session_count(), 1);
}

/// 多个客户端各开各的隧道，互不干扰。
#[tokio::test]
async fn multiple_clients_coexist() {
    let server = TestServer::start().await;
    let a_token = server.add_user("zhangsan").await;
    let b_token = server.add_user("lisi").await;
    let echo_port = spawn_echo_server().await;

    let (ev_a, _ra) = tokio::sync::broadcast::channel(64);
    let (ev_b, _rb) = tokio::sync::broadcast::channel(64);
    let a = cy_core::connect(&server.client_config(&a_token), ev_a, Default::default())
        .await
        .unwrap();
    let b = cy_core::connect(&server.client_config(&b_token), ev_b, Default::default())
        .await
        .unwrap();

    // 两个人都叫 api，但域名带各自的用户名前缀，不会撞
    let a_url = a
        .open_tunnel(TunnelSpec::http("api", echo_port))
        .await
        .unwrap();
    let b_url = b
        .open_tunnel(TunnelSpec::http("api", echo_port))
        .await
        .unwrap();
    assert_eq!(a_url, "https://zhangsan-api.t.example.com");
    assert_eq!(b_url, "https://lisi-api.t.example.com");
    assert_eq!(server.handle.registry.tunnel_count(), 2);
}
