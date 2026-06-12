use std::sync::Arc;

use tokio::sync::mpsc;

use crate::protocol::{Frame, MsgType};
use rebar_core::process::table::ProcessTable;
use rebar_core::process::{Message, ProcessId, SendError};
use rebar_core::router::MessageRouter;

/// Commands sent to the remote transport layer for cross-node delivery.
#[derive(Debug)]
pub enum RouterCommand {
    Send { node_id: u64, frame: Frame },
}

/// A distributed message router that delivers messages locally when the
/// target is on this node, or encodes them as frames and forwards to
/// the remote transport layer for cross-node delivery.
pub struct DistributedRouter {
    node_id: u64,
    table: Arc<ProcessTable>,
    remote_tx: mpsc::Sender<RouterCommand>,
}

impl DistributedRouter {
    #[must_use]
    pub const fn new(
        node_id: u64,
        table: Arc<ProcessTable>,
        remote_tx: mpsc::Sender<RouterCommand>,
    ) -> Self {
        Self {
            node_id,
            table,
            remote_tx,
        }
    }
}

impl MessageRouter for DistributedRouter {
    fn route(&self, from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        if to.node_id() == self.node_id {
            // Local delivery
            let msg = Message::new(from, payload);
            self.table.send(to, msg)
        } else {
            // Remote delivery: encode as frame and send to transport
            let frame = encode_send_frame(from, to, payload);
            self.remote_tx
                .try_send(RouterCommand::Send {
                    node_id: to.node_id(),
                    frame,
                })
                .map_err(|_| SendError::NodeUnreachable(to.node_id()))
        }
    }
}

