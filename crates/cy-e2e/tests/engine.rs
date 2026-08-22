//! 端到端：引擎层的行为——自动重连、状态恢复、本地 API。
//!
//! 这一层守的是「用户不需要做任何事」：服务端重启、笔记本合盖、网络抖动之后，
//! 隧道该自己回来，而不是要用户重新点一次开关。

use std::time::Duration;

use cy_core::{Brand, Engine};
use cy_e2e::{spawn_echo_server, wait_for, TestServer};

fn brand(server: &TestServer) -> Brand {
    Brand {
        default_server: server.handle.control_addr.to_string(),
        tls_pin: server.handle.fingerprint.clone(),
        update_url: String::new(),
    }
}

#[tokio::test]
async fn engine_connects_and_opens_tunnels() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");

    engine.add_tunnel("wx", echo).await.expect("开隧道");

    let status = engine.status();
    assert!(status.connected);
    assert_eq!(status.domain_suffix, "t.example.com");
    let tunnel = status.tunnel("wx").expect("状态里应有这条隧道");
    assert_eq!(
        tunnel.url.as_deref(),
        Some("https://zhangsan-wx.t.example.com")
    );
}

/// 服务端重启之后，客户端要自己回来，隧道也要自己重开。
///
/// 这是整个引擎最重要的一条：没有它，每次服务端更新都要通知所有同事
/// 手动重连一次。
#[tokio::test]
async fn tunnels_come_back_after_the_server_restarts() {
    let data_dir = tempfile::tempdir().unwrap();
    let echo = spawn_echo_server().await;

    // 固定端口起服务端，好让重启后客户端能连回同一个地址
    let control_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };

    let make_server = || {
        let dir = data_dir.path().to_path_buf();
        async move {
            let mut config = cy_server::Config {
                storage: cy_server::config::StorageConfig { data_dir: dir },
                ..Default::default()
            };
            config.http.domain_suffix = "t.example.com".into();
            config.control.listen = format!("127.0.0.1:{control_port}").parse().unwrap();
            config.http.listen = "127.0.0.1:0".parse().unwrap();
            config.admin.listen = "127.0.0.1:0".parse().unwrap();
            config.control.heartbeat_secs = 1;
            cy_server::Server::start(config).await.expect("启动服务端")
        }
    };

    let first = make_server().await;
    let token = first.store.add_user("zhangsan", None, 10).await.unwrap();
    let fingerprint = first.fingerprint.clone();

    let engine = Engine::start(
        None,
        Brand {
            default_server: format!("127.0.0.1:{control_port}"),
            tls_pin: fingerprint.clone(),
            update_url: String::new(),
        },
    );
    engine
        .login(format!("127.0.0.1:{control_port}"), &token, &fingerprint)
        .await
        .expect("登录");
    engine.add_tunnel("wx", echo).await.expect("开隧道");
    assert!(engine.status().connected);

    // 服务端下线
    first.shutdown().await;
    wait_for("引擎察觉掉线", Duration::from_secs(10), || {
        !engine.status().connected
    })
    .await;

    // 服务端回来（数据目录不变，所以证书指纹和用户都还在）
    let second = make_server().await;

    // 客户端应当自己连回来，并且把隧道重开
    wait_for(
        "自动重连并恢复隧道",
        Duration::from_secs(30),
        || {
            engine.status().connected
                && engine
                    .status()
                    .tunnel("wx")
                    .and_then(|t| t.url.clone())
                    .is_some()
        },
    )
    .await;

    assert_eq!(
        second.registry.tunnel_count(),
        1,
        "服务端这边也该有这条隧道"
    );
}

