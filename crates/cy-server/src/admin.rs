//! 管理接口：只监听回环，给命令行和运维用。
//!
//! # 为什么绑了回环还要做鉴权
//!
//! 「绑 127.0.0.1 就安全」这个直觉在有浏览器的机器上不成立：任何网页里的 JS 都能
//! `fetch("http://127.0.0.1:7001/...")`，再配合 DNS rebinding 甚至能读到响应。
//! 服务器上还常有多个 ssh 用户共用一台机器。
//!
//! 所以这里加两道闸，都不需要用户配置任何东西：
//! - 带浏览器特征头（`Origin` / `Sec-Fetch-Site`）的请求一律拒绝——curl 和脚本
//!   不会带这些头，浏览器一定会带；
//! - `Host` 必须是回环地址，挡住 DNS rebinding（那种攻击靠的就是把域名解析到
//!   127.0.0.1，但 Host 头里留着攻击者的域名）。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::registry::Registry;
use crate::store::Store;

#[derive(Clone)]
pub struct AdminState {
    pub store: Store,
    pub registry: Arc<Registry>,
    pub fingerprint: String,
    pub domain_suffix: String,
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AdminState,
    shutdown: CancellationToken,
) {
    let app = Router::new()
        .route("/status", get(status))
        .route("/tunnels", get(tunnels))
        .route("/users", get(users))
        .route("/kick/{user}", post(kick))
        .route("/audit", get(audit))
        .route("/api/client/version", get(client_version))
        .route("/download", get(download_page))
        .layer(axum::middleware::from_fn(guard_local_only))
        .with_state(state);

    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown.cancelled().await;
    });
    if let Err(e) = server.await {
        tracing::warn!(error = %e, "管理接口结束");
    }
}

/// 挡住浏览器发起的请求，以及 Host 不是回环的请求。
async fn guard_local_only(req: Request, next: Next) -> Response {
    let headers = req.headers();

    // 浏览器一定会带这些头之一，curl / 脚本不会。
    let looks_like_browser = headers.contains_key("origin")
        || headers.contains_key("sec-fetch-site")
        || headers.contains_key("sec-fetch-mode");
    if looks_like_browser {
        return (
            StatusCode::FORBIDDEN,
            "管理接口不接受来自浏览器的请求。请用命令行工具访问。\n",
        )
            .into_response();
    }

    // DNS rebinding 的特征：解析到了 127.0.0.1，但 Host 里还留着攻击者的域名。
    let host_ok = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|h| {
            let name = cy_proto::naming::host_without_port(h);
            name == "127.0.0.1" || name == "localhost" || name == "[::1]" || name == "::1"
        })
        .unwrap_or(false);
    if !host_ok {
        return (
            StatusCode::FORBIDDEN,
            "Host 必须是 127.0.0.1 或 localhost。\n",
        )
            .into_response();
    }

    next.run(req).await
}

#[derive(Serialize)]
struct Status {
    server: &'static str,
    proto: u32,
    fingerprint: String,
    domain_suffix: String,
    sessions: usize,
    tunnels: usize,
}

async fn status(State(s): State<AdminState>) -> Json<Status> {
    Json(Status {
        server: crate::SERVER_VERSION,
        proto: cy_proto::PROTO_VERSION,
        fingerprint: s.fingerprint.clone(),
        domain_suffix: s.domain_suffix.clone(),
        sessions: s.registry.session_count(),
        tunnels: s.registry.tunnel_count(),
    })
}

#[derive(Serialize)]
struct TunnelRow {
    host: String,
    user: String,
    name: String,
}

async fn tunnels(State(s): State<AdminState>) -> Json<Vec<TunnelRow>> {
    Json(
        s.registry
            .list_tunnels()
            .into_iter()
            .map(|(host, user, name)| TunnelRow { host, user, name })
            .collect(),
    )
}

#[derive(Serialize)]
struct UserRow {
    name: String,
    created_at: i64,
    expires_at: Option<i64>,
    revoked: bool,
    online: bool,
}

async fn users(State(s): State<AdminState>) -> Result<Json<Vec<UserRow>>, AdminError> {
    let online: std::collections::HashSet<String> = s
        .registry
        .list_tunnels()
        .into_iter()
        .map(|(_, user, _)| user)
        .collect();

    let rows = s
        .store
        .list_users()
        .await?
        .into_iter()
        .map(|u| UserRow {
            online: online.contains(&u.name),
            name: u.name,
            created_at: u.created_at,
            expires_at: u.expires_at,
            revoked: u.revoked_at.is_some(),
        })
        .collect();
    Ok(Json(rows))
}

#[derive(Serialize)]
struct Kicked {
    kicked: usize,
}

async fn kick(
    State(s): State<AdminState>,
    axum::extract::Path(user): axum::extract::Path<String>,
) -> Json<Kicked> {
    let kicked = s.registry.kick_user(&user);
    tracing::info!(%user, kicked, "管理员踢人");
    Json(Kicked { kicked })
}

