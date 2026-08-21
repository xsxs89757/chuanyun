//! 端到端：接入同事的服务。
//!
//! 场景是真实的：李四改前端，不想在本机跑一整套后端，想直接用张三机器上那份
//! （连着他的测试数据）。验收标准是**李四的配置永远指向 127.0.0.1**——
//! 上游是谁、地址长什么样，前端代码和配置文件都不该知道。

use std::time::Duration;

use cy_core::connect::ConnectSpec;
use cy_core::{Brand, Engine, TunnelSpec};
use cy_e2e::TestServer;

fn brand(server: &TestServer) -> Brand {
    Brand {
        default_server: server.handle.control_addr.to_string(),
        tls_pin: server.handle.fingerprint.clone(),
        update_url: String::new(),
    }
}

/// 起一个"张三的后端"。
/// 拿一个当前空闲的端口号。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn spawn_backend(who: &'static str) -> u16 {
    use axum::{routing::get, Router};
    let app = Router::new()
        .route("/whoami", get(move || async move { who }))
        .route(
            "/host",
            get(|headers: axum::http::HeaderMap| async move {
                headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    p
}

/// 完整链路：李四本地端口 → 张三的隧道 → 张三的机器。
///
/// 这是这个功能的核心承诺——李四的代码只跟 127.0.0.1 打交道。
#[tokio::test]
async fn a_colleagues_service_appears_on_a_local_port() {
    let server = TestServer::start().await;
    let zhangsan_token = server.add_user("zhangsan").await;
    let backend = spawn_backend("我是张三的后端").await;

    // 测试环境里没有 DNS，没法让 zhangsan-api.t.example.com 解析到本机。
    // 所以给张三登记一个"自定义域名" 127.0.0.1——这条路径本来就支持
    // （FR-S11），正好让上游地址既能连通又能被 ingress 认出来。
    server
        .store()
        .add_custom_domain("zhangsan", "127.0.0.1")
        .await
        .unwrap();

    let zhangsan = Engine::start(None, brand(&server));
    zhangsan
        .login(
            server.handle.control_addr.to_string(),
            &zhangsan_token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();

    // 直连一层，用自定义域名开隧道
    let (events, _rx) = tokio::sync::broadcast::channel(16);
    let conn = cy_core::connect(
        &server.client_config(&zhangsan_token),
        events,
        Default::default(),
    )
    .await
    .unwrap();
    conn.open_tunnel(TunnelSpec::http("api", backend).with_domain("127.0.0.1"))
        .await
        .expect("用已登记的自定义域名开隧道");

    // 李四接入：上游就是 ingress 的地址，Host 会被改写成 127.0.0.1，
    // 正好命中张三登记的那个自定义域名
    let lisi = Engine::start(None, brand(&server));
    let local_port = free_port();
    lisi.add_connect(ConnectSpec::new(
        local_port,
        format!("http://{}", server.handle.http_addr),
    ))
    .await
    .expect("接入应当起得来");

    // 李四的代码只知道 127.0.0.1——它不需要知道上游是谁
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let who = client
        .get(format!("http://127.0.0.1:{local_port}/whoami"))
        .send()
        .await
        .expect("经本地端口应当能访问到张三的服务")
        .text()
        .await
        .unwrap();
    assert_eq!(who, "我是张三的后端");
}

/// 自定义域名必须先登记——否则谁都能声称自己是 pay.example.com。
#[tokio::test]
async fn unregistered_custom_domain_is_refused() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;

    let (events, _rx) = tokio::sync::broadcast::channel(16);
    let conn = cy_core::connect(&server.client_config(&token), events, Default::default())
        .await
        .unwrap();

    let err = conn
        .open_tunnel(TunnelSpec::http("api", 8080).with_domain("pay.example.com"))
        .await
        .expect_err("没登记过的域名不该给用");
    assert!(err.contains("占用") || err.contains("保留"), "实际：{err}");
}

/// 别人登记的域名，我也不能用。
#[tokio::test]
async fn someone_elses_custom_domain_is_refused() {
    let server = TestServer::start().await;
    let zhangsan_token = server.add_user("zhangsan").await;
    server.add_user("lisi").await;
    server
        .store()
        .add_custom_domain("lisi", "pay.example.com")
        .await
        .unwrap();

    let (events, _rx) = tokio::sync::broadcast::channel(16);
    let conn = cy_core::connect(
        &server.client_config(&zhangsan_token),
        events,
        Default::default(),
    )
    .await
    .unwrap();

    assert!(
        conn.open_tunnel(TunnelSpec::http("api", 8080).with_domain("pay.example.com"))
            .await
            .is_err(),
        "别人登记的域名不该给用"
    );
}

/// 短写法要靠登录后拿到的域名后缀补全；还没登录就该说清楚。
#[tokio::test]
async fn short_form_requires_knowing_the_domain_suffix() {
    let server = TestServer::start().await;
    let engine = Engine::start(None, brand(&server));

    let err = engine
        .add_connect(ConnectSpec::new(free_port(), "zhangsan-api"))
        .await
        .expect_err("还不知道后缀，应当失败");
    assert!(err.contains("先登录"), "实际：{err}");

    // 登录之后同样的写法就能用了
    let token = server.add_user("lisi").await;
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();

    let upstream = engine
        .add_connect(ConnectSpec::new(free_port(), "zhangsan-api"))
        .await
        .expect("登录后短写法应当可用");
    assert_eq!(upstream, "https://zhangsan-api.t.example.com");
}

/// 本地端口被自己占着时，接入要给一条能照做的错误。
///
/// 这个场景很常见：你自己也把 api 跑起来了。此时"自己跑的优先"是合理的默认，
/// 但必须说清楚，不能默默失败。
#[tokio::test]
async fn port_conflict_is_explained() {
    let server = TestServer::start().await;
    let engine = Engine::start(None, brand(&server));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let err = engine
        .add_connect(ConnectSpec::new(port, "https://zhangsan-api.t.example.com"))
        .await
        .expect_err("端口被占应当失败");
    assert!(err.contains("已被占用"), "实际：{err}");
    assert!(err.contains("停掉它"), "该告诉用户怎么办：{err}");

    // 状态里也要留下失败原因，界面才能显示
    let status = engine.status();
    assert!(!status.connects[0].running);
    assert!(status.connects[0].error.is_some());
}

/// 同一个端口重复接入，后来的替换先前的——否则旧的占着端口，新的永远起不来。
#[tokio::test]
async fn re_adding_the_same_port_replaces_the_old_one() {
    let server = TestServer::start().await;
    let token = server.add_user("lisi").await;
    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();

    let port = free_port();

    engine
        .add_connect(ConnectSpec::new(port, "zhangsan-api"))
        .await
        .unwrap();
    // 换个上游，同一个端口
    engine
        .add_connect(ConnectSpec::new(port, "lisi-api"))
        .await
        .expect("替换应当成功，而不是报端口被占");

    let status = engine.status();
    assert_eq!(status.connects.len(), 1, "同一个端口只该有一条接入");
    assert!(status.connects[0].upstream.contains("lisi-api"));
}

/// 移除接入后端口要放开。
#[tokio::test]
async fn removing_a_connect_frees_the_port() {
    let server = TestServer::start().await;
    let token = server.add_user("lisi").await;
    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();

    let port = free_port();

    engine
        .add_connect(ConnectSpec::new(port, "zhangsan-api"))
        .await
        .unwrap();
    engine.remove_connect(port).await;
    assert!(engine.status().connects.is_empty());

    // 端口应该能被别人绑上了
    cy_e2e::wait_for("端口放开", Duration::from_secs(5), || {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    })
    .await;
}

/// 本地 API 也能管理接入。
#[tokio::test]
async fn local_api_manages_connects() {
    let server = TestServer::start().await;
    let token = server.add_user("lisi").await;
    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();

    let api_port = free_port();
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            let _ = cy_core::local_api::serve(engine, api_port).await;
        });
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let target_port = free_port();

    let api = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{api_port}");

    let created: serde_json::Value = api
        .post(format!("{base}/api/connects"))
        .json(&serde_json::json!({"local_port": target_port, "from": "zhangsan-api"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["ok"], true);
    assert_eq!(created["upstream"], "https://zhangsan-api.t.example.com");

    let list: serde_json::Value = api
        .get(format!("{base}/api/connects"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["from"], "zhangsan-api");

    api.delete(format!("{base}/api/connects/{target_port}"))
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = api
        .get(format!("{base}/api/connects"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 0);
}