/// 凭证被拒时不该反复重连——那既没用又会把服务端日志刷满。
#[tokio::test]
async fn rejected_credentials_stop_the_retry_loop() {
    let server = TestServer::start().await;
    let engine = Engine::start(None, brand(&server));

    let err = engine
        .login(
            server.handle.control_addr.to_string(),
            "cy_nobody_deadbeef",
            &server.handle.fingerprint,
        )
        .await
        .expect_err("错误凭证应当登录失败");
    assert!(err.contains("凭证"), "实际：{err}");

    // 等一会儿，确认它没在后台偷偷重试
    tokio::time::sleep(Duration::from_secs(2)).await;
    let status = engine.status();
    assert!(!status.connected);
    assert!(status.needs_login, "应当停下来等用户重新登录");
}

/// 没连上的时候也能配隧道——连上之后自动开通。
///
/// 反过来（提示"请先登录"然后把用户刚输入的东西丢掉）是最烦人的那类交互。
#[tokio::test]
async fn tunnels_can_be_configured_while_offline() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let engine = Engine::start(None, brand(&server));

    // 还没登录就先配隧道
    engine.add_tunnel("wx", echo).await.expect("离线也该能配");
    assert!(engine.status().tunnel("wx").is_some());
    assert!(engine.status().tunnel("wx").unwrap().url.is_none());

    // 登录之后它应该自己开起来
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");

    wait_for(
        "离线时配的隧道自动开通",
        Duration::from_secs(10),
        || {
            engine
                .status()
                .tunnel("wx")
                .and_then(|t| t.url.clone())
                .is_some()
        },
    )
    .await;
}

/// 状态要能存盘并在重开后恢复——包括「上次哪些开着」。
#[tokio::test]
async fn state_survives_a_restart() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("state.json");

    {
        let engine = Engine::start(Some(state_path.clone()), brand(&server));
        engine
            .login(
                server.handle.control_addr.to_string(),
                &token,
                &server.handle.fingerprint,
            )
            .await
            .unwrap();
        engine.add_tunnel("wx", echo).await.unwrap();
        engine.add_tunnel("admin", 5173).await.ok(); // 本地服务不存在也没关系，配置要留下
        engine.set_enabled("admin", false).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        engine.shutdown().await;
    }

    // 「重开应用」
    let engine = Engine::start(Some(state_path), brand(&server));
    wait_for(
        "恢复上次的连接与隧道",
        Duration::from_secs(15),
        || engine.status().connected && engine.status().tunnel("wx").is_some(),
    )
    .await;

    let status = engine.status();
    assert!(
        status.tunnel("wx").and_then(|t| t.url.clone()).is_some(),
        "上次开着的隧道应当自动恢复"
    );
    // 上次关掉的那条不该自己跑起来
    assert!(
        status
            .tunnel("admin")
            .map(|t| t.url.is_none())
            .unwrap_or(true),
        "上次关着的隧道不该自动开通"
    );
}

