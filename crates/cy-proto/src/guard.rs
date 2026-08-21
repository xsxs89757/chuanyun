//! 本地接口的访问判定。
//!
//! 客户端的本地 API 和服务端的管理接口都只监听回环地址，但**绑回环挡不住浏览器**：
//! 任何网页里的 JS 都能 `fetch("http://127.0.0.1:7075/...")`，配上 DNS rebinding
//! 甚至能读到响应——那就等于把用户的本地端口交给了一个网页。
//!
//! 两个接口的判定逻辑必须一模一样，所以放在这里共享：安全规则在两处各写一遍，
//! 迟早会有一处漏掉某个补丁。
//!
//! # 怎么区分「浏览器」和「脚本」
//!
//! 靠 `Origin` 和 `Sec-Fetch-Site`：
//!
//! - 网页发起的跨源 `fetch` 一定带 `Origin`，也一定带 `Sec-Fetch-Site: cross-site`；
//! - 命令行工具和脚本不带这两个头。
//!
//! **不能拿 `Sec-Fetch-Mode` 当依据**——Node 的 `fetch` 会发 `sec-fetch-mode: cors`，
//! 按它判断会把正经脚本（包括我们自己的 vite 插件）全部挡在门外。

/// 这个请求看起来是网页发起的吗？
///
/// `header` 按名字取头（名字已小写）。
pub fn looks_like_browser<'a>(header: impl Fn(&str) -> Option<&'a str>) -> bool {
    // 跨源请求必带 Origin；脚本不会自己加这个头
    if header("origin").is_some() {
        return true;
    }

    // Sec-Fetch-Site 只有浏览器会发。`none` 表示用户直接在地址栏输入，
    // 那个也是浏览器，一并拦下。
    if header("sec-fetch-site").is_some() {
        return true;
    }

    false
}

/// Host 头是不是回环地址。
///
/// 挡的是 DNS rebinding：攻击者把自己的域名解析到 127.0.0.1，请求就"来自本机"了，
/// 但 Host 头里还留着他的域名。
pub fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else {
        // HTTP/1.1 要求必须有 Host。没有的多半不是正经客户端。
        return false;
    };
    let name = crate::naming::host_without_port(host);
    matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn check(pairs: &[(&str, &str)]) -> bool {
        let map = headers(pairs);
        // 这里刻意用一个拿不到引用生命周期的写法来模拟真实调用
        let leaked: HashMap<String, &'static str> = map
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    Box::leak(v.clone().into_boxed_str()) as &'static str,
                )
            })
            .collect();
        looks_like_browser(|name| leaked.get(name).copied())
    }

    #[test]
    fn browser_fetch_is_detected() {
        // 恶意网页里的 fetch
        assert!(check(&[
            ("origin", "https://evil.example.com"),
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "cors"),
        ]));
        // 只有 Origin 也算
        assert!(check(&[("origin", "https://evil.example.com")]));
        // 地址栏直接访问
        assert!(check(&[("sec-fetch-site", "none")]));
    }

    /// 这条守着一个真踩过的坑：Node 的 fetch 会发 `sec-fetch-mode: cors`。
    ///
    /// 早先把这个头当成浏览器特征，结果所有 Node 脚本都被挡在门外——
    /// 包括我们自己的 vite 插件。「脚本能调本地 API」是这套接口存在的理由，
    /// 挡掉它等于功能全废。
    #[test]
    fn node_fetch_is_not_a_browser() {
        assert!(
            !check(&[
                ("host", "127.0.0.1:7075"),
                ("accept", "*/*"),
                ("sec-fetch-mode", "cors"),
                ("user-agent", "node"),
            ]),
            "Node 的 fetch 带 sec-fetch-mode，但它不是浏览器"
        );
    }

    #[test]
    fn command_line_tools_pass() {
        assert!(!check(&[
            ("host", "127.0.0.1:7075"),
            ("user-agent", "curl/8.4.0")
        ]));
        assert!(!check(&[("host", "localhost:7075")]));
    }

    #[test]
    fn loopback_hosts_are_recognised() {
        for h in [
            "127.0.0.1",
            "127.0.0.1:7075",
            "localhost",
            "localhost:7075",
            "[::1]:7075",
        ] {
            assert!(host_is_loopback(Some(h)), "{h} 应当算回环");
        }
    }

    #[test]
    fn rebinding_attempts_are_rejected() {
        // 攻击者把 evil.example.com 解析到 127.0.0.1，Host 头露了馅
        assert!(!host_is_loopback(Some("evil.example.com")));
        assert!(!host_is_loopback(Some("evil.example.com:7075")));
        assert!(!host_is_loopback(None));
    }
}
