use crate::process::table::ProcessTable;
use crate::process::{Message, ProcessId, SendError};
use std::sync::Arc;
use tracing::instrument;

/// Trait for routing messages between processes.
/// Implementations decide whether to deliver locally or over the network.
pub trait MessageRouter: Send + Sync {
    fn route(&self, from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Result<(), SendError>;
}

/// Default router that delivers messages to the local ProcessTable.
pub struct LocalRouter {
    table: Arc<ProcessTable>,
}

impl LocalRouter {
    pub fn new(table: Arc<ProcessTable>) -> Self {
        Self { table }
    }
}

impl MessageRouter for LocalRouter {
    #[instrument(level = "trace", skip(self, payload))]
    fn route(&self, from: ProcessId, to: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        let msg = Message::new_internal(from, payload);
        self.table.send(to, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::mailbox::Mailbox;
    use crate::process::table::ProcessHandle;

    #[test]
    fn local_router_delivers_locally() {
        let table = Arc::new(ProcessTable::new(1));
        let pid = table.allocate_pid();
        let (tx, mut rx) = Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));

        let router = LocalRouter::new(table);
        let from = ProcessId::new(1, 0);
        router
            .route(from, pid, rmpv::Value::String("hello".into()))
            .unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "hello");
    }

    #[test]
    fn local_router_rejects_unknown_pid() {
        let table = Arc::new(ProcessTable::new(1));
        let router = LocalRouter::new(table);
        let from = ProcessId::new(1, 0);
        let dead_pid = ProcessId::new(1, 999);

        let result = router.route(from, dead_pid, rmpv::Value::Nil);
        assert!(matches!(result, Err(SendError::ProcessDead(_))));
    }

    #[test]
    fn local_router_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LocalRouter>();
    }
}