/// 本地 API：脚本注册端口、查地址。
#[tokio::test]
async fn local_api_registers_ports_and_resolves_addresses() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo = spawn_echo_server().await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();

    // 挑一个空闲端口起本地 API
    let api_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            let _ = cy_core::local_api::serve(engine, api_port).await;
        });
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let base = format!("http://127.0.0.1:{api_port}");
    let client = reqwest::Client::new();

    // 隧道还没开：resolve 应当回退到本地地址
    let body: serde_json::Value = client
        .get(format!("{base}/api/resolve?port={echo}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["mode"], "local");
    assert_eq!(body["url"], format!("http://127.0.0.1:{echo}"));

    // 脚本注册端口（只给端口，名字自动生成）
    let created: serde_json::Value = client
        .post(format!("{base}/api/tunnels"))
        .json(&serde_json::json!({"port": echo, "name": "api"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created[0]["ok"], true);

    // 现在 resolve 应当给公网地址——业务代码据此生成回调 URL
    let body: serde_json::Value = client
        .get(format!("{base}/api/resolve?port={echo}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["mode"], "tunnel");
    assert_eq!(body["url"], "https://zhangsan-api.t.example.com");

    // shell 友好的形式
    let plain = client
        .get(format!("{base}/api/resolve?port={echo}&plain=1"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(plain, "https://zhangsan-api.t.example.com");
}

/// 批量注册：一个项目起了好几个端口，脚本一次全注册。
#[tokio::test]
async fn local_api_accepts_a_batch_of_ports() {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();

    let api_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            let _ = cy_core::local_api::serve(engine, api_port).await;
        });
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let created: serde_json::Value = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{api_port}/api/tunnels"))
        .json(&serde_json::json!([
            {"port": 8082, "name": "api"},
            {"port": 5173, "name": "admin"},
            {"port": 5666}
        ]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(created.as_array().unwrap().len(), 3);
    assert_eq!(created[0]["name"], "api");
    // 没给名字的按端口生成
    assert_eq!(created[2]["name"], "p5666");

    let status = engine.status();
    assert_eq!(status.tunnels.len(), 3);
}

// ================= 访问口令走完整路径 =================

/// 一个真正的 HTTP 服务。回声服务是 TCP 级的字节回声，HTTP 请求回声回去
/// hyper 解析不了，口令放行之后会拿到 502 而不是 200。
async fn local_http() -> u16 {
    use axum::{routing::get, Router};
    let app = Router::new().route("/", get(|| async { "secret content" }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    p
}

/// 经引擎开的带口令隧道，口令要真的生效。
///
/// 这条测试是补上来的：`tcp_and_auth.rs` 那几条直接调 `conn.open_tunnel(spec)`，
/// 绕过了引擎，所以证明的是「服务端会校验」，不是「产品能用」。实际上引擎那层
/// 只传了名字和端口，口令一路被丢掉——设了等于没设，而用户以为门锁着。
#[tokio::test]
async fn a_password_set_through_the_engine_is_actually_enforced() {
    use cy_core::TunnelSpec;

    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let echo = local_http().await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");

    engine
        .add_tunnel_spec(TunnelSpec::http("wx", echo).with_auth("demo:s3cret"))
        .await
        .expect("开隧道");

    let client = reqwest::Client::builder()
        .resolve("zhangsan-wx.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();

    let anon = client
        .get("http://zhangsan-wx.t.example.com/")
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401, "不带口令必须被挡");

    let wrong = client
        .get("http://zhangsan-wx.t.example.com/")
        .basic_auth("demo", Some("nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401, "口令错也要挡");

    let ok = client
        .get("http://zhangsan-wx.t.example.com/")
        .basic_auth("demo", Some("s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "口令对要放行");
}

/// 重连之后口令还在。
///
/// 引擎重连时会把期望态里的隧道全量重开一遍。如果那条路径不带口令，
/// 隧道会照常回来但门没了——这比一开始就没设更危险，因为没有任何提示。
#[tokio::test]
async fn the_password_survives_a_server_restart() {
    use cy_core::TunnelSpec;

    let data_dir = tempfile::tempdir().unwrap();
    let echo = local_http().await;
    let control_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };

    let make_server = || {
        let dir = data_dir.path().to_path_buf();
        async move {
            let mut config = cy_server::Config {
                storage: cy_server::config::StorageConfig { data_dir: dir },
                ..Default::default()
            };
            config.http.domain_suffix = "t.example.com".into();
            config.control.listen = format!("127.0.0.1:{control_port}").parse().unwrap();
            config.http.listen = "127.0.0.1:0".parse().unwrap();
            config.admin.listen = "127.0.0.1:0".parse().unwrap();
            config.control.heartbeat_secs = 1;
            cy_server::Server::start(config).await.expect("启动服务端")
        }
    };

    let first = make_server().await;
    let token = first.store.add_user("zhangsan", None, 10).await.unwrap();
    let fingerprint = first.fingerprint.clone();

    let engine = Engine::start(
        None,
        Brand {
            default_server: format!("127.0.0.1:{control_port}"),
            tls_pin: fingerprint.clone(),
            update_url: String::new(),
        },
    );
    engine
        .login(format!("127.0.0.1:{control_port}"), &token, &fingerprint)
        .await
        .expect("登录");
    engine
        .add_tunnel_spec(TunnelSpec::http("wx", echo).with_auth("demo:s3cret"))
        .await
        .expect("开隧道");

    first.shutdown().await;
    wait_for("引擎察觉掉线", Duration::from_secs(10), || {
        !engine.status().connected
    })
    .await;

    let second = make_server().await;
    wait_for(
        "自动重连并恢复隧道",
        Duration::from_secs(30),
        || {
            engine.status().connected
                && engine
                    .status()
                    .tunnel("wx")
                    .and_then(|t| t.url.clone())
                    .is_some()
        },
    )
    .await;

    // 重开之后的隧道必须还带着口令
    let client = reqwest::Client::builder()
        .resolve("zhangsan-wx.t.example.com", second.http_addr)
        .build()
        .unwrap();
    let anon = client
        .get("http://zhangsan-wx.t.example.com/")
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401, "重连之后门还得在");

    let ok = client
        .get("http://zhangsan-wx.t.example.com/")
        .basic_auth("demo", Some("s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "带对口令仍然能进");
}

// ================= 升级提示 =================

/// 服务端下载目录里有比我新的包，连上之后状态里就要有「有新版本」。
///
/// 这是升级体系的全部链路：管理员把包丢进目录 → 握手时服务端看一眼目录 →
/// 客户端比对自己的版本 → 界面挂横幅。不经 GitHub，不经另外的 HTTP 接口。
#[tokio::test]
async fn a_newer_package_on_the_server_shows_up_as_an_update() {
    let downloads = tempfile::tempdir().unwrap();
    // 比任何真实版本都大，保证「比当前新」
    std::fs::write(
        downloads.path().join("chuanyun-99.0.0-macos-universal.dmg"),
        b"x",
    )
    .unwrap();
    std::fs::write(
        downloads.path().join("chuanyun-98.0.0-windows-x86_64.msi"),
        b"x",
    )
    .unwrap();

    let dl = downloads.path().to_path_buf();
    let server = TestServer::start_with(move |c| {
        c.admin.download_dir = Some(dl);
        c.admin.download_url = Some("https://t.example.com/download".into());
    })
    .await;
    let token = server.add_user("zhangsan").await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");

    let update = engine.status().update.expect("应该提示有新版本");
    assert_eq!(update.version, "99.0.0", "取目录里最新的那个");
    assert_eq!(
        update.url.as_deref(),
        Some("https://t.example.com/download"),
        "下载地址来自服务端配置"
    );
}

/// 服务端没放安装包就什么都不提示——别拿服务端自己的版本号冒充。
#[tokio::test]
async fn no_packages_on_the_server_means_no_update_prompt() {
    let empty = tempfile::tempdir().unwrap();
    let dl = empty.path().to_path_buf();
    let server = TestServer::start_with(move |c| {
        c.admin.download_dir = Some(dl);
    })
    .await;
    let token = server.add_user("zhangsan").await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");

    assert!(engine.status().update.is_none());
}

/// 目录里的包和我一样新或更旧，也不提示。
#[tokio::test]
async fn an_older_package_is_not_an_update() {
    let downloads = tempfile::tempdir().unwrap();
    std::fs::write(
        downloads.path().join("chuanyun-0.0.1-macos-universal.dmg"),
        b"x",
    )
    .unwrap();
    let dl = downloads.path().to_path_buf();
    let server = TestServer::start_with(move |c| {
        c.admin.download_dir = Some(dl);
    })
    .await;
    let token = server.add_user("zhangsan").await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");

    assert!(engine.status().update.is_none(), "0.0.1 不比当前版本新");
}

/// 项目脚本（base 的 dev.sh、vite 插件）每次启动都按「名字 + 端口」重新注册一遍，
/// 它们不知道也不该知道口令。重新注册不能把隧道主人在客户端设好的口令抹掉——
/// 那会变成启动脚本静默拆门，而且没有任何提示。
#[tokio::test]
async fn a_script_re_registering_the_tunnel_does_not_strip_the_password() {
    use cy_core::TunnelSpec;

    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let http = local_http().await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");

    // 隧道主人在客户端里设了口令
    engine
        .add_tunnel_spec(TunnelSpec::http("api", http).with_auth("demo:s3cret"))
        .await
        .expect("开隧道");

    // dev.sh 启动：只给名字和端口
    engine.add_tunnel("api", http).await.expect("重新注册");

    let client = reqwest::Client::builder()
        .resolve("zhangsan-api.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();
    let anon = client
        .get("http://zhangsan-api.t.example.com/")
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401, "重新注册之后门还得在");
    assert!(
        engine.status().tunnel("api").unwrap().protected,
        "界面上也还该显示「已设口令」"
    );
}

/// 改口令不用删了重建：地址不变，新口令立刻生效。
#[tokio::test]
async fn changing_the_password_keeps_the_address_and_takes_effect_at_once() {
    use cy_core::TunnelSpec;

    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let http = local_http().await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");
    engine
        .add_tunnel_spec(TunnelSpec::http("api", http).with_auth("demo:old"))
        .await
        .expect("开隧道");
    let url_before = engine.status().tunnel("api").unwrap().url.clone();

    engine
        .set_auth("api", Some("demo:new".into()))
        .await
        .expect("改口令");

    assert_eq!(
        engine.status().tunnel("api").unwrap().url,
        url_before,
        "地址不能变"
    );
    assert!(engine.status().tunnel("api").unwrap().protected);

    let client = reqwest::Client::builder()
        .resolve("zhangsan-api.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();
    let old = client
        .get("http://zhangsan-api.t.example.com/")
        .basic_auth("demo", Some("old"))
        .send()
        .await
        .unwrap();
    assert_eq!(old.status(), 401, "旧口令要立刻失效");
    let new = client
        .get("http://zhangsan-api.t.example.com/")
        .basic_auth("demo", Some("new"))
        .send()
        .await
        .unwrap();
    assert_eq!(new.status(), 200, "新口令要能进");

    // 去掉口令
    engine.set_auth("api", None).await.expect("去掉口令");
    assert!(!engine.status().tunnel("api").unwrap().protected);
    let anon = client
        .get("http://zhangsan-api.t.example.com/")
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 200, "去掉口令之后不带口令也能进");

    // 改不存在的隧道要报错，不是静默成功
    assert!(engine.set_auth("nope", Some("a:b".into())).await.is_err());
}

/// 连着不断的客户端也要知道有新包。
///
/// 真机上撞到的：服务端升级重启，客户端立刻重连，握手那一刻目录里还是旧包；
/// 五秒后新包放进去——而客户端再也不重连了，提示永远出不来。
/// 现在心跳每次带最新版本，放进去十几秒内就能看到。
#[tokio::test]
async fn a_package_dropped_in_after_connecting_is_noticed_within_a_heartbeat() {
    let downloads = tempfile::tempdir().unwrap();
    let dl = downloads.path().to_path_buf();
    let server = TestServer::start_with(move |c| {
        c.admin.download_dir = Some(dl);
        c.admin.download_url = Some("https://t.example.com/download".into());
        c.control.heartbeat_secs = 1;
    })
    .await;
    let token = server.add_user("zhangsan").await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .expect("登录");
    assert!(
        engine.status().update.is_none(),
        "连上时目录是空的，不该提示"
    );

    // 管理员这时候才把新包放进去
    std::fs::write(
        downloads.path().join("chuanyun-99.0.0-macos-universal.dmg"),
        b"x",
    )
    .unwrap();

    wait_for(
        "心跳把新版本带过来",
        Duration::from_secs(10),
        || engine.status().update.is_some(),
    )
    .await;
    let u = engine.status().update.unwrap();
    assert_eq!(u.version, "99.0.0");
    assert_eq!(u.url.as_deref(), Some("https://t.example.com/download"));
}
