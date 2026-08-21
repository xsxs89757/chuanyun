//! 端到端：请求观测与重放。
//!
//! 这个功能存在的理由是「支付回调只推有限几次」——所以验收标准不是
//! 「能看到请求」，而是「能拿同一份报文再打一次，且一字不差」。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cy_core::{Brand, Engine};
use cy_e2e::{wait_for, TestServer};

fn brand(server: &TestServer) -> Brand {
    Brand {
        default_server: server.handle.control_addr.to_string(),
        tls_pin: server.handle.fingerprint.clone(),
        update_url: String::new(),
    }
}

/// 一个记录下所有收到过的请求体的假服务。
async fn spawn_recording_service() -> (u16, Arc<Mutex<Vec<String>>>) {
    use axum::{routing::post, Router};

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();

    let app = Router::new().route(
        "/notify",
        post(move |body: String| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(body);
                "SUCCESS"
            }
        }),
    );

    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (port, seen)
}

async fn setup() -> (
    TestServer,
    Engine,
    u16,
    Arc<Mutex<Vec<String>>>,
    reqwest::Client,
) {
    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;
    let (port, seen) = spawn_recording_service().await;

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();
    engine.add_tunnel("wx", port).await.unwrap();

    let client = reqwest::Client::builder()
        .resolve("zhangsan-wx.t.example.com", server.handle.http_addr)
        .build()
        .unwrap();

    (server, engine, port, seen, client)
}

const CALLBACK: &str = r#"{"out_trade_no":"X20260821","total_fee":1,"sign":"A1B2C3"}"#;

#[tokio::test]
async fn requests_through_the_tunnel_are_recorded() {
    let (_server, engine, _port, _seen, client) = setup().await;

    client
        .post("http://zhangsan-wx.t.example.com/notify")
        .header("content-type", "application/json")
        .header("x-wx-signature", "A1B2C3")
        .body(CALLBACK)
        .send()
        .await
        .unwrap();

    wait_for("请求被记录下来", Duration::from_secs(5), || {
        !engine.inspector().list(Some("wx")).is_empty()
    })
    .await;

    let records = engine.inspector().list(Some("wx"));
    let r = &records[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.path, "/notify");
    assert_eq!(r.status, Some(200), "响应状态码要回填");
    assert!(r.duration.is_some(), "耗时要记下来");
    assert_eq!(r.header("x-wx-signature"), Some("A1B2C3"));
    assert!(
        r.body_text().contains("X20260821"),
        "报文：{}",
        r.body_text()
    );
}

/// 重放：同一份报文再打一次，本地服务应当收到**一模一样**的内容。
///
/// 这是整个功能的立身之本——签名、时间戳、订单号都不能变，
/// 否则重放出来的请求会验签失败，等于没有重放。
#[tokio::test]
async fn replay_delivers_byte_identical_payload() {
    let (_server, engine, port, seen, client) = setup().await;

    client
        .post("http://zhangsan-wx.t.example.com/notify")
        .header("content-type", "application/json")
        .header("x-wx-signature", "A1B2C3")
        .body(CALLBACK)
        .send()
        .await
        .unwrap();

    wait_for("请求被记录", Duration::from_secs(5), || {
        !engine.inspector().list(Some("wx")).is_empty()
    })
    .await;
    assert_eq!(seen.lock().unwrap().len(), 1, "本地服务收到了原始请求");

    // 重放
    let record = engine.inspector().list(Some("wx"))[0].clone();
    let (status, _) = cy_core::inspector::replay(&record, port)
        .await
        .expect("重放应当成功");
    assert_eq!(status, 200);

    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "本地服务应当收到第二份");
    assert_eq!(bodies[0], bodies[1], "重放的报文必须和原始的一字不差");
    assert_eq!(bodies[1], CALLBACK);
}

