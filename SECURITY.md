# 安全

## 报告漏洞

**请不要用公开 issue 报告安全问题。**

用 GitHub 的 [Security Advisory](../../security/advisories/new) 私下提交。我们会尽快回复。

## 值得重点看的地方

如果你在做代码审计，这几处最值得先看：

| 位置 | 为什么 |
|---|---|
| [`crates/cy-core/src/verifier.rs`](crates/cy-core/src/verifier.rs) | 服务端证书校验。自定义证书校验最经典的翻车方式是把验签方法写成"直接放行"——那样整套校验就作废了。文件顶部写了为什么必须委托给 rustls。 |
| [`crates/cy-server/src/ingress_http.rs`](crates/cy-server/src/ingress_http.rs) | HTTP 入口。按请求路由（而非按连接）是为了避免 nginx keepalive 下的请求串线；`X-Forwarded-For` 只在对端属于信任列表时才采信。 |
| [`crates/cy-server/src/admin.rs`](crates/cy-server/src/admin.rs) 与 [`crates/cy-core/src/local_api.rs`](crates/cy-core/src/local_api.rs) | 两个本地接口。绑回环挡不住浏览器，所以额外拒绝带浏览器特征头的请求，并校验 Host 防 DNS rebinding。 |
| [`crates/cy-server/src/store.rs`](crates/cy-server/src/store.rs) | 凭证只存 SHA-256；日志里只打前缀。 |

## 设计上的取舍

**隧道是公网暴露的。** 这是它的用途，不是缺陷。但用户需要知道：开着的时候任何知道地址的人都能访问到那个本地端口。客户端和文档里都写了这一点，V1.5 会加访问口令。

**子域名是可预测的**（`{用户名}-{隧道名}`）。这是产品决策——地址要能记住、能填进微信后台。代价是扫描器可能发现暴露的服务。补偿措施：未知子域名统一返回 404，不区分"从没存在过"和"刚关掉"，不给扫描者反馈。

**控制通道用自签证书 + 指纹校验**，不走 CA。服务端首次启动生成证书并打印指纹，客户端只认这一张。安全性等价于私有 CA，但省掉了"为了内网工具再申请一张证书"的麻烦。

## 支持范围

只维护最新版本。发现问题请先升级到最新再复现。
