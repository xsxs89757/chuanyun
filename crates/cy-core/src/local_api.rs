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
use std::time::Duration;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::engine::Engine;

/// 本地 API 的固定端口。
///
/// 有意不做「被占就换下一个」：脚本里写死 7075、应用却悄悄漂到 7076，
/// 这种问题比"端口被占，启动失败"难查得多。占用了就报错，让用户知道。
pub const DEFAULT_PORT: u16 = 7075;

/// 桌面端接进来的两个动作。无头模式没有窗口，不接就是 `None`。
///
/// 另一个穿云启动时靠它们和已经在跑的这个打交道（见 [`crate::peer`]）：
/// 版本不比它新就请它把窗口叫出来、自己退出；比它新就请它让位、自己接班。
#[derive(Clone, Default)]
pub struct AppHooks {
    /// 把主窗口叫到最前面
    pub show: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 整个应用退出：断开连接、收掉托盘图标、结束进程
    pub quit: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub async fn serve(engine: Engine, port: u16) -> std::io::Result<()> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    serve_on(listener, engine, AppHooks::default()).await
}

/// 在一个已经绑好的端口上提供服务。
///
/// 桌面端启动时要先占住端口再做别的：端口能不能绑上，正是判断「有没有另一个
/// 穿云已经在跑」的第一道信号，绑上了就别松手，免得中间被别人抢走。
pub async fn serve_on(
    listener: std::net::TcpListener,
    engine: Engine,
    hooks: AppHooks,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(port = addr.port(), "本地 API 已就绪");
    }
    axum::serve(listener, router(engine, hooks)).await
}

fn router(engine: Engine, hooks: AppHooks) -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/tunnels", get(list_tunnels).post(create_tunnels))
        .route(
            "/api/tunnels/{name}",
            delete(remove_tunnel).patch(update_tunnel),
        )
        .route("/api/resolve", get(resolve))
        .route("/api/connects", get(list_connects).post(create_connect))
        .route("/api/connects/{port}", delete(remove_connect))
        .route("/api/requests", get(list_requests).delete(clear_requests))
        .route("/api/requests/{id}", get(get_request))
        .route("/api/requests/{id}/replay", post(replay_request))
        .route("/api/shutdown", post(shutdown))
        .route("/api/show", post(show))
        .route("/api/quit", post(quit))
        .layer(Extension(hooks))
        .layer(axum::middleware::from_fn(guard_local_only))
        .with_state(Arc::new(engine))
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
    /// 当前客户端版本
    version: &'static str,
    /// 进程号。新版本接班时，旧的迟迟不退就靠它结束进程
    pid: u32,
    /// 服务端上有更新的版本时才出现
    #[serde(skip_serializing_if = "Option::is_none")]
    update: Option<UpdateBody>,
}

#[derive(Serialize)]
struct UpdateBody {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

async fn status(State(engine): State<Arc<Engine>>) -> Json<StatusBody> {
    let s = engine.status();
    Json(StatusBody {
        connected: s.connected,
        domain_suffix: s.domain_suffix,
        needs_login: s.needs_login,
        reconnect_attempt: s.reconnect_attempt,
        last_error: s.last_error,
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        update: s.update.map(|u| UpdateBody {
            version: u.version,
            url: u.url,
        }),
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
    /// 设了访问口令（口令本身不回显）
    protected: bool,
    /// 口令的用户名部分
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_user: Option<String>,
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
                protected: t.protected,
                auth_user: t.auth_user,
            })
            .collect(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTunnel {
    port: u16,
    /// 不填就按端口自动生成，如 `p5678`。
    ///
    /// 脚本里最常见的写法是只给端口——名字对机器不重要，
    /// 但对人重要，所以生成的名字也得是能看懂的。
    #[serde(default)]
    name: Option<String>,
    /// 访问口令，`用户名:口令`。给隧道加一道门。
    #[serde(default)]
    auth: Option<String>,
    /// 自定义域名（要管理员先登记给本人）。
    #[serde(default)]
    domain: Option<String>,
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
        let mut spec = crate::client::TunnelSpec::http(&name, item.port);
        if let Some(auth) = item.auth {
            spec = spec.with_auth(auth);
        }
        if let Some(domain) = item.domain {
            spec = spec.with_domain(domain);
        }
        // 按名字幂等：脚本重复执行不该报错，也不该开出一堆重复隧道
        match engine.add_tunnel_spec(spec).await {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TunnelPatch {
    /// 新口令；传空字符串表示去掉口令
    #[serde(default)]
    auth: Option<String>,
    /// 开/关。脚本退出时用 `false` 把隧道关掉而不是 DELETE——DELETE 会把这条隧道
    /// 连同用户在客户端设的口令一起删掉，下次启动回来的就是一条没门的隧道。
    #[serde(default)]
    enabled: Option<bool>,
}

/// 改一条隧道的口令或开关，地址不变。
async fn update_tunnel(
    State(engine): State<Arc<Engine>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(body): Json<TunnelPatch>,
) -> Response {
    if body.auth.is_none() && body.enabled.is_none() {
        return (StatusCode::BAD_REQUEST, "要改什么？支持 auth、enabled\n").into_response();
    }
    if engine.status().tunnel(&name).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": format!("没有叫 {name} 的隧道") })),
        )
            .into_response();
    }
    if let Some(auth) = body.auth {
        let auth = if auth.trim().is_empty() {
            None
        } else {
            if !auth.contains(':') {
                return (StatusCode::BAD_REQUEST, "口令要写成 用户名:口令\n").into_response();
            }
            Some(auth)
        };
        if let Err(e) = engine.set_auth(&name, auth).await {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "ok": false, "error": e })),
            )
                .into_response();
        }
    }
    if let Some(enabled) = body.enabled {
        engine.set_enabled(&name, enabled).await;
    }
    let t = engine.status().tunnel(&name).cloned();
    Json(serde_json::json!({
        "ok": true,
        "protected": t.as_ref().map(|t| t.protected).unwrap_or(false),
        "enabled": t.as_ref().map(|t| t.enabled).unwrap_or(false),
    }))
    .into_response()
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
    // 引擎正卡在连服务器的握手里时，要等握手有结果才处理得到——别让调用方跟着干等
    let _ = tokio::time::timeout(Duration::from_secs(3), engine.shutdown()).await;
    StatusCode::NO_CONTENT
}

