use thiserror::Error;

/// Errors that can occur during frame encoding/decoding.
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("invalid message type: {0}")]
    InvalidMsgType(u8),
    #[error("frame too short: need at least {expected} bytes, got {actual}")]
    TooShort { expected: usize, actual: usize },
    #[error("frame too large: declared {declared} bytes exceeds maximum {max}")]
    TooLarge { declared: u64, max: u64 },
    #[error("msgpack decode error: {0}")]
    MsgpackDecode(String),
    #[error("msgpack encode error: {0}")]
    MsgpackEncode(String),
}

/// Maximum total size, in bytes, of a single wire frame (fixed header plus the
/// declared header and payload sections).
///
/// This bound is enforced by [`Frame::decode`] and by the transports before any
/// buffer is allocated from a peer-controlled length prefix, so a malicious peer
/// cannot trigger an unbounded allocation (remote OOM/DoS). 64 MiB is well above
/// any legitimate frame while remaining cheap to reject.
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// [`MAX_FRAME_SIZE`] as a `u64`, for overflow-safe comparisons against
/// peer-declared lengths without a `usize`-to-`u64` cast.
const MAX_FRAME_SIZE_U64: u64 = 64 * 1024 * 1024;

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
    /// SWIM failure-detection / gossip message (Ping / Ack / `PingReq`, with
    /// piggybacked membership gossip). The kind is carried inside the payload
    /// envelope rather than as a distinct wire byte.
    Swim = 0x0E,
}

impl MsgType {
    /// Decode a message type from its wire byte.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::InvalidMsgType`] if `v` does not correspond to a
    /// known message type.
    pub const fn from_u8(v: u8) -> Result<Self, FrameError> {
        match v {
            0x01 => Ok(Self::Send),
            0x02 => Ok(Self::Monitor),
            0x03 => Ok(Self::Demonitor),
            0x04 => Ok(Self::Link),
            0x05 => Ok(Self::Unlink),
            0x06 => Ok(Self::Exit),
            0x07 => Ok(Self::ProcessDown),
            0x08 => Ok(Self::NameLookup),
            0x09 => Ok(Self::NameRegister),
            0x0A => Ok(Self::NameUnregister),
            0x0B => Ok(Self::Heartbeat),
            0x0C => Ok(Self::HeartbeatAck),
            0x0D => Ok(Self::NodeInfo),
            0x0E => Ok(Self::Swim),
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

/// Fixed header size: `version(1)` + `msg_type(1)` + `request_id(8)` + `header_len(4)` + `payload_len(4)` = 18
const FIXED_HEADER_SIZE: usize = 18;

impl Frame {
    /// Encode this frame into its wire representation.
    ///
    /// # Panics
    ///
    /// Panics if the header or payload fails to encode as msgpack, or if
    /// either encoded section exceeds `u32::MAX` bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut header_buf = Vec::new();
        rmpv::encode::write_value(&mut header_buf, &self.header)
            .expect("msgpack header encode failed");

        let mut payload_buf = Vec::new();
        rmpv::encode::write_value(&mut payload_buf, &self.payload)
            .expect("msgpack payload encode failed");

        let header_len =
            u32::try_from(header_buf.len()).expect("frame header exceeds u32::MAX bytes");
        let payload_len =
            u32::try_from(payload_buf.len()).expect("frame payload exceeds u32::MAX bytes");

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

    /// Decode a frame from its wire representation.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::TooShort`] if `bytes` is shorter than the fixed
    /// header or the declared section lengths, [`FrameError::TooLarge`] if the
    /// declared total exceeds [`MAX_FRAME_SIZE`], [`FrameError::InvalidMsgType`]
    /// for an unknown message type byte, and [`FrameError::MsgpackDecode`] if
    /// the header or payload is not valid msgpack.
    ///
    /// # Panics
    ///
    /// Does not panic in practice: the length check above guarantees the
    /// fixed-size slices converted via `try_into` are exactly the right size.
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
        let header_len_u32 = u32::from_be_bytes(bytes[10..14].try_into().unwrap());
        let payload_len_u32 = u32::from_be_bytes(bytes[14..18].try_into().unwrap());

        // Compute the declared total with checked u64 arithmetic so
        // attacker-controlled section lengths cannot overflow `usize` on 32-bit
        // targets (which would wrap to a small value and bypass the bounds check
        // below). Both lengths are widened to `u64` first so the addition itself
        // cannot overflow.
        let max = MAX_FRAME_SIZE_U64;
        let declared =
            (FIXED_HEADER_SIZE as u64) + u64::from(header_len_u32) + u64::from(payload_len_u32);
        if declared > max {
            return Err(FrameError::TooLarge { declared, max });
        }
        // `declared <= MAX_FRAME_SIZE`, so it fits in usize on every supported
        // target; `try_from` keeps the conversion lint-clean (no truncating cast).
        let Ok(expected_total) = usize::try_from(declared) else {
            return Err(FrameError::TooLarge { declared, max });
        };
        // Both lengths are bounded by `declared`, so these conversions cannot
        // fail; `try_from` keeps them lint-clean.
        let (Ok(header_len), Ok(payload_len)) = (
            usize::try_from(header_len_u32),
            usize::try_from(payload_len_u32),
        ) else {
            return Err(FrameError::TooLarge { declared, max });
        };
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

        Ok(Self {
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
            header: rmpv::Value::Map(vec![(
                rmpv::Value::String("dest".into()),
                rmpv::Value::Integer(42.into()),
            )]),
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
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Heartbeat,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.msg_type, MsgType::Heartbeat);
    }

    #[test]
    fn encode_decode_with_request_id() {
        let frame = Frame {
            version: 1,
            msg_type: MsgType::NameLookup,
            request_id: 12345,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.request_id, 12345);
    }

    #[test]
    fn all_msg_types_roundtrip() {
        let types = [
            MsgType::Send,
            MsgType::Monitor,
            MsgType::Demonitor,
            MsgType::Link,
            MsgType::Unlink,
            MsgType::Exit,
            MsgType::ProcessDown,
            MsgType::NameLookup,
            MsgType::NameRegister,
            MsgType::NameUnregister,
            MsgType::Heartbeat,
            MsgType::HeartbeatAck,
            MsgType::NodeInfo,
        ];
        for msg_type in types {
            let frame = Frame {
                version: 1,
                msg_type,
                request_id: 0,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::Nil,
            };
            let decoded = Frame::decode(&frame.encode()).unwrap();
            assert_eq!(decoded.msg_type, msg_type);
        }
    }

    #[test]
    fn decode_invalid_msg_type() {
        let mut bytes = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        }
        .encode();
        bytes[1] = 0xFF;
        assert!(Frame::decode(&bytes).is_err());
    }

    #[test]
    fn decode_truncated_header() {
        assert!(Frame::decode(&[0u8; 5]).is_err());
    }

    #[test]
    fn decode_truncated_payload() {
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::String("data".into()),
        };
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
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::String(big_string.into()),
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload.as_str().unwrap().len(), 100_000);
    }

