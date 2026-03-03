# Wire Protocol Internals

## Overview

Rebar nodes communicate using a custom binary wire protocol designed for low-overhead inter-node messaging. Each message is represented as a `Frame` consisting of a fixed 18-byte header followed by two variable-length sections: a MessagePack-encoded header and a MessagePack-encoded payload.

The protocol is implemented in `crates/rebar-cluster/src/protocol/frame.rs`.

## Frame Layout

Every frame on the wire follows this structure:

```
Offset  Size  Field         Description
------  ----  -----------   ----------------------------------------
0       1     version       Protocol version (currently 0x01)
1       1     msg_type      Message type (MsgType as u8)
2       8     request_id    Request correlation ID (u64 big-endian)
10      4     header_len    Length of header section (u32 big-endian)
14      4     payload_len   Length of payload section (u32 big-endian)
18      N     header        MessagePack-encoded header data
18+N    M     payload       MessagePack-encoded payload data
```

The fixed header is exactly 18 bytes (`FIXED_HEADER_SIZE`). The total frame size is `18 + header_len + payload_len`.

All multi-byte integer fields are encoded in **big-endian** byte order.

## Message Types

There are 13 message types, each identified by a single-byte discriminant:

| Hex    | Name             | Description                                         |
|--------|------------------|-----------------------------------------------------|
| `0x01` | Send             | Send a message to a remote process                  |
| `0x02` | Monitor          | Request monitoring of a remote process              |
| `0x03` | Demonitor        | Cancel a monitor                                    |
| `0x04` | Link             | Create a bidirectional link between processes        |
| `0x05` | Unlink           | Remove a bidirectional link                         |
| `0x06` | Exit             | Signal process exit                                 |
| `0x07` | ProcessDown      | Monitor notification that a process has exited      |
| `0x08` | NameLookup       | Query the global name registry                      |
| `0x09` | NameRegister     | Register a name in the global registry              |
| `0x0A` | NameUnregister   | Remove a name from the global registry              |
| `0x0B` | Heartbeat        | Periodic liveness check                             |
| `0x0C` | HeartbeatAck     | Response to a heartbeat                             |
| `0x0D` | NodeInfo         | Node metadata exchange during connection setup      |

The `MsgType` enum is defined with `#[repr(u8)]` for direct casting. Any byte value outside `0x01..=0x0D` results in a `FrameError::InvalidMsgType` error during decoding.

## Encoding Process

`Frame::encode()` serializes a frame into a `Vec<u8>` suitable for transmission:

1. **Serialize the header** -- Encode `self.header` (an `rmpv::Value`) into a temporary byte buffer using `rmpv::encode::write_value()`.

2. **Serialize the payload** -- Encode `self.payload` (an `rmpv::Value`) into a separate temporary byte buffer using `rmpv::encode::write_value()`.

3. **Compute lengths** -- Record `header_len` and `payload_len` as `u32` values from the serialized buffer sizes.

4. **Allocate output buffer** -- Pre-allocate a `Vec<u8>` with capacity `FIXED_HEADER_SIZE + header_len + payload_len` to avoid reallocations.

5. **Write the fixed header** (18 bytes):
   - Push `version` (1 byte)
   - Push `msg_type as u8` (1 byte)
   - Extend with `request_id.to_be_bytes()` (8 bytes)
   - Extend with `header_len.to_be_bytes()` (4 bytes)
   - Extend with `payload_len.to_be_bytes()` (4 bytes)

6. **Append variable sections** -- Extend with the serialized header bytes, then the serialized payload bytes.

The encoding is deterministic: encoding the same frame twice produces identical byte output.

## Decoding Process

`Frame::decode(bytes: &[u8])` reconstructs a `Frame` from raw bytes:

1. **Check minimum length** -- If `bytes.len() < 18`, return `FrameError::TooShort` with `expected: 18` and `actual: bytes.len()`.

2. **Parse the fixed header**:
   - `version = bytes[0]`
   - `msg_type = MsgType::from_u8(bytes[1])?` -- validates the message type
   - `request_id = u64::from_be_bytes(bytes[2..10])`
   - `header_len = u32::from_be_bytes(bytes[10..14]) as usize`
   - `payload_len = u32::from_be_bytes(bytes[14..18]) as usize`

3. **Validate total length** -- Compute `expected_total = 18 + header_len + payload_len`. If `bytes.len() < expected_total`, return `FrameError::TooShort`.

4. **Extract byte slices**:
   - Header bytes: `bytes[18 .. 18 + header_len]`
   - Payload bytes: `bytes[18 + header_len .. 18 + header_len + payload_len]`

