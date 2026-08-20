//! 穿云客户端核心库。
//!
//! 隧道引擎与 UI 无关：桌面端（`cy-desktop`）、示例 headless 壳、未来的 CLI
//! 都只是这个库的壳。库内不做任何界面假设，对外通过事件流和命令 API 交互。

pub use cy_proto::PROTO_VERSION;
