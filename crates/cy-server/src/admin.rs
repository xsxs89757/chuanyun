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

use axum::extract::{Path, Request, State};
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
    /// 客户端安装包放在哪。下载页只列出这里实际存在的文件。
    pub download_dir: std::path::PathBuf,
    /// 展示给客户端填的控制通道地址。
    pub control_addr: String,
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AdminState,
    shutdown: CancellationToken,
) {
    // 下载页和版本接口本来就该让浏览器访问：前者是同事拿安装包的地方，
    // 后者是客户端查更新用的。套上 guard_local_only 它们就永远打不开——
    // 浏览器必发 Sec-Fetch-Site，经 nginx 反代后 Host 也不是回环地址。
    // 两个都只读、没有副作用，页面上的证书指纹与域名后缀本来就是要发给同事的。
    let public = Router::new()
        .route("/api/client/version", get(client_version))
        .route("/download", get(download_page))
        .route("/download/{file}", get(download_file))
        .with_state(state.clone());

    // 其余接口能踢人、能列用户和审计，仍然只许本机用命令行调。
    let private = Router::new()
        .route("/status", get(status))
        .route("/tunnels", get(tunnels))
        .route("/users", get(users))
        .route("/kick/{user}", post(kick))
        .route("/audit", get(audit))
        .layer(axum::middleware::from_fn(guard_local_only))
        .with_state(state);

    let app = public.merge(private);

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
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());

    // 绑回环挡不住网页里的 JS，所以另外看请求像不像浏览器发的。
    // 判定逻辑与服务端管理接口共用一份（cy_proto::guard），
    // 安全规则各写一遍迟早有一处漏补丁。
    if cy_proto::guard::looks_like_browser(get) {
        return (
            StatusCode::FORBIDDEN,
            "管理接口不接受来自浏览器的请求。请用命令行工具访问。\n",
        )
            .into_response();
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
    let packages = available_packages(&s.download_dir);
    let buttons = if packages.is_empty() {
        // 与其摆两个点了 404 的按钮，不如直接说安装包还没放上来
        format!(
            "<p class=\"muted\">安装包还没放上来。管理员把 .dmg / .msi 放进 \
             <code>{}</code> 就会出现在这里。</p>",
            s.download_dir.display()
        )
    } else {
        packages
            .iter()
            .map(|(label, name)| format!("<a class=\"btn\" href=\"download/{name}\">{label}</a>"))
            .collect::<Vec<_>>()
            .join("\n    ")
    };
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
  <p class="muted">装好之后打开，按下面「登录要填什么」把三项填进去。</p>

  <div class="row">{}</div>

  <h2>第一次打开</h2>
  <p>这个客户端没有购买代码签名证书，所以系统会拦一下：</p>
  <ul>
    <li><b>macOS</b>：双击会弹「Apple 无法验证…」，点<b>完成</b>（别点「移到废纸篓」），再到 <b>系统设置 → 隐私与安全性</b>，拉到最底下点 <b>仍要打开</b>。
        嫌麻烦就在终端里跑 <code>xattr -dr com.apple.quarantine /Applications/穿云.app</code>。
        （网上常见的「右键 → 打开」从 macOS 15 起已经不管用了。）</li>
    <li><b>Windows</b>：SmartScreen 提示时点「更多信息」→「仍要运行」。</li>
  </ul>
  <p class="muted">这两步每台机器只需要做一次。</p>

  <h2>登录要填什么</h2>
  <p class="muted">
    <b>服务器</b>：<code>{}</code><br>
    <b>凭证</b>：管理员单独发给你的那串 <code>cy_...</code>。只发一次，别弄丢——
    丢了找管理员用 <code>user reissue</code> 重发。<br>
    <b>证书指纹</b>：<code>{}</code><br>
    <span style="font-size:.9em">指纹这栏也可以留空：留空就是首次连接时弹窗让你确认，确认后记住。
    如果装的是公司内部版，服务器和指纹都已经预置好了，只填凭证即可。</span>
  </p>
  <p class="muted">开出来的地址长这样：<code>你的名字-隧道名.{}</code>，固定不变。</p>
</main>
</html>"#,
        buttons, s.control_addr, s.fingerprint, s.domain_suffix
    );

    ([("content-type", "text/html; charset=utf-8")], html).into_response()
}

