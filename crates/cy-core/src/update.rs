//! 检查有没有新版本。
//!
//! 版本从 **GitHub Releases** 拉——CI 打 tag 时已经把三个平台的安装包传上去了，
//! 那里本来就是最新版所在的地方，不必再单独维护一份版本清单。
//!
//! 内部部署如果不想让客户端连外网，把品牌配置里的 `update_url` 指向自己的
//! 服务端即可（服务端的管理接口提供了同样形状的 `/api/client/version`）。
//!
//! 这里只**告诉**用户有新版，不自动下载安装。安装包没有代码签名，
//! 静默替换一个未签名的二进制不是好主意；给个链接让用户自己点更稳妥。

use std::time::Duration;

use serde::Deserialize;

/// 查到的新版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// 版本号，已去掉 `v` 前缀
    pub version: String,
    /// 给用户打开的页面
    pub url: String,
    pub notes: String,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("没有配置更新地址")]
    Disabled,
    #[error("查询更新失败: {0}")]
    Request(String),
    #[error("看不懂对方返回的内容")]
    BadResponse,
}

/// 查一下有没有比当前版本更新的版本。
///
/// 返回 `Ok(None)` 表示已经是最新的。
pub async fn check(update_url: &str, current: &str) -> Result<Option<Release>, UpdateError> {
    if update_url.trim().is_empty() {
        return Err(UpdateError::Disabled);
    }

    let release = fetch(&api_url(update_url)).await?;
    if is_newer(&release.version, current) {
        Ok(Some(release))
    } else {
        Ok(None)
    }
}

/// 把用户填的地址换算成能查版本的接口地址。
///
/// 允许直接写仓库主页——那是大家会顺手复制的东西，让他们再去查 API 路径
/// 没有必要。
pub fn api_url(configured: &str) -> String {
    let trimmed = configured.trim().trim_end_matches('/');

    // 已经是 API 地址或自建服务端，原样使用
    if trimmed.contains("api.github.com") || !trimmed.contains("github.com") {
        return trimmed.to_string();
    }

    // https://github.com/owner/repo[/releases…] → API
    if let Some(path) = trimmed
        .split("github.com/")
        .nth(1)
        .map(|p| p.trim_end_matches("/releases"))
    {
        let mut parts = path.split('/');
        if let (Some(owner), Some(repo)) = (parts.next(), parts.next()) {
            if !owner.is_empty() && !repo.is_empty() {
                return format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
            }
        }
    }
    trimmed.to_string()
}

/// GitHub 的 release 结构；我们只要其中三个字段。
///
/// 自建服务端返回 `{"version": "...", "notes": "..."}` 也能吃下——
/// 两种形状在这里合并处理，省得为内部部署再写一套。
#[derive(Debug, Deserialize)]
struct RawRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    notes: String,
}

/// 把 HTTP 状态码翻成用户看得懂的话。
///
/// 403 十有八九是限流：GitHub 对未认证请求按来源 IP 算 60 次/小时，
/// 一间公司共用一个出口 IP 很容易撞上。甩一个「403 Forbidden」出去，
/// 用户只会以为是自己没权限。
fn status_message(status: reqwest::StatusCode) -> String {
    match status {
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS => {
            "GitHub 暂时不让查了（同一个出口 IP 查得太频繁），过一会儿再试".to_string()
        }
        reqwest::StatusCode::NOT_FOUND => "查不到发布信息，更新地址可能配错了".to_string(),
        other => format!("对方返回 {other}"),
    }
}

async fn fetch(url: &str) -> Result<Release, UpdateError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        // GitHub 要求带 User-Agent，不带会直接 403
        .user_agent(concat!("chuanyun/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| UpdateError::Request(e.to_string()))?;

    let response = client
        .get(url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| UpdateError::Request(e.to_string()))?;

    if !response.status().is_success() {
        return Err(UpdateError::Request(status_message(response.status())));
    }

    let raw: RawRelease = response
        .json()
        .await
        .map_err(|_| UpdateError::BadResponse)?;

    let version = pick(&raw.tag_name, &raw.version);
    if version.is_empty() {
        return Err(UpdateError::BadResponse);
    }

    Ok(Release {
        version: version.trim_start_matches('v').to_string(),
        url: if raw.html_url.is_empty() {
            url.to_string()
        } else {
            raw.html_url
        },
        notes: pick(&raw.body, &raw.notes),
    })
}

fn pick(a: &str, b: &str) -> String {
    if a.is_empty() { b } else { a }.to_string()
}

/// `candidate` 是不是比 `current` 新。
///
/// 只比数字段，忽略 `-beta` 之类的后缀——预发布版本我们本来也不推给用户。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    parts(candidate) > parts(current)
}

