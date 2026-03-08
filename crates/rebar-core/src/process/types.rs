use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId {
    node_id: u64,
    local_id: u64,
}

impl ProcessId {
    pub fn new(node_id: u64, local_id: u64) -> Self {
        Self { node_id, local_id }
    }

    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub fn local_id(&self) -> u64 {
        self.local_id
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}.{}>", self.node_id, self.local_id)
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    from: ProcessId,
    payload: rmpv::Value,
    timestamp: Option<u64>,
}

impl Message {
    /// Create a new message with a timestamp (for external/user-facing messages).
    pub fn new(from: ProcessId, payload: rmpv::Value) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Self {
            from,
            payload,
            timestamp: Some(timestamp),
        }
    }

    /// Create a message without a timestamp (for internal routing, lower overhead).
    pub fn new_internal(from: ProcessId, payload: rmpv::Value) -> Self {
        Self {
            from,
            payload,
            timestamp: None,
        }
    }

    pub fn from(&self) -> ProcessId {
        self.from
    }

    pub fn payload(&self) -> &rmpv::Value {
        &self.payload
    }

    /// Returns the timestamp if one was set, or None for internal messages.
    pub fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }
}

#[derive(Debug, Clone)]
pub enum ExitReason {
    Normal,
    Abnormal(String),
    Kill,
    LinkedExit(ProcessId, Box<ExitReason>),
}

impl ExitReason {
    pub fn is_normal(&self) -> bool {
        matches!(self, ExitReason::Normal)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("process dead: {0}")]
    ProcessDead(ProcessId),
    #[error("mailbox full for: {0}")]
    MailboxFull(ProcessId),
    #[error("node unreachable: {0}")]
    NodeUnreachable(u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn process_id_equality() {
        let a = ProcessId::new(1, 42);
        let b = ProcessId::new(1, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn process_id_inequality_different_node() {
        assert_ne!(ProcessId::new(1, 42), ProcessId::new(2, 42));
    }

    #[test]
    fn process_id_inequality_different_local() {
        assert_ne!(ProcessId::new(1, 42), ProcessId::new(1, 43));
    }

    #[test]
    fn process_id_is_copy() {
        let a = ProcessId::new(1, 42);
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn process_id_display() {
        assert_eq!(format!("{}", ProcessId::new(1, 42)), "<1.42>");
    }

    #[test]
    fn process_id_hash_dedup() {
        let mut set = HashSet::new();
        set.insert(ProcessId::new(1, 1));
        set.insert(ProcessId::new(1, 1));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn process_id_accessors() {
        let pid = ProcessId::new(5, 99);
        assert_eq!(pid.node_id(), 5);
        assert_eq!(pid.local_id(), 99);
    }

    #[test]
    fn process_id_debug() {
        let pid = ProcessId::new(1, 1);
        let debug = format!("{:?}", pid);
        assert!(debug.contains("ProcessId"));
    }

    #[test]
    fn message_creation() {
        let from = ProcessId::new(1, 1);
        let payload = rmpv::Value::String("hello".into());
        let msg = Message::new(from, payload.clone());
        assert_eq!(msg.from(), from);
        assert_eq!(*msg.payload(), payload);
        assert!(msg.timestamp().unwrap() > 0);
    }

    #[test]
    fn message_with_map_payload() {
        let from = ProcessId::new(1, 1);
        let payload = rmpv::Value::Map(vec![(
            rmpv::Value::String("key".into()),
            rmpv::Value::Integer(42.into()),
        )]);
        let msg = Message::new(from, payload.clone());
        assert_eq!(*msg.payload(), payload);
    }

    #[test]
    fn message_with_nil_payload() {
        let msg = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        assert_eq!(*msg.payload(), rmpv::Value::Nil);
    }

    #[test]
    fn message_with_binary_payload() {
        let data = rmpv::Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let msg = Message::new(ProcessId::new(1, 1), data.clone());
        assert_eq!(*msg.payload(), data);
    }

    #[test]
    fn message_with_nested_array() {
        let payload = rmpv::Value::Array(vec![
            rmpv::Value::Integer(1.into()),
            rmpv::Value::Array(vec![rmpv::Value::Integer(2.into())]),
        ]);
        let msg = Message::new(ProcessId::new(1, 1), payload.clone());
        assert_eq!(*msg.payload(), payload);
    }

    #[test]
    fn message_new_internal_has_no_timestamp() {
        let msg = Message::new_internal(ProcessId::new(1, 1), rmpv::Value::Nil);
        assert!(msg.timestamp().is_none());
        assert_eq!(msg.from(), ProcessId::new(1, 1));
    }

    #[test]
    fn exit_reason_normal_is_normal() {
        assert!(ExitReason::Normal.is_normal());
    }

    #[test]
    fn exit_reason_abnormal_is_not_normal() {
        assert!(!ExitReason::Abnormal("panicked".into()).is_normal());
    }

    #[test]
    fn exit_reason_kill_is_not_normal() {
        assert!(!ExitReason::Kill.is_normal());
    }

    #[test]
    fn exit_reason_linked_exit() {
        let reason = ExitReason::LinkedExit(
            ProcessId::new(1, 5),
            Box::new(ExitReason::Abnormal("crash".into())),
        );
        assert!(!reason.is_normal());
    }

    #[test]
    fn send_error_display() {
        let err = SendError::ProcessDead(ProcessId::new(1, 5));
        let msg = format!("{}", err);
        assert!(msg.contains("1") && msg.contains("5"));
    }
}