    #[test]
    fn binary_payload_roundtrip() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Binary(data.clone()),
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload, rmpv::Value::Binary(data));
    }

    #[test]
    fn nested_map_payload() {
        let payload = rmpv::Value::Map(vec![(
            rmpv::Value::String("nested".into()),
            rmpv::Value::Map(vec![(
                rmpv::Value::String("deep".into()),
                rmpv::Value::Integer(42.into()),
            )]),
        )]);
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload,
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload.as_map().unwrap().len(), 1);
    }

    #[test]
    fn max_request_id() {
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: u64::MAX,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.request_id, u64::MAX);
    }

    #[test]
    fn version_preserved() {
        let frame = Frame {
            version: 42,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.version, 42);
    }

    #[test]
    fn encode_deterministic() {
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Heartbeat,
            request_id: 0,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Nil,
        };
        let a = frame.encode();
        let b = frame.encode();
        assert_eq!(a, b);
    }

    #[test]
    fn decode_rejects_oversized_declared_length() {
        // Hand-craft a header whose declared payload length exceeds
        // MAX_FRAME_SIZE. decode must reject it via TooLarge WITHOUT requiring
        // (or allocating) a buffer that large — we pass only the 18-byte header.
        let mut bytes = vec![0u8; FIXED_HEADER_SIZE];
        bytes[0] = 1; // version
        bytes[1] = MsgType::Send as u8;
        // header_len = 0, payload_len = u32::MAX
        bytes[14..18].copy_from_slice(&u32::MAX.to_be_bytes());
        match Frame::decode(&bytes) {
            Err(FrameError::TooLarge { declared, max }) => {
                assert_eq!(max, MAX_FRAME_SIZE as u64);
                assert!(declared > MAX_FRAME_SIZE as u64);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_overflowing_section_lengths() {
        // header_len = u32::MAX and payload_len = u32::MAX. Summed as usize this
        // could wrap on a 32-bit target; checked u64 arithmetic must instead
        // report TooLarge rather than overflow and bypass the bounds check.
        let mut bytes = vec![0u8; FIXED_HEADER_SIZE];
        bytes[0] = 1;
        bytes[1] = MsgType::Send as u8;
        bytes[10..14].copy_from_slice(&u32::MAX.to_be_bytes());
        bytes[14..18].copy_from_slice(&u32::MAX.to_be_bytes());
        let expected = (FIXED_HEADER_SIZE as u64) + u64::from(u32::MAX) + u64::from(u32::MAX);
        match Frame::decode(&bytes) {
            Err(FrameError::TooLarge { declared, max }) => {
                assert_eq!(declared, expected);
                assert_eq!(max, MAX_FRAME_SIZE as u64);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn header_and_payload_both_populated() {
        let header = rmpv::Value::Map(vec![
            (
                rmpv::Value::String("from".into()),
                rmpv::Value::Integer(1.into()),
            ),
            (
                rmpv::Value::String("to".into()),
                rmpv::Value::Integer(2.into()),
            ),
        ]);
        let payload = rmpv::Value::String("body".into());
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 7,
            header,
            payload,
        };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.header.as_map().unwrap().len(), 2);
        assert_eq!(decoded.payload.as_str().unwrap(), "body");
    }
}
