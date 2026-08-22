<div align="center">

<img src="docs/img/logo.svg" alt="穿云" width="280">

**把本机端口变成一个能填进微信后台的固定 HTTPS 地址。**

[English](README.md) · [中文](README.zh-CN.md)

[![CI](../../actions/workflows/ci.yml/badge.svg)](../../actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Made with Slint](https://img.shields.io/badge/Made%20with-Slint-2379f4)](https://slint.dev)
[![npm](https://img.shields.io/npm/v/vite-plugin-chuanyun?label=vite-plugin-chuanyun)](https://www.npmjs.com/package/vite-plugin-chuanyun)

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

## 安装

### 服务端（Linux，x86_64）

一条命令：

```bash
curl -fsSL https://github.com/xsxs89757/chuanyun/releases/latest/download/install-server.sh \
  | sudo sh -s -- --domain t.example.com
```

`t.example.com` 换成你打算给隧道用的子域。脚本会下载最新版、核对校验和、装成 systemd 服务，
然后把**接下来该做的四件事**（泛解析、放行一个端口、nginx 反代、发第一个凭证）直接打在屏幕上。

以后再跑一次就是升级，配置和数据都留着。

不放心把脚本管道进 root shell 的话，先读一遍——就一个文件，
[`scripts/install-server.sh`](scripts/install-server.sh)——或者照[手动步骤](docs/部署.md#手动安装)来。

### 给同事发凭证

装完之后在服务器上。**用户名用拼音**（小写字母、数字、连字符）——它会成为隧道域名的一部分，
`zhangsan-wx.t.example.com`，DNS 里放不下汉字：

```bash
chuanyun-server user add zhangsan    # 输出凭证，只显示这一次
chuanyun-server fingerprint          # 证书指纹，和凭证一起发给本人
```

同事在客户端填三样：服务器地址、凭证、指纹（指纹可留空，首次连接会弹窗确认）。

```bash
chuanyun-server user list            # 谁有凭证
chuanyun-server user reissue zhangsan   # 凭证丢了：换一张，旧的立刻失效
chuanyun-server user revoke zhangsan    # 离职：吊销，在线连接立刻断
chuanyun-server status               # 谁在线、几条隧道
```

更多（升级、证书、排查）见[运维速查](docs/运维.md)。

### 桌面客户端（Windows、macOS）

到 [最新发布](../../releases/latest) 下载。安装包**没有代码签名**，第一次打开要多一步，每台机器做一次：

- **macOS**：双击会弹「Apple 无法验证…」，点**完成**（*别点*「移到废纸篓」），
  然后到 系统设置 → 隐私与安全性 → 拉到最底下，点**仍要打开**。嫌麻烦就一行命令：
  `xattr -dr com.apple.quarantine /Applications/穿云.app`
  （网上常见的「右键 → 打开」从 macOS 15 起已经不管用了。）
- **Windows**：SmartScreen 提示时点「更多信息」→「仍要运行」。

## 没有服务器也能先看看

```bash
git clone https://github.com/xsxs89757/chuanyun.git && cd chuanyun
./scripts/demo.sh
```

这个脚本在本机起一整套跑通全链路——不需要域名、nginx 或公网服务器。

## 特点

- **固定子域名**：`{用户}-{隧道名}.t.你的域名`，重启不变，可以放心填进微信/支付后台
- **零配置文件**：服务器地址可以编译进安装包（见[品牌注入](brand/README.md)），同事只需要一个凭证
- **每人独立凭证**：可签发、可吊销、可审计，离职即断，不是一个大家共享的 token
- **断线自己回来**：网络抖动、服务端重启、笔记本合盖之后隧道自动恢复，地址不变
- **本地 API**：项目脚本可以动态注册端口；业务代码用一个接口拿到「隧道开着就返回公网地址、关着就返回本地地址」，一份代码两种环境
- **纯 Rust 桌面端**：不依赖系统 WebView，单个二进制，Windows 与 macOS 通用
- **请求观测与重放**：穿过隧道的请求留一份在本地，可以拿同一份报文反复重打——支付回调只推几次，这是唯一能反复调的办法
- **接入同事的服务**：反过来，把同事的隧道映射成你本地的端口，代码里照常写 `127.0.0.1`
- **协议只做该做的**：HTTP(S) 与 TCP，传输走 TCP + TLS。不做 UDP、KCP、P2P 打洞——面小才好维护

## 文档

| | |
|---|---|
| [部署服务端](docs/部署.md) | 域名解析、nginx 接入、发凭证、排查 |
| [怎么用](docs/使用.md) | 开隧道、调微信回调、项目接入、本地 API |
| [运维速查](docs/运维.md) | 日常命令：发凭证、升级、证书、排查 |
| [品牌注入](brand/README.md) | 怎么打出「同事装上就能登录」的内部版 |
| [Vite 插件](integrations/vite-plugin-chuanyun/GUIDE.zh-CN.md) | 前端项目一行接入 |
| [发版](docs/发布.md) | 打 tag、npm 发布、更新检查怎么走 |
| [参与开发](CONTRIBUTING.md) | 仓库结构、约束、测试怎么写 |

## 现状

功能齐了：HTTP(S) 与 TCP 隧道、桌面客户端（Windows / macOS）、自动重连、
请求观测与重放、访问口令、接入同事的服务、本地 API、Vite 插件、
用户与凭证管理、审计。

还没做：Web 管理台、私有互连（点对点、不经公网暴露）、QUIC 传输。

## 仓库结构

| 目录 | 内容 |
|---|---|
| `crates/cy-proto` | 协议定义：控制消息、错误码、命名规则。不做 IO |
| `crates/cy-core` | 客户端核心库：连接、重连、隧道引擎、本地 API。与界面无关 |
| `crates/cy-server` | 服务端：控制通道、HTTP 入口、管理接口 |
| `crates/cy-desktop` | 桌面客户端（Slint），是 `cy-core` 的薄壳 |
| `crates/cy-e2e` | 端到端测试 |
| `integrations/` | 周边接入：Vite 插件 |

`cy-core` 和 `cy-server` 互不依赖，只共用 `cy-proto`。

## 许可

本项目采用 [Apache-2.0](LICENSE)。

界面使用 [Slint](https://slint.dev)。我们选择其 Royalty-Free 许可分发桌面应用，归属声明展示在应用的「关于」页。第三方从源码构建时，可自行在 Slint 的 GPLv3 / Royalty-Free / 商业许可中作出选择。详见 [THIRD-PARTY.md](THIRD-PARTY.md)。
