//! 本地 API：让项目脚本和业务代码跟穿云说话。
//!
//! 解决两件真实的麻烦：
//!
//! 1. **端口是会变的。** 一个项目今天起 8082，明天加了 5666、5678，每加一个就要
//!    有人去界面上手工建隧道。启动脚本调一下这里，新端口自动接入。
//! 2. **回调地址要跟着环境切换。** 业务代码里写死公网地址，本地跑就不通；写死
//!    localhost，微信又回调不到。调 `/api/resolve`：隧道开着给公网地址，
//!    关着给本地地址，代码一份就够。
//!
//! # 安全
//!
//! 只监听回环，但这挡不住浏览器——任何网页里的 JS 都能 fetch 到 127.0.0.1，
//! 配上 DNS rebinding 还能读到响应，那就等于把用户的本地端口暴露给了一个网页。
//! 所以带浏览器特征头的请求一律拒绝（curl 和脚本不会带），Host 也必须是回环。

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::engine::Engine;

/// 本地 API 的固定端口。
///
/// 有意不做「被占就换下一个」：脚本里写死 7075、应用却悄悄漂到 7076，
/// 这种问题比"端口被占，启动失败"难查得多。占用了就报错，让用户知道。
pub const DEFAULT_PORT: u16 = 7075;

pub async fn serve(engine: Engine, port: u16) -> std::io::Result<()> {
    let app = Router::new()
        .route("/api/status", get(status))
        .route("/api/tunnels", get(list_tunnels).post(create_tunnels))
        .route("/api/tunnels/{name}", delete(remove_tunnel))
        .route("/api/resolve", get(resolve))
        .route("/api/connects", get(list_connects).post(create_connect))
        .route("/api/connects/{port}", delete(remove_connect))
        .route("/api/requests", get(list_requests).delete(clear_requests))
        .route("/api/requests/{id}", get(get_request))
        .route("/api/requests/{id}/replay", post(replay_request))
        .route("/api/shutdown", post(shutdown))
        .layer(axum::middleware::from_fn(guard_local_only))
        .with_state(Arc::new(engine));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "本地 API 已就绪");
    axum::serve(listener, app).await
}

async fn guard_local_only(req: Request, next: Next) -> Response {
    let headers = req.headers();
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());

    // 绑回环挡不住网页里的 JS，所以另外看请求像不像浏览器发的。
    // 判定逻辑与服务端管理接口共用一份（cy_proto::guard），
    // 安全规则各写一遍迟早有一处漏补丁。
    if cy_proto::guard::looks_like_browser(get) {
        return (StatusCode::FORBIDDEN, "本地 API 不接受来自浏览器的请求。\n").into_response();
    }

    // DNS rebinding：域名解析到了 127.0.0.1，但 Host 里还留着攻击者的域名
    if !cy_proto::guard::host_is_loopback(get("host")) {
        return (
            StatusCode::FORBIDDEN,
            "Host 必须是 127.0.0.1 或 localhost。\n",
        )
            .into_response();
    }

    next.run(req).await
}

