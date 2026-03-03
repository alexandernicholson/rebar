use thiserror::Error;

/// Errors that can occur during frame encoding/decoding.
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("invalid message type: {0}")]
    InvalidMsgType(u8),
    #[error("frame too short: need at least {expected} bytes, got {actual}")]
    TooShort { expected: usize, actual: usize },
    #[error("msgpack decode error: {0}")]
    MsgpackDecode(String),
    #[error("msgpack encode error: {0}")]
    MsgpackEncode(String),
}

/// Wire protocol message types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MsgType {
    Send = 0x01,
    Monitor = 0x02,
    Demonitor = 0x03,
    Link = 0x04,
    Unlink = 0x05,
    Exit = 0x06,
    ProcessDown = 0x07,
    NameLookup = 0x08,
    NameRegister = 0x09,
    NameUnregister = 0x0A,
    Heartbeat = 0x0B,
    HeartbeatAck = 0x0C,
    NodeInfo = 0x0D,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Result<Self, FrameError> {
        match v {
            0x01 => Ok(MsgType::Send),
            0x02 => Ok(MsgType::Monitor),
            0x03 => Ok(MsgType::Demonitor),
            0x04 => Ok(MsgType::Link),
            0x05 => Ok(MsgType::Unlink),
            0x06 => Ok(MsgType::Exit),
            0x07 => Ok(MsgType::ProcessDown),
            0x08 => Ok(MsgType::NameLookup),
            0x09 => Ok(MsgType::NameRegister),
            0x0A => Ok(MsgType::NameUnregister),
            0x0B => Ok(MsgType::Heartbeat),
            0x0C => Ok(MsgType::HeartbeatAck),
            0x0D => Ok(MsgType::NodeInfo),
            _ => Err(FrameError::InvalidMsgType(v)),
        }
    }
}

/// A wire protocol frame.
#[derive(Clone, Debug)]
pub struct Frame {
    pub version: u8,
    pub msg_type: MsgType,
    pub request_id: u64,
    pub header: rmpv::Value,
    pub payload: rmpv::Value,
}

