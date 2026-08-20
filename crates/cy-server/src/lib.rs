//! 穿云服务端。
//!
//! `main.rs` 只是薄壳：真正的启动入口是 [`Server::start`]，它返回携带实际绑定地址的
//! 句柄——集成测试据此用 `:0` 端口起真实服务端，不需要预分配端口或猜测配置。

pub use cy_proto::PROTO_VERSION;
