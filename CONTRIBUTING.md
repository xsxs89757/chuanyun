# 参与开发

## 起步

需要 Rust 1.92 以上（Slint 的要求）。

```bash
git clone <仓库地址>
cd chuanyun
cargo test --workspace
```

想看它实际怎么跑：

```bash
./scripts/demo.sh
```

这个脚本在本机起一整套（服务端 + 客户端 + 一个假的本地服务），跑通完整链路。不需要域名、nginx 或公网服务器。

## 仓库结构

```
crates/
  cy-proto/    协议定义：控制消息、错误码、命名规则。不做 IO
  cy-core/     客户端核心库：连接、重连、隧道引擎、本地 API。与界面无关
  cy-server/   服务端：控制通道、HTTP 入口、管理接口
  cy-desktop/  桌面客户端（Slint），是 cy-core 的薄壳
  cy-e2e/      端到端测试
```

一条重要约束：**cy-core 和 cy-server 互不依赖**，只通过 cy-proto 里的协议对话。两端都要的管道代码（yamux 驱动、JSON Lines 编解码）放在 cy-proto 里共享，别各抄一份——并发代码抄两份的下场是某天在一边修了 bug、另一边没修。

界面逻辑不要写进 cy-core。它得能在没有图形环境的 CI 上跑完整测试。

## 提交之前

```bash
cargo fmt --all
cargo clippy --workspace --all-targets   # 零警告
cargo test --workspace
```

CI 会跑同样这几条，外加 musl 交叉编译和 `cargo deny check`。

## 写测试

测试要说明**为什么**这条断言重要，而不只是断言了什么。比如：

```rust
// 保留字当用户名不行——会产出 admin-xxx 这种像官方的地址
assert_eq!(validate_user("admin"), Err(code::NAME_RESERVED));
```

端到端测试起真实的服务端和客户端（见 [`crates/cy-e2e/src/lib.rs`](crates/cy-e2e/src/lib.rs)），所有监听器都绑 `:0` 让内核分配端口——端口冲突导致的偶发失败是测试里最没意思的一类噪音。

时序断言用 `wait_for` 而不是 `sleep`：写短了偶发失败，写长了测试变慢。

## 代码风格

注释写**为什么**，不写代码本身已经说清楚的事。尤其是那些"看起来可以更简单，但不行"的地方——把不行的理由留下，否则下一个人（可能就是三个月后的你）会去简化它。

比如 [`ingress_http.rs`](crates/cy-server/src/ingress_http.rs) 顶部解释了为什么按请求路由而不是按连接：更省事的写法在 nginx keepalive 下会让请求串线。没有这段注释，这个决定看起来就只是绕远路。

用户能看到的文案一律用中文，而且要说人话——报错要告诉用户发生了什么、该怎么办，别把错误码直接甩出去。

## 验证脚本

单元测试之外还有三个跑真东西的脚本，改动核心链路后值得跑一遍：

```bash
./scripts/demo.sh              # 本机起全套，跑通完整链路
./scripts/verify-desktop.sh    # 桌面端：自动连接、恢复隧道、本地 API
./scripts/verify-vite-plugin.sh # 真 Vite 项目经隧道访问
```

它们不是摆设——`verify-vite-plugin.sh` 抓出过两个单元测试发现不了的 bug
（本地 API 把 Node 的 fetch 当浏览器挡掉了、Vite 只绑 IPv6 而数据面只连 IPv4）。

## 现在还没做的

Web 管理台、私有互连（点对点、不经公网暴露）、QUIC 传输。设计已经想过，见项目文档。