async fn show(Extension(hooks): Extension<AppHooks>) -> Response {
    match &hooks.show {
        Some(show) => {
            show();
            StatusCode::NO_CONTENT.into_response()
        }
        None => (
            StatusCode::NOT_IMPLEMENTED,
            "这个穿云没有窗口（无头模式）\n",
        )
            .into_response(),
    }
}

async fn quit(Extension(hooks): Extension<AppHooks>) -> Response {
    match &hooks.quit {
        Some(quit) => {
            // 只是发起：真正退出要先断开连接，这个应答得赶在进程结束前送出去
            quit();
            StatusCode::ACCEPTED.into_response()
        }
        None => (
            StatusCode::NOT_IMPLEMENTED,
            "这个穿云不能从这里退出（无头模式），请用 /api/shutdown\n",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn app() -> Router {
        app_with(AppHooks::default())
    }

    /// 和 `serve_on` 用的是同一个 router，测试里不另抄一份路由表
    fn app_with(hooks: AppHooks) -> Router {
        let engine = Engine::start(None, crate::engine::Brand::default());
        router(engine, hooks)
    }

    async fn post_to(app: Router, uri: &str, origin: Option<&str>) -> StatusCode {
        let mut req = HttpRequest::builder()
            .method("POST")
            .uri(uri)
            .header("host", "127.0.0.1:7075");
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        app.oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// 记下被调了几次的钩子
    fn counting_hook() -> (
        Arc<dyn Fn() + Send + Sync>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook = {
            let n = n.clone();
            Arc::new(move || {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        };
        (hook, n)
    }

    #[tokio::test]
    async fn show_and_quit_reach_the_desktop_hooks() {
        let (show, shown) = counting_hook();
        let (quit, quits) = counting_hook();
        let hooks = AppHooks {
            show: Some(show),
            quit: Some(quit),
        };

        assert_eq!(
            post_to(app_with(hooks.clone()), "/api/show", None).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(shown.load(std::sync::atomic::Ordering::SeqCst), 1);

        assert_eq!(
            post_to(app_with(hooks), "/api/quit", None).await,
            StatusCode::ACCEPTED
        );
        assert_eq!(quits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn headless_has_no_window_to_show_or_quit() {
        assert_eq!(
            post_to(app(), "/api/show", None).await,
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            post_to(app(), "/api/quit", None).await,
            StatusCode::NOT_IMPLEMENTED
        );
    }

    /// 网页里的 JS 也能往回环地址发 POST——不能让任何一个网页把用户的穿云关掉。
    #[tokio::test]
    async fn web_pages_cannot_quit_the_app() {
        let (quit, quits) = counting_hook();
        let hooks = AppHooks {
            show: None,
            quit: Some(quit),
        };
        assert_eq!(
            post_to(
                app_with(hooks),
                "/api/quit",
                Some("https://evil.example.com")
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(quits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn status_reports_version_and_pid() {
        let (status, body) = get_body("/api/status").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["pid"], std::process::id());
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
        // 打错字段名必须报错，不能像以前那样悄悄丢掉——访问口令就是这么失效的
        assert!(
            serde_json::from_str::<NewTunnels>(r#"{"port":5678,"auht":"a:b"}"#).is_err(),
            "打错的字段要被拒绝"
        );

        let with_auth: NewTunnels =
            serde_json::from_str(r#"{"port":5678,"auth":"demo:s3cret"}"#).unwrap();
        match with_auth {
            NewTunnels::One(t) => assert_eq!(t.auth.as_deref(), Some("demo:s3cret")),
            _ => panic!("应解析成单个"),
        }

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
