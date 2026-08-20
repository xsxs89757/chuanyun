//! 数据流头。
//!
//! 服务端每收到一个外部请求（HTTP）或连接（TCP），就在 yamux 上开一条新流，
//! 先写一行 JSON 说明这条流属于哪个隧道、对端是谁，之后就是纯字节双向拷贝。
//!
//! 「一行 JSON + 裸字节」这个形状是有意的：客户端读完这一行之后不需要理解上层协议，
//! 直接把剩下的字节转发给本地端口就行。WebSocket 升级、SSE、大文件上传因此天然可用——
//! 它们对隧道来说都只是字节流。

use serde::{Deserialize, Serialize};

use crate::control::TunnelKind;

/// 数据流的第一行。
///
/// 字段名有意压短（`t` 而非 `tunnel_id`）：每条流都要带一份，而 HTTP 场景下
/// 一个网页可能同时开几十条流。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHeader {
    /// 隧道 ID（对应 `open_tunnel` 里的 `id`）
    #[serde(rename = "t")]
    pub tunnel_id: String,
    /// 外部访问者的 IP，客户端可用于展示与请求观测
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    pub kind: TunnelKind,
}

impl StreamHeader {
    pub fn new(tunnel_id: impl Into<String>, kind: TunnelKind) -> Self {
        Self {
            tunnel_id: tunnel_id.into(),
            peer: None,
            kind,
        }
    }

    pub fn with_peer(mut self, peer: impl Into<String>) -> Self {
        self.peer = Some(peer.into());
        self
    }

    /// 序列化成一行（**不含**结尾换行，由调用方按需补上）。
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("StreamHeader 结构固定，序列化不会失败")
    }

    /// 从一行 JSON 解析（调用方应已剥掉结尾换行）。
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = StreamHeader::new("t1", TunnelKind::Http).with_peer("113.5.4.3");
        let parsed = StreamHeader::from_line(&h.to_line()).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn peer_is_optional() {
        let h = StreamHeader::new("t1", TunnelKind::Tcp);
        let line = h.to_line();
        assert!(!line.contains("peer"), "没有 peer 时不该占字节：{line}");
        assert_eq!(StreamHeader::from_line(&line).unwrap(), h);
    }

    #[test]
    fn stays_compact() {
        // 每条流都要带一份，盯着别让它无声变胖
        let line = StreamHeader::new("t1", TunnelKind::Http)
            .with_peer("113.5.4.3")
            .to_line();
        assert!(line.len() < 64, "流头长到了 {} 字节：{line}", line.len());
    }

    #[test]
    fn tolerates_unknown_fields() {
        let h = StreamHeader::from_line(r#"{"t":"t1","kind":"http","future":1}"#).unwrap();
        assert_eq!(h.tunnel_id, "t1");
    }
}
