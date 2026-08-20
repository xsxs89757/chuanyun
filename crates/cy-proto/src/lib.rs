//! 穿云协议层。
//!
//! 本 crate 只放**定义**，不做 IO：控制消息、数据流头、错误码、命名规则、版本常量。
//! 服务端（`cy-server`）与客户端（`cy-core`）各自实现 IO，共用这里的一套语义——
//! 两者之间没有代码依赖，只通过这份协议对话。
//!
//! # 一条连接长什么样
//!
//! ```text
//! TCP ─▶ TLS 1.3 ─▶ yamux
//!                    ├─ 流 0：控制流，JSON Lines（control 模块）
//!                    └─ 流 N：数据流，一行流头 + 裸字节（stream 模块）
//! ```
//!
//! 客户端**主动出站**连接服务端，所有隧道流量复用这一条连接——这样穿公司防火墙
//! 和 NAT 都不用额外配置，服务端也只需要开一个控制端口。

pub mod control;
pub mod error;
pub mod naming;
pub mod stream;

#[cfg(feature = "codec")]
pub mod codec;

pub use control::{ClientMsg, Endpoint, ServerMsg, TunnelKind};
pub use stream::StreamHeader;

/// 协议版本。客户端在 `hello` 中上报，服务端不认识则拒绝连接。
///
/// 加字段、加消息类型都是兼容变更（见 [`control`] 模块的约定），不用动这个号；
/// 只有语义变了——同一个字段换含义、握手顺序改了——才递增。
pub const PROTO_VERSION: u32 = 1;
