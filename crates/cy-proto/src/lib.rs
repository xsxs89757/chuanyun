//! 穿云协议层。
//!
//! 本 crate 只放**定义**，不做 IO：控制消息、数据流头、错误码、命名规则、版本常量。
//! 服务端（`cy-server`）与客户端（`cy-core`）各自实现 IO，共用这里的一套语义。

pub mod error;
pub mod naming;

/// 协议版本。客户端在 `hello` 中上报，服务端不认识则拒绝连接。
pub const PROTO_VERSION: u32 = 1;
