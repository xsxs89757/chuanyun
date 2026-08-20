# 第三方组件

穿云本身采用 [Apache-2.0](LICENSE)。依赖里有一项需要单独说明。

## Slint

桌面客户端的界面用 [Slint](https://slint.dev) 构建。Slint 提供三种许可：GPLv3、Royalty-Free、商业授权，使用者三选一。

**本项目选择 Royalty-Free 许可**（Slint Royalty-Free Desktop, Mobile, and Web Applications License）。它允许免费分发桌面应用（含商用），条件是做出归属声明——二选一：

- 在应用的「关于」对话框里展示 `AboutSlint` 组件，或
- 在提供应用下载的公开网页上展示 MadeWithSlint 徽章

我们两样都做了：关于页里有 `AboutSlint` 组件（[`crates/cy-desktop/ui/app.slint`](crates/cy-desktop/ui/app.slint)），README 里有徽章。

几点澄清，免得误读：

- 仓库源码本身仍然是 Apache-2.0。第三方从源码构建时，可以自行在 Slint 的三种许可中另作选择（比如走 GPLv3）。
- 该许可允许把修改过的 Slint 作为应用的一部分分发；不允许的是把 Slint 单独拿出来分发，或移除其许可声明。
- 该许可含一条条款：授权 SixtyFPS GmbH 将你的应用及其 logo 用于官网和宣传引用。对开源项目通常无所谓，但如果你 fork 出去做闭源内部版，值得先知道有这一条。

发布前建议按 [Slint 官方当期条款](https://slint.dev/terms-and-conditions) 复核一遍——条款偶有更新。

## 其余依赖

其余依赖都是 MIT / Apache-2.0 / BSD / ISC 一类的宽松许可。[`deny.toml`](deny.toml) 里列了允许的许可清单，CI 每次都会跑 `cargo deny check` 卡住不符合的依赖。

想看完整清单：

```bash
cargo install cargo-deny
cargo deny list
```