/// 重放可以来来回回做很多次——这正是它的用处。
#[tokio::test]
async fn a_callback_can_be_replayed_repeatedly() {
    let (_server, engine, port, seen, client) = setup().await;

    client
        .post("http://zhangsan-wx.t.example.com/notify")
        .body(CALLBACK)
        .send()
        .await
        .unwrap();
    wait_for("请求被记录", Duration::from_secs(5), || {
        !engine.inspector().list(Some("wx")).is_empty()
    })
    .await;

    let record = engine.inspector().list(Some("wx"))[0].clone();
    for _ in 0..5 {
        cy_core::inspector::replay(&record, port).await.unwrap();
    }

    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 6, "1 次原始 + 5 次重放");
    assert!(bodies.iter().all(|b| b == CALLBACK));
}

/// 观测不能干扰隧道本身——大文件、WebSocket 都得照常工作。
#[tokio::test]
async fn inspection_does_not_break_large_transfers() {
    use axum::{routing::post, Router};

    let server = TestServer::start().await;
    let token = server.add_user("zhangsan").await;

    let app = Router::new().route(
        "/upload",
        post(|body: axum::body::Body| async move {
            use futures::StreamExt;
            let mut total = 0usize;
            let mut stream = body.into_data_stream();
            while let Some(Ok(c)) = stream.next().await {
                total += c.len();
            }
            total.to_string()
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });

    let engine = Engine::start(None, brand(&server));
    engine
        .login(
            server.handle.control_addr.to_string(),
            &token,
            &server.handle.fingerprint,
        )
        .await
        .unwrap();
    engine.add_tunnel("up", port).await.unwrap();

    let client = reqwest::Client::builder()
        .resolve("zhangsan-up.t.example.com", server.handle.http_addr)
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

    // 20MB——远超观测的抓取上限，考验的是「抓取只抄一份、不影响转发」
    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 320;
    let stream = futures::stream::iter(
        (0..CHUNKS).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from_static(&[b'x'; CHUNK]))),
    );

    let total = client
        .post("http://zhangsan-up.t.example.com/upload")
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
        "观测层不该影响转发的字节数"
    );
}

/// 敏感头不该出现在记录里——观测面板会被截图发群里。
#[tokio::test]
async fn recorded_requests_hide_credentials() {
    let (_server, engine, _port, _seen, client) = setup().await;

    client
        .post("http://zhangsan-wx.t.example.com/notify")
        .header("cookie", "session=super-secret")
        .header("authorization", "Bearer my-token")
        .body(CALLBACK)
        .send()
        .await
        .unwrap();

    wait_for("请求被记录", Duration::from_secs(5), || {
        !engine.inspector().list(Some("wx")).is_empty()
    })
    .await;

    let r = engine.inspector().list(Some("wx"))[0].clone();
    assert_eq!(r.header("cookie"), Some("（已隐去）"));
    assert_eq!(r.header("authorization"), Some("（已隐去）"));
}

/// 本地 API 也能拿到记录并触发重放——脚本和界面都走这条路。
#[tokio::test]
async fn local_api_exposes_records_and_replay() {
    let (_server, engine, _port, seen, client) = setup().await;

    client
        .post("http://zhangsan-wx.t.example.com/notify")
        .body(CALLBACK)
        .send()
        .await
        .unwrap();
    wait_for("请求被记录", Duration::from_secs(5), || {
        !engine.inspector().list(Some("wx")).is_empty()
    })
    .await;

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

    let api = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{api_port}");

    let list: serde_json::Value = api
        .get(format!("{base}/api/requests?tunnel=wx"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list[0]["method"], "POST");
    assert_eq!(list[0]["path"], "/notify");
    let id = list[0]["id"].as_u64().unwrap();

    // 详情里能看到报文
    let detail: serde_json::Value = api
        .get(format!("{base}/api/requests/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(detail["body"].as_str().unwrap().contains("X20260821"));

    // 重放
    let replayed: serde_json::Value = api
        .post(format!("{base}/api/requests/{id}/replay"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replayed["status"], 200);
    assert_eq!(seen.lock().unwrap().len(), 2, "重放应当再打一次本地服务");

    // 清空
    api.delete(format!("{base}/api/requests"))
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = api
        .get(format!("{base}/api/requests"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 0);
}