5. **Deserialize MessagePack** -- Decode header and payload byte slices into `rmpv::Value` using `rmpv::decode::read_value()`. Any deserialization failure returns `FrameError::MsgpackDecode`.

## Error Handling

The `FrameError` enum covers all failure modes during encoding and decoding:

| Variant                     | Cause                                                   |
|-----------------------------|---------------------------------------------------------|
| `FrameError::TooShort`      | Input buffer shorter than the required minimum (either the 18-byte fixed header or the full frame as indicated by length fields). Carries `expected` and `actual` sizes. |
| `FrameError::InvalidMsgType` | The `msg_type` byte does not map to any known `MsgType` variant. Carries the invalid `u8` value. |
| `FrameError::MsgpackDecode` | MessagePack deserialization of the header or payload section failed. Carries the error message as a `String`. |
| `FrameError::MsgpackEncode` | MessagePack serialization failed. Carries the error message as a `String`. Note: `encode()` currently uses `.expect()` rather than returning this variant, so this error is defined for future use or external callers. |

## MessagePack

The wire protocol uses [MessagePack](https://msgpack.org/) for the header and payload sections. The choice of MessagePack over alternatives like Protocol Buffers or JSON is motivated by:

- **Compact binary representation** -- MessagePack produces significantly smaller output than JSON while remaining simpler than Protocol Buffers. This reduces bandwidth usage for high-frequency inter-node communication.

- **Schema-less flexibility** -- Both the header and payload are typed as `rmpv::Value`, the dynamic value type from the `rmpv` crate. This allows different message types to carry different data structures without requiring a unified schema or code generation step.

- **Dynamic typing with `rmpv::Value`** -- The `rmpv::Value` enum supports maps, arrays, strings, integers, floats, binary data, booleans, and nil. This makes it straightforward to represent routing metadata in the header (source/destination PIDs, node identifiers) and arbitrary application data in the payload.

By convention:
- The **header** section carries routing and control metadata, such as source and destination process IDs.
- The **payload** section carries the application-level message data being sent between processes.

For simple control messages like `Heartbeat` or `HeartbeatAck`, both header and payload may be `rmpv::Value::Nil`, which encodes to a single byte (`0xC0`).

## Wire Example

Consider encoding a `Send` message from process 1 to process 2 carrying the string `"hello"`, with request ID 42:

### Construction

```rust
let frame = Frame {
    version: 1,
    msg_type: MsgType::Send,
    request_id: 42,
    header: rmpv::Value::Map(vec![
        (rmpv::Value::String("from".into()), rmpv::Value::Integer(1.into())),
        (rmpv::Value::String("to".into()), rmpv::Value::Integer(2.into())),
    ]),
    payload: rmpv::Value::String("hello".into()),
};
let bytes = frame.encode();
```

### Resulting Byte Layout

```
Fixed header (18 bytes):
  01                         version = 1
  01                         msg_type = Send (0x01)
  00 00 00 00 00 00 00 2A    request_id = 42 (big-endian u64)
  00 00 00 0B                header_len = 11 (big-endian u32)
  00 00 00 06                payload_len = 6 (big-endian u32)

Header section (11 bytes, MessagePack map with 2 entries):
  82                         fixmap with 2 entries
  A4 66 72 6F 6D             fixstr "from" (4 bytes)
  01                         positive fixint 1
  A2 74 6F                   fixstr "to" (2 bytes)
  02                         positive fixint 2

Payload section (6 bytes, MessagePack string):
  A5 68 65 6C 6C 6F          fixstr "hello" (5 bytes)

Total frame size: 18 + 11 + 6 = 35 bytes
```

Complete hex dump:

```
01 01 00 00 00 00 00 00 00 2A 00 00 00 0B 00 00
00 06 82 A4 66 72 6F 6D 01 A2 74 6F 02 A5 68 65
6C 6C 6F
```

### Decoding

```rust
let decoded = Frame::decode(&bytes).unwrap();
assert_eq!(decoded.version, 1);
assert_eq!(decoded.msg_type, MsgType::Send);
assert_eq!(decoded.request_id, 42);
// decoded.header is the map {"from": 1, "to": 2}
// decoded.payload is the string "hello"
```

The decode path first reads the 18-byte fixed header, validates the message type byte `0x01` maps to `MsgType::Send`, confirms the buffer contains at least `18 + 11 + 6 = 35` bytes, then deserializes the header and payload slices from MessagePack back into `rmpv::Value` instances.