/// 下载目录里现有的安装包。
///
/// 只认后缀，不递归：这个目录是管理员手动放安装包的地方，不是通用文件服务器。
fn available_packages(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let label = match name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
        {
            Some(ext) if ext == "dmg" => "macOS",
            Some(ext) if ext == "msi" || ext == "exe" => "Windows",
            _ => continue,
        };
        out.push((label.to_string(), name));
    }
    // 文件名排序，保证每次刷新顺序一致
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// 提供下载目录里的一个文件。
///
/// 只接受纯文件名：带路径分隔符或 `..` 的一律拒绝，否则这就是一个
/// 任意文件读取漏洞（`/download/..%2f..%2fetc%2fpasswd`）。
async fn download_file(State(s): State<AdminState>, Path(file): Path<String>) -> Response {
    if file.is_empty()
        || file.contains('/')
        || file.contains('\\')
        || file.contains("..")
        || file.starts_with('.')
    {
        return (StatusCode::BAD_REQUEST, "文件名不合法\n").into_response();
    }
    // 再核对一遍：只发这个目录里我们愿意列出来的那些文件
    if !available_packages(&s.download_dir)
        .iter()
        .any(|(_, name)| name == &file)
    {
        return (StatusCode::NOT_FOUND, "没有这个安装包\n").into_response();
    }

    let path = s.download_dir.join(&file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                ("content-type", "application/octet-stream".to_string()),
                (
                    "content-disposition",
                    format!("attachment; filename=\"{file}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "下载页读文件失败");
            (StatusCode::NOT_FOUND, "没有这个安装包\n").into_response()
        }
    }
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
        state_with_downloads(std::path::PathBuf::from("/nonexistent-downloads"))
    }

    fn state_with_downloads(dir: std::path::PathBuf) -> AdminState {
        AdminState {
            store: Store::in_memory().unwrap(),
            registry: Arc::new(Registry::new()),
            fingerprint: "abc123".into(),
            domain_suffix: "t.example.com".into(),
            download_dir: dir,
            control_addr: "tunnel.example.com:7000".into(),
        }
    }

    fn real_app_with(st: AdminState) -> Router {
        let public = Router::new()
            .route("/api/client/version", get(client_version))
            .route("/download", get(download_page))
            .route("/download/{file}", get(download_file))
            .with_state(st.clone());
        let private = Router::new()
            .route("/status", get(status))
            .route("/tunnels", get(tunnels))
            .route("/users", get(users))
            .route("/kick/{user}", post(kick))
            .route("/audit", get(audit))
            .layer(axum::middleware::from_fn(guard_local_only))
            .with_state(st);
        public.merge(private)
    }

    async fn body_of(app: Router, uri: &str) -> (StatusCode, String) {
        let req = HttpRequest::builder()
            .uri(uri)
            .header("host", "t.example.com")
            .header("sec-fetch-site", "none")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn app() -> Router {
        Router::new()
            .route("/status", get(status))
            .layer(axum::middleware::from_fn(guard_local_only))
            .with_state(state())
    }

    /// 和 `serve()` 里一模一样的路由组装。测公开/私有的划分必须用这个，
    /// 用上面那个简化版会把「下载页有没有被 guard 挡住」这件事测漏。
    fn real_app() -> Router {
        real_app_with(state())
    }

    async fn call_real(req: HttpRequest<Body>) -> StatusCode {
        real_app().oneshot(req).await.unwrap().status()
    }

    /// 下载页是给同事用浏览器打开的。它曾经和 /kick 共用同一道 guard，
    /// 结果浏览器永远 403、经 nginx 反代也 403——真机部署时才发现。
    #[tokio::test]
    async fn download_page_opens_in_a_browser_through_nginx() {
        for uri in ["/download", "/api/client/version"] {
            let req = HttpRequest::builder()
                .uri(uri)
                // 浏览器必发这两个头之一；Host 是 nginx 透传过来的公网域名
                .header("host", "t.example.com")
                .header("sec-fetch-site", "none")
                .header("user-agent", "Mozilla/5.0")
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                call_real(req).await,
                StatusCode::OK,
                "{uri} 应该能在浏览器里打开"
            );
        }
    }

    /// 下载页的按钮曾经写死指向 /download/chuanyun-mac.dmg，而根本没有路由
    /// 提供这些文件——两个按钮点下去都是 404。现在改成列出目录里真有的包。
    #[tokio::test]
    async fn download_page_lists_only_packages_that_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("chuanyun-0.1.0-macos-universal.dmg"),
            b"dmg",
        )
        .unwrap();
        std::fs::write(dir.path().join("readme.txt"), "不是安装包").unwrap();

        let st = state_with_downloads(dir.path().to_path_buf());
        let (code, html) = body_of(real_app_with(st), "/download").await;
        assert_eq!(code, StatusCode::OK);
        assert!(
            html.contains("chuanyun-0.1.0-macos-universal.dmg"),
            "应列出 dmg"
        );
        assert!(!html.contains("readme.txt"), "非安装包不该出现");
        assert!(
            !html.contains("chuanyun-windows.msi"),
            "没有的包不该摆一个死链接"
        );
        // 相对路径，这样反代到 /chuanyun/download 这类前缀下也能用
        assert!(
            html.contains("href=\"download/chuanyun-0.1.0-macos-universal.dmg\""),
            "链接要用相对路径: {html}"
        );
    }

    /// 页面原来写着「不需要填服务器地址」，可开源版必须填；而「服务器信息」里
    /// 给了域名后缀和指纹，唯独没给要填的那个地址。同事照着页面根本登录不了。
    #[tokio::test]
    async fn download_page_tells_you_the_address_to_type() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_with_downloads(dir.path().to_path_buf());
        let (code, html) = body_of(real_app_with(st), "/download").await;
        assert_eq!(code, StatusCode::OK);
        assert!(html.contains("tunnel.example.com:7000"), "要写明填哪个地址");
        assert!(html.contains("abc123"), "指纹也要给");
        assert!(html.contains("t.example.com"), "隧道地址长什么样也说一下");
        assert!(
            !html.contains("不需要填服务器地址"),
            "这句只对内部版成立，不该无条件写出来"
        );
    }

    #[tokio::test]
    async fn download_page_says_so_when_nothing_is_uploaded_yet() {
        let dir = tempfile::tempdir().unwrap();
        let st = state_with_downloads(dir.path().to_path_buf());
        let (code, html) = body_of(real_app_with(st), "/download").await;
        assert_eq!(code, StatusCode::OK);
        assert!(html.contains("安装包还没放上来"), "该说清楚为什么没有按钮");
    }

    #[tokio::test]
    async fn packages_actually_download() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("chuanyun.dmg"), b"\x01\x02\x03").unwrap();
        let st = state_with_downloads(dir.path().to_path_buf());
        let (code, body) = body_of(real_app_with(st), "/download/chuanyun.dmg").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body.as_bytes(), b"\x01\x02\x03");
    }

    /// 下载接口是公开的，所以路径穿越会直接变成任意文件读取。
    #[tokio::test]
    async fn path_traversal_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.dmg"), b"ok").unwrap();
        for evil in [
            "../../../../etc/passwd",
            "..%2f..%2fetc%2fpasswd",
            "sub/dir.dmg",
            "..",
            ".hidden",
        ] {
            let st = state_with_downloads(dir.path().to_path_buf());
            let (code, body) = body_of(real_app_with(st), &format!("/download/{evil}")).await;
            // 具体是 400 还是 404 不重要（axum 归一化后有些根本匹配不到这条路由，
            // 会落到带 guard 的那组拿 403）——重要的是绝不能成功。
            assert!(!code.is_success(), "{evil} 应被拒，实际 {code}");
            assert!(!body.contains("root:"), "{evil} 读到了 /etc/passwd");
        }
    }

    /// 目录里有别的文件也不能顺手下走——只发列得出来的那几个安装包。
    #[tokio::test]
    async fn non_package_files_are_not_served() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secrets.env"), b"TOKEN=xxx").unwrap();
        let st = state_with_downloads(dir.path().to_path_buf());
        let (code, _) = body_of(real_app_with(st), "/download/secrets.env").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    /// 摘掉 guard 的只有那两条只读接口，能踢人的仍然只许本机命令行。
    #[tokio::test]
    async fn management_endpoints_stay_locked_down() {
        for (method, uri) in [
            ("GET", "/status"),
            ("GET", "/users"),
            ("GET", "/audit"),
            ("POST", "/kick/zhangsan"),
        ] {
            let req = HttpRequest::builder()
                .method(method)
                .uri(uri)
                .header("host", "t.example.com")
                .header("sec-fetch-site", "none")
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                call_real(req).await,
                StatusCode::FORBIDDEN,
                "{method} {uri} 不该让浏览器碰到"
            );
        }
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
        // 恶意网页里的 fetch 会带 Origin 和 Sec-Fetch-Site——绑回环挡不住它，
        // 这道闸才行。注意不含 sec-fetch-mode：Node 的 fetch 也发那个头。
        for header in ["origin", "sec-fetch-site"] {
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

    /// Node 的 fetch 会发 `sec-fetch-mode: cors`。
    ///
    /// 早先把这个头当浏览器特征，结果所有 Node 脚本都被挡在门外——
    /// 包括我们自己的 vite 插件。这条守着别再犯。
    #[tokio::test]
    async fn node_style_requests_are_allowed() {
        let req = HttpRequest::builder()
            .uri("/status")
            .header("host", "127.0.0.1:7001")
            .header("sec-fetch-mode", "cors")
            .header("user-agent", "node")
            .header("accept", "*/*")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            call(req).await,
            StatusCode::OK,
            "脚本调本地接口是这套 API 存在的理由，不能被挡"
        );
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