#[derive(Serialize)]
struct AuditRow {
    ts: i64,
    user: String,
    action: String,
}

async fn audit(State(s): State<AdminState>) -> Result<Json<Vec<AuditRow>>, AdminError> {
    let rows = s
        .store
        .recent_audit(200)
        .await?
        .into_iter()
        .map(|(ts, user, action)| AuditRow { ts, user, action })
        .collect();
    Ok(Json(rows))
}

#[derive(Serialize)]
struct ClientVersion {
    version: &'static str,
    notes: &'static str,
}

/// 客户端启动时查这里，看看有没有新版本。
async fn client_version() -> Json<ClientVersion> {
    Json(ClientVersion {
        version: crate::SERVER_VERSION,
        notes: "",
    })
}

/// 下载页：新同事拿这个链接装客户端。
async fn download_page(State(s): State<AdminState>) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>下载穿云客户端</title>
<style>
  body {{ margin:0; padding:3rem 1.5rem; background:#f6f8f7; color:#1b2723;
         font-family:-apple-system,"PingFang SC","Microsoft YaHei",sans-serif; line-height:1.9; }}
  main {{ max-width:42rem; margin:0 auto; }}
  h1 {{ font-size:1.6rem; margin:0 0 .5rem; }}
  h2 {{ font-size:1.05rem; margin:2rem 0 .5rem; }}
  .mark {{ font-size:.75rem; letter-spacing:.18em; color:#0e7a5a; }}
  code {{ background:#edf2ef; padding:.1em .4em; border-radius:3px; font-size:.9em; }}
  .row {{ display:flex; gap:1rem; flex-wrap:wrap; margin:1rem 0; }}
  a.btn {{ display:inline-block; padding:.6rem 1.4rem; background:#0e7a5a; color:#fff;
           text-decoration:none; border-radius:4px; }}
  .muted {{ color:#5c6b65; font-size:.92rem; }}
</style>
<main>
  <div class="mark">穿云</div>
  <h1>下载客户端</h1>
  <p class="muted">装好之后输入管理员发给你的凭证就能用，不需要填服务器地址。</p>

  <div class="row">
    <a class="btn" href="/download/chuanyun-mac.dmg">macOS</a>
    <a class="btn" href="/download/chuanyun-windows.msi">Windows</a>
  </div>

  <h2>第一次打开</h2>
  <p>这个客户端没有购买代码签名证书，所以系统会拦一下：</p>
  <ul>
    <li><b>macOS</b>：在应用图标上<b>右键</b> → 打开 → 再点「打开」。直接双击会被拦住。</li>
    <li><b>Windows</b>：SmartScreen 提示时点「更多信息」→「仍要运行」。</li>
  </ul>
  <p class="muted">这两步每台机器只需要做一次。</p>

  <h2>服务器信息</h2>
  <p class="muted">
    隧道域名后缀：<code>{}</code><br>
    证书指纹：<code>{}</code>
  </p>
</main>
</html>"#,
        s.domain_suffix, s.fingerprint
    );

    ([("content-type", "text/html; charset=utf-8")], html).into_response()
}

struct AdminError(crate::store::StoreError);

impl From<crate::store::StoreError> for AdminError {
    fn from(e: crate::store::StoreError) -> Self {
        Self(e)
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "管理接口出错");
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn state() -> AdminState {
        AdminState {
            store: Store::in_memory().unwrap(),
            registry: Arc::new(Registry::new()),
            fingerprint: "abc123".into(),
            domain_suffix: "t.example.com".into(),
        }
    }

    fn app() -> Router {
        Router::new()
            .route("/status", get(status))
            .layer(axum::middleware::from_fn(guard_local_only))
            .with_state(state())
    }

    async fn call(req: HttpRequest<Body>) -> StatusCode {
        app().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn command_line_access_works() {
        let req = HttpRequest::builder()
            .uri("/status")
            .header("host", "127.0.0.1:7001")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(req).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn browser_requests_are_refused() {
        // 恶意网页里的 fetch 会带 Origin——绑回环挡不住它，这道闸才行
        for header in ["origin", "sec-fetch-site", "sec-fetch-mode"] {
            let req = HttpRequest::builder()
                .uri("/status")
                .header("host", "127.0.0.1:7001")
                .header(header, "https://evil.example.com")
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                call(req).await,
                StatusCode::FORBIDDEN,
                "带 {header} 的请求应被拒"
            );
        }
    }

    #[tokio::test]
    async fn dns_rebinding_is_refused() {
        // 攻击者把 evil.example.com 解析到 127.0.0.1，但 Host 头露了馅
        let req = HttpRequest::builder()
            .uri("/status")
            .header("host", "evil.example.com")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(req).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn localhost_variants_are_accepted() {
        for host in [
            "127.0.0.1:7001",
            "localhost:7001",
            "localhost",
            "[::1]:7001",
        ] {
            let req = HttpRequest::builder()
                .uri("/status")
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            assert_eq!(call(req).await, StatusCode::OK, "{host} 应被接受");
        }
    }
}
