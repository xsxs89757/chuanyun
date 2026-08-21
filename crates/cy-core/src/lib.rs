//! 穿云客户端核心库。
//!
//! 隧道引擎与 UI 无关：桌面端（`cy-desktop`）、示例 headless 壳、未来的 CLI
//! 都只是这个库的壳。库内不做任何界面假设，对外通过事件流和命令 API 交互。
//!
//! 这样切分还有个实际好处：集成测试可以在没有图形环境的 CI 上跑完整链路，
//! 桌面端那边只需要冒烟编译。
//!
//! # 从哪儿开始读
//!
//! [`Engine`] 是入口——它维护「用户想要什么」（隧道列表，存在磁盘上）和
//! 「现在实际怎样」（连上没有、每条隧道的公网地址）两份状态，并负责把它们对齐：
//! 断线自动重连、连上之后把该开的隧道重开一遍。

pub mod backoff;
pub mod brand;
pub mod client;
pub mod engine;
pub mod inspector;
pub mod local_api;
pub mod state;
pub mod verifier;

pub use backoff::Backoff;
pub use client::{connect, Connection, CoreConfig, Event, TunnelSpec, Verify, CLIENT_VERSION};
pub use cy_proto::PROTO_VERSION;
pub use engine::{Brand, Engine, Status, TunnelStatus};
pub use state::State;
