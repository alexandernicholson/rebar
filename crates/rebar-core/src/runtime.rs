use std::future::Future;
use std::sync::Arc;

use crate::process::mailbox::{Mailbox, MailboxRx};
use crate::process::table::{ProcessHandle, ProcessTable};
use crate::process::{Message, ProcessId, SendError};

/// Context provided to each spawned process, giving it access to its own
/// PID, mailbox, and the ability to send messages to other processes.
pub struct ProcessContext {
    pid: ProcessId,
    rx: MailboxRx,
    table: Arc<ProcessTable>,
}

impl ProcessContext {
    /// Return this process's own PID.
    pub fn self_pid(&self) -> ProcessId {
        self.pid
    }

    /// Receive the next message from this process's mailbox.
    ///
    /// Returns `None` if the mailbox is closed (all senders dropped).
    pub async fn recv(&mut self) -> Option<Message> {
        self.rx.recv().await
    }

    /// Receive a message with a timeout.
    ///
    /// Returns `Some(msg)` if a message arrives within the duration,
    /// or `None` if the timeout expires or the mailbox is closed.
    pub async fn recv_timeout(&mut self, duration: std::time::Duration) -> Option<Message> {
        self.rx.recv_timeout(duration).await
    }

    /// Send a message to another process by PID.
    pub async fn send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        let msg = Message::new(self.pid, payload);
        self.table.send(dest, msg)
    }
}

/// The Rebar runtime, responsible for spawning processes and routing messages.
pub struct Runtime {
    node_id: u64,
    table: Arc<ProcessTable>,
}

