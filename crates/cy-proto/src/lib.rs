//! 穿云协议层。
//!
//! 这里放两端共享的东西：默认只有**定义**（控制消息、数据流头、错误码、命名规则、
//! 版本常量），两个可选 feature 额外提供传输层的公共管道——`codec` 是控制流的
//! JSON Lines 编解码，`mux` 是 yamux 驱动。
//!
//! 服务端（`cy-server`）与客户端（`cy-core`）之间没有代码依赖，只通过这份协议对话；
//! 那些两端一模一样的管道代码放在这里共享，而不是各抄一份——并发代码抄两份的下场
//! 是某天在一边修了 bug、另一边没修。
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
pub mod guard;
pub mod naming;
pub mod stream;

#[cfg(feature = "codec")]
pub mod codec;

#[cfg(feature = "mux")]
pub mod mux;

pub use control::{ClientMsg, Endpoint, ServerMsg, TunnelKind};
pub use stream::StreamHeader;

/// 协议版本。客户端在 `hello` 中上报，服务端不认识则拒绝连接。
///
/// 加字段、加消息类型都是兼容变更（见 [`control`] 模块的约定），不用动这个号；
/// 只有语义变了——同一个字段换含义、握手顺序改了——才递增。
pub const PROTO_VERSION: u32 = 1;