#[derive(Serialize)]
struct StatusBody {
    connected: bool,
    domain_suffix: String,
    needs_login: bool,
    reconnect_attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

async fn status(State(engine): State<Arc<Engine>>) -> Json<StatusBody> {
    let s = engine.status();
    Json(StatusBody {
        connected: s.connected,
        domain_suffix: s.domain_suffix,
        needs_login: s.needs_login,
        reconnect_attempt: s.reconnect_attempt,
        last_error: s.last_error,
    })
}

#[derive(Serialize)]
struct TunnelBody {
    name: String,
    local_port: u16,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn list_tunnels(State(engine): State<Arc<Engine>>) -> Json<Vec<TunnelBody>> {
    Json(
        engine
            .status()
            .tunnels
            .into_iter()
            .map(|t| TunnelBody {
                name: t.name,
                local_port: t.local_port,
                enabled: t.enabled,
                url: t.url,
                error: t.error,
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct NewTunnel {
    port: u16,
    /// 不填就按端口自动生成，如 `p5678`。
    ///
    /// 脚本里最常见的写法是只给端口——名字对机器不重要，
    /// 但对人重要，所以生成的名字也得是能看懂的。
    #[serde(default)]
    name: Option<String>,
}

/// 接受单个对象或数组，脚本里两种写法都自然。
#[derive(Deserialize)]
#[serde(untagged)]
enum NewTunnels {
    One(NewTunnel),
    Many(Vec<NewTunnel>),
}

#[derive(Serialize)]
struct CreateResult {
    name: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn create_tunnels(
    State(engine): State<Arc<Engine>>,
    Json(body): Json<NewTunnels>,
) -> Json<Vec<CreateResult>> {
    let items = match body {
        NewTunnels::One(t) => vec![t],
        NewTunnels::Many(v) => v,
    };

    let mut results = Vec::new();
    for item in items {
        let name = item.name.unwrap_or_else(|| format!("p{}", item.port));
        // 按名字幂等：脚本重复执行不该报错，也不该开出一堆重复隧道
        match engine.add_tunnel(&name, item.port).await {
            Ok(()) => {
                let url = engine.status().tunnel(&name).and_then(|t| t.url.clone());
                results.push(CreateResult {
                    name,
                    ok: true,
                    url,
                    error: None,
                });
            }
            Err(e) => results.push(CreateResult {
                name,
                ok: false,
                url: None,
                error: Some(e),
            }),
        }
    }
    Json(results)
}

async fn remove_tunnel(
    State(engine): State<Arc<Engine>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> StatusCode {
    engine.remove_tunnel(name).await;
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct ResolveQuery {
    port: u16,
    /// 加上 `plain=1` 只返回一行地址，方便 shell 里直接用
    #[serde(default)]
    plain: Option<String>,
}

#[derive(Serialize)]
struct Resolved {
    url: String,
    /// `tunnel` = 走公网，`local` = 回本地
    mode: &'static str,
}

/// 这个端口现在对外的地址。
///
/// 隧道开着就给公网地址，没开就给 `http://127.0.0.1:<port>`。
/// 业务代码统一调它生成回调地址，开关隧道就能切环境，不用改代码也不用改配置。
async fn resolve(State(engine): State<Arc<Engine>>, Query(q): Query<ResolveQuery>) -> Response {
    let status = engine.status();
    let (url, mode) = match status.public_url_for_port(q.port) {
        Some(url) => (url.to_string(), "tunnel"),
        None => (format!("http://127.0.0.1:{}", q.port), "local"),
    };

    if q.plain.is_some() {
        return ([("content-type", "text/plain; charset=utf-8")], url).into_response();
    }
    Json(Resolved { url, mode }).into_response()
}

// ================= 接入同事的服务 =================

#[derive(Serialize)]
struct ConnectBody {
    local_port: u16,
    from: String,
    upstream: String,
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn list_connects(State(engine): State<Arc<Engine>>) -> Json<Vec<ConnectBody>> {
    Json(
        engine
            .status()
            .connects
            .into_iter()
            .map(|c| ConnectBody {
                local_port: c.local_port,
                from: c.from,
                upstream: c.upstream,
                running: c.running,
                error: c.error,
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct NewConnect {
    local_port: u16,
    /// 隧道名（如 `zhangsan-api`）或完整 URL
    from: String,
    #[serde(default)]
    auth: Option<String>,
}

async fn create_connect(
    State(engine): State<Arc<Engine>>,
    Json(body): Json<NewConnect>,
) -> Response {
    let mut spec = crate::connect::ConnectSpec::new(body.local_port, body.from);
    if let Some(auth) = body.auth {
        spec = spec.with_auth(auth);
    }
    match engine.add_connect(spec).await {
        Ok(upstream) => {
            Json(serde_json::json!({ "ok": true, "upstream": upstream })).into_response()
        }
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

async fn remove_connect(
    State(engine): State<Arc<Engine>>,
    axum::extract::Path(port): axum::extract::Path<u16>,
) -> StatusCode {
    engine.remove_connect(port).await;
    StatusCode::NO_CONTENT
}

// ================= 请求观测与重放 =================

#[derive(Deserialize)]
struct RequestQuery {
    /// 只看某条隧道的记录
    #[serde(default)]
    tunnel: Option<String>,
}

#[derive(Serialize)]
struct RequestSummary {
    id: u64,
    tunnel: String,
    method: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
    /// Unix 秒
    at: u64,
}

fn summarize(r: &crate::inspector::Record) -> RequestSummary {
    RequestSummary {
        id: r.id,
        tunnel: r.tunnel.clone(),
        method: r.method.clone(),
        path: r.path.clone(),
        status: r.status,
        duration_ms: r.duration.map(|d| d.as_millis()),
        peer: r.peer.clone(),
        at: r
            .at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }
}

async fn list_requests(
    State(engine): State<Arc<Engine>>,
    Query(q): Query<RequestQuery>,
) -> Json<Vec<RequestSummary>> {
    Json(
        engine
            .inspector()
            .list(q.tunnel.as_deref())
            .iter()
            .map(summarize)
            .collect(),
    )
}

#[derive(Serialize)]
struct RequestDetail {
    #[serde(flatten)]
    summary: RequestSummary,
    headers: Vec<(String, String)>,
    body: String,
    body_truncated: usize,
}

async fn get_request(
    State(engine): State<Arc<Engine>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Response {
    match engine.inspector().get(id) {
        Some(r) => Json(RequestDetail {
            summary: summarize(&r),
            headers: r.headers.clone(),
            body: r.body_text(),
            body_truncated: r.body_truncated,
        })
        .into_response(),
        None => (StatusCode::NOT_FOUND, "没有这条记录\n").into_response(),
    }
}

async fn clear_requests(
    State(engine): State<Arc<Engine>>,
    Query(q): Query<RequestQuery>,
) -> StatusCode {
    engine.inspector().clear(q.tunnel.as_deref());
    StatusCode::NO_CONTENT
}

#[derive(Serialize)]
struct ReplayResult {
    status: u16,
    response: String,
}

/// 把一条记录原样重发到本地服务。
///
/// 支付回调只会推送有限几次，推完就没了。有了这个接口，改一行代码就能
/// 拿同一份报文（同样的签名、同样的时间戳）再试一次。
async fn replay_request(
    State(engine): State<Arc<Engine>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Response {
    let Some(record) = engine.inspector().get(id) else {
        return (StatusCode::NOT_FOUND, "没有这条记录\n").into_response();
    };
    // 重放要打回原来那条隧道对应的本地端口——记录里存的是隧道名，
    // 端口可能已经改了，以当前配置为准
    let Some(port) = engine.status().tunnel(&record.tunnel).map(|t| t.local_port) else {
        return (
            StatusCode::CONFLICT,
            "这条记录所属的隧道已经不在了，没法确定该重放到哪个端口\n",
        )
            .into_response();
    };

    match crate::inspector::replay(&record, port).await {
        Ok((status, response)) => Json(ReplayResult { status, response }).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("重放失败：{e}\n")).into_response(),
    }
}

async fn shutdown(State(engine): State<Arc<Engine>>) -> StatusCode {
    engine.shutdown().await;
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn app() -> Router {
        let engine = Engine::start(None, crate::engine::Brand::default());
        Router::new()
            .route("/api/status", get(status))
            .route("/api/resolve", get(resolve))
            .route("/api/connects", get(list_connects).post(create_connect))
            .route("/api/connects/{port}", delete(remove_connect))
            .route("/api/requests", get(list_requests).delete(clear_requests))
            .route("/api/requests/{id}", get(get_request))
            .route("/api/requests/{id}/replay", post(replay_request))
            .layer(axum::middleware::from_fn(guard_local_only))
            .with_state(Arc::new(engine))
    }

    async fn get_body(uri: &str) -> (StatusCode, String) {
        let req = HttpRequest::builder()
            .uri(uri)
            .header("host", "127.0.0.1:7075")
            .body(Body::empty())
            .unwrap();
        let resp = app().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn resolve_falls_back_to_localhost_when_no_tunnel() {
        let (status, body) = get_body("/api/resolve?port=5678").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("http://127.0.0.1:5678"), "实际返回：{body}");
        assert!(body.contains("\"local\""));
    }

    #[tokio::test]
    async fn plain_form_is_shell_friendly() {
        let (_, body) = get_body("/api/resolve?port=5678&plain=1").await;
        // shell 里直接 $(curl ...) 用，不该有引号和 JSON 包裹
        assert_eq!(body, "http://127.0.0.1:5678");
    }

    #[tokio::test]
    async fn browser_requests_are_refused() {
        let req = HttpRequest::builder()
            .uri("/api/status")
            .header("host", "127.0.0.1:7075")
            .header("origin", "https://evil.example.com")
            .body(Body::empty())
            .unwrap();
        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "网页 JS 能 fetch 到回环地址，这道闸不能少"
        );
    }

    /// Node 的 fetch 会发 `sec-fetch-mode: cors`。
    ///
    /// 早先把这个头当浏览器特征，结果所有 Node 脚本都被挡在门外——
    /// 包括我们自己的 vite 插件。这条守着别再犯。
    #[tokio::test]
    async fn node_style_requests_are_allowed() {
        let req = HttpRequest::builder()
            .uri("/api/status")
            .header("host", "127.0.0.1:7075")
            .header("sec-fetch-mode", "cors")
            .header("user-agent", "node")
            .header("accept", "*/*")
            .body(Body::empty())
            .unwrap();
        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "脚本调本地接口是这套 API 存在的理由，不能被挡"
        );
    }

    #[tokio::test]
    async fn dns_rebinding_is_refused() {
        let req = HttpRequest::builder()
            .uri("/api/status")
            .header("host", "evil.example.com")
            .body(Body::empty())
            .unwrap();
        let resp = app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn tunnel_name_defaults_to_the_port() {
        // 脚本里最常见的写法是只给端口
        let one: NewTunnels = serde_json::from_str(r#"{"port":5678}"#).unwrap();
        match one {
            NewTunnels::One(t) => {
                assert_eq!(t.port, 5678);
                assert_eq!(t.name.unwrap_or_else(|| format!("p{}", t.port)), "p5678");
            }
            _ => panic!("应解析成单个对象"),
        }
    }

    #[test]
    fn batch_form_is_accepted_too() {
        let many: NewTunnels =
            serde_json::from_str(r#"[{"port":8082,"name":"api"},{"port":5666}]"#).unwrap();
        match many {
            NewTunnels::Many(v) => assert_eq!(v.len(), 2),
            _ => panic!("应解析成数组"),
        }
    }
}