impl Runtime {
    /// Create a new runtime for the given node ID.
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            table: Arc::new(ProcessTable::new(node_id)),
        }
    }

    /// Return this runtime's node ID.
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Spawn a new process that runs the given async handler.
    ///
    /// The handler receives a `ProcessContext` and can use it to send/receive
    /// messages. Returns the new process's PID.
    ///
    /// The spawned task is wrapped so that panics are caught and do not
    /// crash the runtime. After the handler completes (normally or via panic),
    /// the process is removed from the process table.
    pub async fn spawn<F, Fut>(&self, handler: F) -> ProcessId
    where
        F: FnOnce(ProcessContext) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let pid = self.table.allocate_pid();
        let (tx, rx) = Mailbox::unbounded();

        let handle = ProcessHandle::new(tx);
        self.table.insert(pid, handle);

        let ctx = ProcessContext {
            pid,
            rx,
            table: Arc::clone(&self.table),
        };

        let table = Arc::clone(&self.table);

        // Spawn a wrapper task that catches panics via the JoinHandle.
        // tokio::spawn catches panics in the spawned task and returns
        // JoinError instead of propagating them, so we spawn the handler
        // inside an inner task and await its JoinHandle.
        tokio::spawn(async move {
            let inner = tokio::spawn(handler(ctx));
            // Whether the handler completes normally or panics,
            // we always clean up by removing from the process table.
            let _ = inner.await;
            table.remove(&pid);
        });

        pid
    }

    /// Send a message to a process by PID from outside any process context.
    ///
    /// Uses a synthetic PID of <node_id, 0> as the sender.
    pub async fn send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        let from = ProcessId::new(self.node_id, 0);
        let msg = Message::new(from, payload);
        self.table.send(dest, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_returns_pid() {
        let rt = Runtime::new(1);
        let pid = rt.spawn(|_ctx| async {}).await;
        assert_eq!(pid.node_id(), 1);
        assert_eq!(pid.local_id(), 1);
    }

    #[tokio::test]
    async fn spawn_multiple_unique_pids() {
        let rt = Runtime::new(1);
        let pid1 = rt.spawn(|_ctx| async {}).await;
        let pid2 = rt.spawn(|_ctx| async {}).await;
        assert_ne!(pid1, pid2);
    }

    #[tokio::test]
    async fn send_message_between_processes() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let receiver = rt
            .spawn(move |mut ctx| async move {
                let msg = ctx.recv().await.unwrap();
                done_tx
                    .send(msg.payload().as_str().unwrap().to_string())
                    .unwrap();
            })
            .await;
        rt.spawn(move |ctx| async move {
            ctx.send(receiver, rmpv::Value::String("hello".into()))
                .await
                .unwrap();
        })
        .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn self_pid_is_correct() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let pid = rt
            .spawn(move |ctx| async move {
                done_tx.send(ctx.self_pid()).unwrap();
            })
            .await;
        let reported = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pid, reported);
    }

    #[tokio::test]
    async fn send_to_dead_process_returns_error() {
        let rt = Runtime::new(1);
        let result = rt.send(ProcessId::new(1, 999), rmpv::Value::Nil).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recv_timeout_in_process() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |mut ctx| async move {
            let result = ctx.recv_timeout(std::time::Duration::from_millis(10)).await;
            done_tx.send(result.is_none()).unwrap();
        })
        .await;
        let was_none = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(was_none);
    }

    #[tokio::test]
    async fn process_can_send_to_self() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |mut ctx| async move {
            let me = ctx.self_pid();
            ctx.send(me, rmpv::Value::String("self-msg".into()))
                .await
                .unwrap();
            let msg = ctx.recv().await.unwrap();
            done_tx
                .send(msg.payload().as_str().unwrap().to_string())
                .unwrap();
        })
        .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, "self-msg");
    }

    #[tokio::test]
    async fn chain_of_three_processes() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let c = rt
            .spawn(move |mut ctx| async move {
                let msg = ctx.recv().await.unwrap();
                done_tx.send(msg.payload().as_u64().unwrap()).unwrap();
            })
            .await;
        let b = rt
            .spawn(move |mut ctx| async move {
                let msg = ctx.recv().await.unwrap();
                let val = msg.payload().as_u64().unwrap();
                ctx.send(c, rmpv::Value::Integer((val + 1).into()))
                    .await
                    .unwrap();
            })
            .await;
        rt.spawn(move |ctx| async move {
            ctx.send(b, rmpv::Value::Integer(1u64.into()))
                .await
                .unwrap();
        })
        .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, 2);
    }

    #[tokio::test]
    async fn fan_out_fan_in() {
        let rt = Runtime::new(1);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let mut workers = Vec::new();
        for _ in 0..5 {
            let tx = tx.clone();
            let pid = rt
                .spawn(move |mut ctx| async move {
                    let msg = ctx.recv().await.unwrap();
                    let val = msg.payload().as_u64().unwrap();
                    tx.send(val * 2).await.unwrap();
                })
                .await;
            workers.push(pid);
        }
        drop(tx);
        rt.spawn(move |ctx| async move {
            for (i, pid) in workers.iter().enumerate() {
                ctx.send(*pid, rmpv::Value::Integer((i as u64).into()))
                    .await
                    .unwrap();
            }
        })
        .await;
        let mut results = Vec::new();
        while let Ok(Some(val)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
        {
            results.push(val);
        }
        results.sort();
        assert_eq!(results, vec![0, 2, 4, 6, 8]);
    }

    #[tokio::test]
    async fn spawn_100_processes() {
        let rt = Runtime::new(1);
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        for i in 0..100u64 {
            let tx = tx.clone();
            rt.spawn(move |_ctx| async move {
                tx.send(i).await.unwrap();
            })
            .await;
        }
        drop(tx);
        let mut count = 0;
        while let Ok(Some(_)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
        {
            count += 1;
        }
        assert_eq!(count, 100);
    }

    #[tokio::test]
    async fn process_panic_does_not_crash_runtime() {
        let rt = Runtime::new(1);
        rt.spawn(|_ctx| async move {
            panic!("intentional panic");
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |_ctx| async move {
            done_tx.send(42u64).unwrap();
        })
        .await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn node_id_accessor() {
        let rt = Runtime::new(42);
        assert_eq!(rt.node_id(), 42);
    }

    #[tokio::test]
    async fn multiple_messages_to_same_process() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let receiver = rt
            .spawn(move |mut ctx| async move {
                let mut sum = 0u64;
                for _ in 0..5 {
                    let msg = ctx.recv().await.unwrap();
                    sum += msg.payload().as_u64().unwrap();
                }
                done_tx.send(sum).unwrap();
            })
            .await;
        for i in 1..=5u64 {
            rt.send(receiver, rmpv::Value::Integer(i.into()))
                .await
                .unwrap();
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, 15);
    }

    #[tokio::test]
    async fn process_context_send_returns_error_for_dead_target() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |ctx| async move {
            let result = ctx.send(ProcessId::new(1, 999), rmpv::Value::Nil).await;
            done_tx.send(result.is_err()).unwrap();
        })
        .await;
        let was_err = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(was_err);
    }
}
