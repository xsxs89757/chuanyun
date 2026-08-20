<div align="center">

# 穿云

**把本机端口变成一个能填进微信后台的固定 HTTPS 地址。**

[![CI](https://github.com/chuanyun/chuanyun/actions/workflows/ci.yml/badge.svg)](https://github.com/chuanyun/chuanyun/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Made with Slint](https://img.shields.io/badge/Made%20with-Slint-2379f4)](https://slint.dev)

</div>

## 这是什么

穿云是一个内网穿透工具：桌面客户端把你本机的端口映射到自有服务器的域名下，外部（微信服务器、客户浏览器、联调的同事）就能访问到。

和 frp 解决同一类问题，区别在于**它假设有人替你部署好了服务端**：同事拿到安装包，输入一个凭证就能用，不需要理解 proxy 类型、vhost 端口、subdomain_host，也不需要维护任何配置文件。

```
微信服务器 ──HTTPS──▶ nginx:443 ──▶ 穿云服务端 ══隧道══▶ 你的电脑 ──▶ 127.0.0.1:8082
```

## 为什么会有这个项目

调试微信公众号、小程序、支付回调时，微信要求回调地址是**备案域名 + 公网 HTTPS**，本地服务没法直接调。已有的方案都不太合适：

- **frp** 功能齐全，但每个人都要手写配置文件、没有官方图形界面；认证是一个全局共享的 token，分不清谁是谁，也没法单独踢掉某个人。
- **ngrok 等 SaaS** 体验好，但数据经第三方、免费版是随机域名——而微信后台填的是具体子域名，一换就得重配。

如果你本来就有服务器和已备案域名，穿云让这件事变成：装上、登录、开一个隧道。

## 特点

- **固定子域名**：`{用户}-{隧道名}.t.你的域名`，重启不变，可以放心填进微信/支付后台
- **零配置文件**：服务器地址可以编译进安装包（见[品牌注入](brand/README.md)），同事只需要一个凭证
- **每人独立凭证**：可签发、可吊销、可审计，离职即断，不是一个大家共享的 token
- **本地 API**：项目启动脚本可以动态注册端口，新加的服务自动接入；业务代码用一个接口拿到"隧道开着就返回公网地址、关着就返回本地地址"
- **纯 Rust 桌面端**：不依赖系统 WebView，单个二进制，Windows 与 macOS 通用
- **协议只做该做的**：HTTP(S) 与 TCP，传输走 TCP + TLS。不做 UDP、KCP、P2P 打洞——面小才好维护

## 现状

早期开发中，尚未发布可用版本。进展见 [Issues](https://github.com/chuanyun/chuanyun/issues)。

## 构建

需要 Rust 1.92 及以上（Slint 的要求）。

```bash
cargo build --release
```

产物：`chuanyun-server`（服务端）与 `chuanyun`（桌面客户端）。

服务端部署与 nginx 接入见 [`deploy/`](deploy/)，其中 [`nginx.conf.example`](deploy/nginx.conf.example) 可以直接改改用。

## 仓库结构

| 目录 | 内容 |
|---|---|
| `crates/cy-proto` | 协议定义：控制消息、错误码、命名规则。不做 IO |
| `crates/cy-core` | 客户端核心库：连接管理、隧道引擎、重连、本地 API。与界面无关 |
| `crates/cy-server` | 服务端：控制通道、HTTP 入口、管理接口 |
| `crates/cy-desktop` | 桌面客户端（Slint），是 `cy-core` 的薄壳 |
| `crates/cy-e2e` | 端到端集成测试 |

## 许可

本项目采用 [Apache-2.0](LICENSE)。

界面使用 [Slint](https://slint.dev)。我们选择其 Royalty-Free 许可分发桌面应用，归属声明展示在应用的「关于」页。第三方从源码构建时，可自行在 Slint 的 GPLv3 / Royalty-Free / 商业许可中作出选择。详见 [THIRD-PARTY.md](THIRD-PARTY.md)。
