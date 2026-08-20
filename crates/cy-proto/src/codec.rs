//! 控制流的 JSON Lines 编解码器（feature `codec`）。
//!
//! 服务端和客户端都用它，只是收发的类型正好相反：服务端 `JsonLines<ClientMsg, ServerMsg>`，
//! 客户端 `JsonLines<ServerMsg, ClientMsg>`。类型参数把方向固定下来，编译期就挡住
//! 「往控制流上写了个本该由对端发的消息」这类错误。

use std::marker::PhantomData;

use serde::{de::DeserializeOwned, Serialize};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder, LinesCodec, LinesCodecError};

/// 单条控制消息的长度上限。
///
/// 控制消息都很小（最大的 `hello` 也就几百字节）。设上限是因为对端不可信：
/// 没有这道闸，一个不发换行的连接能让我们无限缓冲下去。
pub const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("控制消息超长或格式有误")]
    Line(#[from] LinesCodecError),
    #[error("控制消息不是合法 JSON: {0}")]
    Json(#[from] serde_json::Error),
    // tokio-util 的 Decoder/Encoder 要求错误类型能承接 io::Error，
    // 底层连接出问题时框架会直接构造这个变体。
    #[error("连接读写失败: {0}")]
    Io(#[from] std::io::Error),
}

/// 一行一条 JSON 消息的编解码器。`In` 是收到的类型，`Out` 是发出的类型。
pub struct JsonLines<In, Out> {
    lines: LinesCodec,
    _marker: PhantomData<fn() -> (In, Out)>,
}

impl<In, Out> JsonLines<In, Out> {
    pub fn new() -> Self {
        Self {
            lines: LinesCodec::new_with_max_length(MAX_LINE_BYTES),
            _marker: PhantomData,
        }
    }
}

impl<In, Out> Default for JsonLines<In, Out> {
    fn default() -> Self {
        Self::new()
    }
}

impl<In, Out> Decoder for JsonLines<In, Out>
where
    In: DeserializeOwned,
{
    type Item = In;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<In>, CodecError> {
        match self.lines.decode(src)? {
            Some(line) => Ok(Some(serde_json::from_str(&line)?)),
            None => Ok(None),
        }
    }
}

impl<In, Out> Encoder<Out> for JsonLines<In, Out>
where
    Out: Serialize,
{
    type Error = CodecError;

    fn encode(&mut self, item: Out, dst: &mut BytesMut) -> Result<(), CodecError> {
        let line = serde_json::to_string(&item)?;
        self.lines.encode(&line, dst)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ClientMsg, ServerMsg};

    /// 服务端视角：收 ClientMsg、发 ServerMsg
    type ServerSide = JsonLines<ClientMsg, ServerMsg>;
    /// 客户端视角：收 ServerMsg、发 ClientMsg
    type ClientSide = JsonLines<ServerMsg, ClientMsg>;

    #[test]
    fn encodes_and_decodes_across_both_sides() {
        let mut buf = BytesMut::new();

        // 客户端发 hello
        let hello = ClientMsg::Hello {
            proto: crate::PROTO_VERSION,
            client: "0.1.0".into(),
            os: "macos".into(),
            token: "cy_zhangsan_abc".into(),
        };
        ClientSide::new().encode(hello.clone(), &mut buf).unwrap();

        // 服务端收到
        let got = ServerSide::new().decode(&mut buf).unwrap().unwrap();
        assert_eq!(got, hello);
        assert!(buf.is_empty(), "解析完应该没有残留字节");
    }

    #[test]
    fn decodes_multiple_messages_from_one_buffer() {
        let mut buf = BytesMut::new();
        let mut enc = ServerSide::new();
        enc.encode(ServerMsg::Ping { seq: 1 }, &mut buf).unwrap();
        enc.encode(ServerMsg::Ping { seq: 2 }, &mut buf).unwrap();

        let mut dec = ClientSide::new();
        assert_eq!(
            dec.decode(&mut buf).unwrap(),
            Some(ServerMsg::Ping { seq: 1 })
        );
        assert_eq!(
            dec.decode(&mut buf).unwrap(),
            Some(ServerMsg::Ping { seq: 2 })
        );
        assert_eq!(dec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn waits_for_a_complete_line() {
        let mut buf = BytesMut::from(&br#"{"type":"ping","seq":1}"#[..]);
        let mut dec = ClientSide::new();
        // 还没收到换行，不能急着解析
        assert_eq!(dec.decode(&mut buf).unwrap(), None);
        buf.extend_from_slice(b"\n");
        assert_eq!(
            dec.decode(&mut buf).unwrap(),
            Some(ServerMsg::Ping { seq: 1 })
        );
    }

    #[test]
    fn rejects_oversized_line() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&vec![b'x'; MAX_LINE_BYTES + 1]);
        let err = ClientSide::new().decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::Line(_)), "应当因超长被拒: {err}");
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        let mut buf = BytesMut::from(&b"not json at all\n"[..]);
        let err = ClientSide::new().decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::Json(_)));
    }
}