fn parts(v: &str) -> (u32, u32, u32) {
    let core = v.trim_start_matches('v');
    // 去掉 -beta.1 / +build 这类后缀
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut it = core.split('.').map(|p| p.parse().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn rate_limit_reads_like_a_sentence_not_a_status_code() {
        // 实测撞到过：一间公司共用出口 IP，未认证请求 60 次/小时很容易用完
        let msg = super::status_message(reqwest::StatusCode::FORBIDDEN);
        assert!(msg.contains("过一会儿再试"), "应给出下一步动作: {msg}");
        assert!(!msg.contains("403"), "不该把状态码甩给用户: {msg}");

        assert_eq!(
            super::status_message(reqwest::StatusCode::TOO_MANY_REQUESTS),
            msg,
            "429 和 403 是同一件事，说法要一致"
        );
    }

    #[test]
    fn unexpected_status_still_says_something() {
        let msg = super::status_message(reqwest::StatusCode::BAD_GATEWAY);
        assert!(msg.contains("502"), "没预料到的状态码就把它原样带上: {msg}");
    }

    use super::*;

    #[test]
    fn repo_home_page_is_turned_into_an_api_url() {
        // 用户会顺手复制仓库主页地址，别让他们再去查 API 路径
        let expected = "https://api.github.com/repos/xsxs89757/chuanyun/releases/latest";
        for input in [
            "https://github.com/xsxs89757/chuanyun",
            "https://github.com/xsxs89757/chuanyun/",
            "https://github.com/xsxs89757/chuanyun/releases",
        ] {
            assert_eq!(api_url(input), expected, "输入：{input}");
        }
    }

    #[test]
    fn api_urls_and_self_hosted_are_left_alone() {
        let api = "https://api.github.com/repos/a/b/releases/latest";
        assert_eq!(api_url(api), api);

        // 内部部署指向自己的服务端
        let own = "https://tunnel.example.com/chuanyun/api/client/version";
        assert_eq!(api_url(own), own);
    }

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"), "一样就不算新");
        assert!(!is_newer("0.1.0", "0.2.0"), "旧版本不该提示更新");
        // tag 带不带 v 都要算对
        assert!(!is_newer("v0.1.0", "0.1.0"));
    }

    #[test]
    fn prerelease_suffixes_do_not_confuse_the_comparison() {
        // 0.2.0-beta 的数字段就是 0.2.0，比 0.1.0 新
        assert!(is_newer("0.2.0-beta.1", "0.1.0"));
        // 但不该比正式的 0.2.0 更新
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
    }

    #[test]
    fn malformed_versions_do_not_panic() {
        for v in ["", "abc", "1", "1.2", "....", "v"] {
            let _ = is_newer(v, "0.1.0");
            let _ = is_newer("0.1.0", v);
        }
    }

    #[tokio::test]
    async fn empty_url_means_disabled() {
        assert!(matches!(
            check("", "0.1.0").await,
            Err(UpdateError::Disabled)
        ));
        assert!(matches!(
            check("   ", "0.1.0").await,
            Err(UpdateError::Disabled)
        ));
    }

    /// 用一个假的 GitHub 接口验整条链路。
    #[tokio::test]
    async fn reports_a_newer_release() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/releases/latest",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "tag_name": "v0.9.0",
                        "html_url": "https://github.com/a/b/releases/tag/v0.9.0",
                        "body": "修了几个 bug"
                    }))
                }),
            );
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/releases/latest");
        let release = check(&url, "0.1.0").await.unwrap().expect("应当发现新版本");
        assert_eq!(release.version, "0.9.0");
        assert!(release.url.contains("v0.9.0"));
        assert_eq!(release.notes, "修了几个 bug");

        // 已经是最新的时候不该提示
        assert_eq!(check(&url, "0.9.0").await.unwrap(), None);
        assert_eq!(check(&url, "1.0.0").await.unwrap(), None);
    }

    /// 自建服务端返回的是 `{"version": ...}`，同样要认。
    #[tokio::test]
    async fn accepts_the_self_hosted_shape() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/api/client/version",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({ "version": "0.5.0", "notes": "内部版" }))
                }),
            );
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/api/client/version");
        let release = check(&url, "0.1.0").await.unwrap().unwrap();
        assert_eq!(release.version, "0.5.0");
        assert_eq!(release.notes, "内部版");
    }

    #[tokio::test]
    async fn network_failures_are_reported_not_panicked() {
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = check(&format!("http://127.0.0.1:{dead}/x"), "0.1.0")
            .await
            .unwrap_err();
        assert!(matches!(err, UpdateError::Request(_)));
    }
}