/// Fixed header size: version(1) + msg_type(1) + request_id(8) + header_len(4) + payload_len(4) = 18
const FIXED_HEADER_SIZE: usize = 18;

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut header_buf = Vec::new();
        rmpv::encode::write_value(&mut header_buf, &self.header)
            .expect("msgpack header encode failed");

        let mut payload_buf = Vec::new();
        rmpv::encode::write_value(&mut payload_buf, &self.payload)
            .expect("msgpack payload encode failed");

        let header_len = header_buf.len() as u32;
        let payload_len = payload_buf.len() as u32;

        let total = FIXED_HEADER_SIZE + header_buf.len() + payload_buf.len();
        let mut out = Vec::with_capacity(total);

        out.push(self.version);
        out.push(self.msg_type as u8);
        out.extend_from_slice(&self.request_id.to_be_bytes());
        out.extend_from_slice(&header_len.to_be_bytes());
        out.extend_from_slice(&payload_len.to_be_bytes());
        out.extend_from_slice(&header_buf);
        out.extend_from_slice(&payload_buf);

        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() < FIXED_HEADER_SIZE {
            return Err(FrameError::TooShort {
                expected: FIXED_HEADER_SIZE,
                actual: bytes.len(),
            });
        }

        let version = bytes[0];
        let msg_type = MsgType::from_u8(bytes[1])?;
        let request_id = u64::from_be_bytes(bytes[2..10].try_into().unwrap());
        let header_len = u32::from_be_bytes(bytes[10..14].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(bytes[14..18].try_into().unwrap()) as usize;

        let expected_total = FIXED_HEADER_SIZE + header_len + payload_len;
        if bytes.len() < expected_total {
            return Err(FrameError::TooShort {
                expected: expected_total,
                actual: bytes.len(),
            });
        }

        let header_bytes = &bytes[FIXED_HEADER_SIZE..FIXED_HEADER_SIZE + header_len];
        let header = rmpv::decode::read_value(&mut &header_bytes[..])
            .map_err(|e| FrameError::MsgpackDecode(e.to_string()))?;

        let payload_start = FIXED_HEADER_SIZE + header_len;
        let payload_bytes = &bytes[payload_start..payload_start + payload_len];
        let payload = rmpv::decode::read_value(&mut &payload_bytes[..])
            .map_err(|e| FrameError::MsgpackDecode(e.to_string()))?;

        Ok(Frame {
            version,
            msg_type,
            request_id,
            header,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_send() {
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Map(vec![
                (rmpv::Value::String("dest".into()), rmpv::Value::Integer(42.into())),
            ]),
            payload: rmpv::Value::String("hello".into()),
        };
        let bytes = frame.encode();
        let decoded = Frame::decode(&bytes).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.msg_type, MsgType::Send);
        assert_eq!(decoded.payload, rmpv::Value::String("hello".into()));
    }

    #[test]
    fn encode_decode_heartbeat() {
        let frame = Frame { version: 1, msg_type: MsgType::Heartbeat, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.msg_type, MsgType::Heartbeat);
    }

    #[test]
    fn encode_decode_with_request_id() {
        let frame = Frame { version: 1, msg_type: MsgType::NameLookup, request_id: 12345, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.request_id, 12345);
    }

    #[test]
    fn all_msg_types_roundtrip() {
        let types = [
            MsgType::Send, MsgType::Monitor, MsgType::Demonitor,
            MsgType::Link, MsgType::Unlink, MsgType::Exit,
            MsgType::ProcessDown, MsgType::NameLookup,
            MsgType::NameRegister, MsgType::NameUnregister,
            MsgType::Heartbeat, MsgType::HeartbeatAck, MsgType::NodeInfo,
        ];
        for msg_type in types {
            let frame = Frame { version: 1, msg_type, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
            let decoded = Frame::decode(&frame.encode()).unwrap();
            assert_eq!(decoded.msg_type, msg_type);
        }
    }

    #[test]
    fn decode_invalid_msg_type() {
        let mut bytes = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil }.encode();
        bytes[1] = 0xFF;
        assert!(Frame::decode(&bytes).is_err());
    }

    #[test]
    fn decode_truncated_header() {
        assert!(Frame::decode(&[0u8; 5]).is_err());
    }

    #[test]
    fn decode_truncated_payload() {
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::String("data".into()) };
        let bytes = frame.encode();
        let truncated = &bytes[..bytes.len() - 2];
        assert!(Frame::decode(truncated).is_err());
    }

    #[test]
    fn decode_empty_bytes() {
        assert!(Frame::decode(&[]).is_err());
    }

    #[test]
    fn large_payload_roundtrip() {
        let big_string = "x".repeat(100_000);
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::String(big_string.clone().into()) };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload.as_str().unwrap().len(), 100_000);
    }

    #[test]
    fn binary_payload_roundtrip() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Binary(data.clone()) };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload, rmpv::Value::Binary(data));
    }

    #[test]
    fn nested_map_payload() {
        let payload = rmpv::Value::Map(vec![
            (rmpv::Value::String("nested".into()), rmpv::Value::Map(vec![
                (rmpv::Value::String("deep".into()), rmpv::Value::Integer(42.into())),
            ])),
        ]);
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload.as_map().unwrap().len(), 1);
    }

    #[test]
    fn max_request_id() {
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: u64::MAX, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.request_id, u64::MAX);
    }

    #[test]
    fn version_preserved() {
        let frame = Frame { version: 42, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.version, 42);
    }

    #[test]
    fn encode_deterministic() {
        let frame = Frame { version: 1, msg_type: MsgType::Heartbeat, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let a = frame.encode();
        let b = frame.encode();
        assert_eq!(a, b);
    }

    #[test]
    fn header_and_payload_both_populated() {
        let header = rmpv::Value::Map(vec![
            (rmpv::Value::String("from".into()), rmpv::Value::Integer(1.into())),
            (rmpv::Value::String("to".into()), rmpv::Value::Integer(2.into())),
        ]);
        let payload = rmpv::Value::String("body".into());
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 7, header, payload };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.header.as_map().unwrap().len(), 2);
        assert_eq!(decoded.payload.as_str().unwrap(), "body");
    }
}