/// Encode a send message into a wire protocol frame.
#[must_use]
pub fn encode_send_frame(from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Frame {
    Frame {
        version: 1,
        msg_type: MsgType::Send,
        request_id: 0,
        header: rmpv::Value::Map(vec![
            (
                rmpv::Value::String("from_node".into()),
                rmpv::Value::Integer(from.node_id().into()),
            ),
            (
                rmpv::Value::String("from_local".into()),
                rmpv::Value::Integer(from.local_id().into()),
            ),
            (
                rmpv::Value::String("to_node".into()),
                rmpv::Value::Integer(to.node_id().into()),
            ),
            (
                rmpv::Value::String("to_local".into()),
                rmpv::Value::Integer(to.local_id().into()),
            ),
        ]),
        payload,
    }
}

/// Errors from delivering an inbound frame to a local process.
#[derive(Debug, thiserror::Error)]
pub enum DeliverError {
    /// The frame came off the wire from an untrusted peer and is malformed
    /// (header is not a map, or an addressing field has the wrong type).
    #[error("malformed inbound frame: {0}")]
    Malformed(&'static str),
    /// The frame was well-formed but the destination could not be reached.
    #[error(transparent)]
    Send(#[from] SendError),
}

/// Deliver an inbound frame from the network to a local process.
///
/// Extracts addressing information from the frame header and delivers the
/// payload to the target process's mailbox via the process table.
///
/// The frame header is fully attacker-controlled, so this never panics on
/// malformed input — it returns [`DeliverError::Malformed`] instead.
///
/// # Errors
///
/// Returns [`DeliverError::Malformed`] if the header is not a map or an
/// addressing field is not a `u64`, or [`DeliverError::Send`] if the target
/// process is not reachable.
pub fn deliver_inbound_frame(table: &ProcessTable, frame: &Frame) -> Result<(), DeliverError> {
    let header = frame
        .header
        .as_map()
        .ok_or(DeliverError::Malformed("header is not a map"))?;

    let mut from_node: u64 = 0;
    let mut from_local: u64 = 0;
    let mut to_node: u64 = 0;
    let mut to_local: u64 = 0;

    for (key, value) in header {
        let Some(key_str) = key.as_str() else { continue };
        let slot = match key_str {
            "from_node" => &mut from_node,
            "from_local" => &mut from_local,
            "to_node" => &mut to_node,
            "to_local" => &mut to_local,
            _ => continue,
        };
        *slot = value
            .as_u64()
            .ok_or(DeliverError::Malformed("addressing field is not a u64"))?;
    }

    let from = ProcessId::new(from_node, from_local);
    let to = ProcessId::new(to_node, to_local);
    let msg = Message::new(from, frame.payload.clone());
    table.send(to, msg).map_err(DeliverError::Send)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebar_core::process::mailbox::Mailbox;
    use rebar_core::process::table::ProcessHandle;

    #[test]
    fn distributed_router_routes_local() {
        let table = Arc::new(ProcessTable::new(1));
        let pid = table.allocate_pid();
        let (tx, mut rx) = Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));

        let (remote_tx, _remote_rx) = mpsc::channel(10);
        let router = DistributedRouter::new(1, Arc::clone(&table), remote_tx);

        let from = ProcessId::new(1, 0);
        router
            .route(from, pid, rmpv::Value::String("local-msg".into()))
            .unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "local-msg");
    }

    #[tokio::test]
    async fn distributed_router_routes_remote() {
        let table = Arc::new(ProcessTable::new(1));
        let (remote_tx, mut remote_rx) = mpsc::channel(10);
        let router = DistributedRouter::new(1, Arc::clone(&table), remote_tx);

        let from = ProcessId::new(1, 5);
        let remote_pid = ProcessId::new(2, 10);

        router
            .route(from, remote_pid, rmpv::Value::String("remote-msg".into()))
            .unwrap();

        let cmd = remote_rx.recv().await.unwrap();
        match cmd {
            RouterCommand::Send { node_id, frame } => {
                assert_eq!(node_id, 2);
                assert_eq!(frame.msg_type, MsgType::Send);
                assert_eq!(frame.payload, rmpv::Value::String("remote-msg".into()));
            }
        }
    }

    #[test]
    fn distributed_router_node_unreachable() {
        let table = Arc::new(ProcessTable::new(1));
        let (remote_tx, _remote_rx) = mpsc::channel(1); // capacity 1
        let router = DistributedRouter::new(1, Arc::clone(&table), remote_tx);

        let from = ProcessId::new(1, 0);
        let remote_pid = ProcessId::new(2, 1);
        router.route(from, remote_pid, rmpv::Value::Nil).unwrap(); // fills channel

        let result = router.route(from, remote_pid, rmpv::Value::Nil);
        assert!(matches!(result, Err(SendError::NodeUnreachable(2))));
    }

    #[test]
    fn inbound_frame_delivers_to_local_process() {
        let table = Arc::new(ProcessTable::new(2));
        let pid = table.allocate_pid();
        let (tx, mut rx) = Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));

        let frame = encode_send_frame(
            ProcessId::new(1, 5),
            pid,
            rmpv::Value::String("from-remote".into()),
        );

        deliver_inbound_frame(&table, &frame).unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "from-remote");
        assert_eq!(msg.from().node_id(), 1);
        assert_eq!(msg.from().local_id(), 5);
    }

    #[test]
    fn malformed_inbound_frame_returns_error_not_panic() {
        let table = Arc::new(ProcessTable::new(2));

        // Header is not a map (attacker-controlled): must not panic.
        let bad_header = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::String("not a map".into()),
            payload: rmpv::Value::Nil,
        };
        assert!(matches!(
            deliver_inbound_frame(&table, &bad_header),
            Err(DeliverError::Malformed(_))
        ));

        // Addressing field has the wrong type: must not panic.
        let bad_field = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Map(vec![(
                rmpv::Value::String("to_local".into()),
                rmpv::Value::String("not a u64".into()),
            )]),
            payload: rmpv::Value::Nil,
        };
        assert!(matches!(
            deliver_inbound_frame(&table, &bad_field),
            Err(DeliverError::Malformed(_))
        ));
    }

    #[test]
    fn encode_send_frame_has_correct_fields() {
        let from = ProcessId::new(1, 5);
        let to = ProcessId::new(2, 10);
        let frame = encode_send_frame(from, to, rmpv::Value::Integer(42.into()));

        assert_eq!(frame.version, 1);
        assert_eq!(frame.msg_type, MsgType::Send);
        assert_eq!(frame.payload, rmpv::Value::Integer(42.into()));
        let header = frame.header.as_map().unwrap();
        assert_eq!(header.len(), 4);
    }
}
