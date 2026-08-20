//! 控制流消息。
//!
//! 客户端连上来之后开的第一条 yamux 流就是控制流，双向传 JSON Lines（一行一条消息）。
//! 控制面流量很小，用 JSON 而不是二进制：抓包能直接看懂，加字段也不用改对端。
//!
//! 两条兼容性约定：
//! - **未知字段直接忽略**（serde 默认行为）——新版本加字段，老版本照常工作；
//! - **未知消息类型解析成 [`ClientMsg::Unknown`] / [`ServerMsg::Unknown`]** 而不是报错，
//!   收到就跳过。反过来，[`crate::PROTO_VERSION`] 对不上是硬失败——那说明语义变了，
//!   继续跑只会出更难查的问题。

use serde::{Deserialize, Serialize};

/// 隧道类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelKind {
    /// HTTP(S)：按 Host 路由，走 443 入口
    Http,
    /// TCP：从公网端口池分配一个端口（V1.5）
    Tcp,
}

/// 隧道开通后对外可达的地址。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endpoint {
    /// HTTP 隧道：完整地址，如 `https://zhangsan-wx.t.example.com`
    Url(String),
    /// TCP 隧道：`host:port`
    Addr(String),
}

/// 客户端 → 服务端。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// 握手。必须是控制流上的第一条消息。
    Hello {
        proto: u32,
        /// 客户端版本，如 "0.1.0"
        client: String,
        /// 操作系统标识，仅用于服务端审计与排查
        os: String,
        token: String,
    },

    /// 请求开一条隧道。
    ///
    /// 注意这里**不带本地端口**：本地端口只存在于客户端侧，服务端不需要知道，
    /// 也就没法泄露。`id` 由客户端生成，用来把后续的成功/失败消息对上号。
    OpenTunnel {
        id: String,
        kind: TunnelKind,
        /// 隧道名，最终子域名由服务端拼成 `{user}-{name}`
        name: String,
        /// 绑定的自定义域名；`None` 表示用约定式子域名
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_domain: Option<String>,
        /// 访问口令（`user:pass`），服务端在入口处校验；`None` 表示不设防
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<String>,
        /// TCP 隧道申请的固定公网端口；0 或 `None` 表示从池里随便分一个
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_port: Option<u16>,
    },

    /// 关闭一条隧道。
    CloseTunnel {
        id: String,
    },

    Ping {
        seq: u64,
    },
    Pong {
        seq: u64,
    },

    /// 收到了不认识的消息类型——跳过即可，不要当成错误。
    #[serde(other)]
    Unknown,
}

/// 服务端 → 客户端。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// 握手成功。
    Welcome {
        /// 会话 ID，写进审计日志，方便把一次连接的前后行为串起来
        session: String,
        /// 服务端版本
        server: String,
        /// 心跳间隔（秒），客户端据此发 ping；连续 3 次没回应就判定连接已死
        heartbeat_secs: u64,
        /// 域名后缀，如 `t.example.com`。客户端拿它做展示与本地 API 的返回值，
        /// 但**不用它自己拼子域名**——地址一律以服务端返回的为准。
        domain_suffix: String,
    },

    /// 隧道开通成功。
    TunnelOpened {
        id: String,
        public: Endpoint,
    },

    /// 出错了。带 `id` 表示是针对某次 `open_tunnel` 的失败，不带则是连接级错误。
    Error {
        /// 稳定错误码，见 [`crate::error::code`]
        code: String,
        /// 服务端给的补充说明；界面展示优先用 [`crate::error::human`] 翻译 `code`
        #[serde(default)]
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

    /// 被管理员踢下线，随后连接会断开。
    Kick {
        reason: String,
    },

    Ping {
        seq: u64,
    },
    Pong {
        seq: u64,
    },

    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_client(msg: &ClientMsg) -> ClientMsg {
        let line = serde_json::to_string(msg).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn roundtrip_server(msg: &ServerMsg) -> ServerMsg {
        let line = serde_json::to_string(msg).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn client_messages_roundtrip() {
        let hello = ClientMsg::Hello {
            proto: crate::PROTO_VERSION,
            client: "0.1.0".into(),
            os: "macos".into(),
            token: "cy_zhangsan_0123456789abcdef".into(),
        };
        assert_eq!(roundtrip_client(&hello), hello);

        let open = ClientMsg::OpenTunnel {
            id: "t1".into(),
            kind: TunnelKind::Http,
            name: "wx".into(),
            custom_domain: None,
            auth: None,
            remote_port: None,
        };
        assert_eq!(roundtrip_client(&open), open);
    }

    #[test]
    fn server_messages_roundtrip() {
        let welcome = ServerMsg::Welcome {
            session: "s_8d".into(),
            server: "0.1.0".into(),
            heartbeat_secs: 15,
            domain_suffix: "t.example.com".into(),
        };
        assert_eq!(roundtrip_server(&welcome), welcome);

        let opened = ServerMsg::TunnelOpened {
            id: "t1".into(),
            public: Endpoint::Url("https://zhangsan-wx.t.example.com".into()),
        };
        assert_eq!(roundtrip_server(&opened), opened);
    }

    #[test]
    fn optional_fields_are_omitted_when_empty() {
        let open = ClientMsg::OpenTunnel {
            id: "t1".into(),
            kind: TunnelKind::Http,
            name: "wx".into(),
            custom_domain: None,
            auth: None,
            remote_port: None,
        };
        let line = serde_json::to_string(&open).unwrap();
        assert!(
            !line.contains("custom_domain"),
            "线上不该出现空字段：{line}"
        );
        assert!(!line.contains("auth"));
        assert!(!line.contains("remote_port"));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // 新版本服务端多发了一个字段，老客户端应该照常解析
        let line = r#"{"type":"welcome","session":"s1","server":"9.9.9",
                       "heartbeat_secs":15,"domain_suffix":"t.example.com",
                       "some_future_field":{"nested":true}}"#;
        let msg: ServerMsg = serde_json::from_str(line).unwrap();
        assert!(matches!(msg, ServerMsg::Welcome { .. }));
    }

    #[test]
    fn unknown_message_type_becomes_unknown_not_error() {
        let msg: ServerMsg = serde_json::from_str(r#"{"type":"brand_new_thing","x":1}"#).unwrap();
        assert_eq!(msg, ServerMsg::Unknown);

        let msg: ClientMsg = serde_json::from_str(r#"{"type":"brand_new_thing"}"#).unwrap();
        assert_eq!(msg, ClientMsg::Unknown);
    }

    #[test]
    fn missing_optional_message_defaults_to_empty() {
        let msg: ServerMsg = serde_json::from_str(r#"{"type":"error","code":"E_LIMIT"}"#).unwrap();
        match msg {
            ServerMsg::Error { code, message, id } => {
                assert_eq!(code, "E_LIMIT");
                assert_eq!(message, "");
                assert_eq!(id, None);
            }
            other => panic!("解析成了 {other:?}"),
        }
    }

    #[test]
    fn endpoint_is_tagged_by_shape() {
        let url = serde_json::to_string(&Endpoint::Url("https://a.example.com".into())).unwrap();
        assert_eq!(url, r#"{"url":"https://a.example.com"}"#);
        let addr = serde_json::to_string(&Endpoint::Addr("a.example.com:20017".into())).unwrap();
        assert_eq!(addr, r#"{"addr":"a.example.com:20017"}"#);
    }
}
