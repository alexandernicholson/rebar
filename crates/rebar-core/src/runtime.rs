use std::future::Future;
use std::sync::Arc;

#[cfg(feature = "tracing")]
use tracing::instrument;

use crate::process::mailbox::{Mailbox, MailboxRx};
use crate::process::table::{ProcessHandle, ProcessTable};
use crate::process::{Message, ProcessId, SendError};
use crate::router::{LocalRouter, MessageRouter, RouterKind};

/// Context provided to each spawned process, giving it access to its own
/// PID, mailbox, and the ability to send messages to other processes.
pub struct ProcessContext {
    pid: ProcessId,
    rx: MailboxRx,
    router: Arc<RouterKind>,
}

impl ProcessContext {
    /// Return this process's own PID.
    #[must_use]
    pub const fn self_pid(&self) -> ProcessId {
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
    ///
    /// # Errors
    ///
    /// Returns `SendError` if the destination process is not reachable.
    #[allow(clippy::unused_async)]
    pub async fn send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        self.router.route(self.pid, dest, payload)
    }

    /// Send a message to self after `delay`.
    ///
    /// The message will be delivered to this process's mailbox after the delay.
    /// Returns a [`TimerRef`](crate::timer::TimerRef) that can be used to cancel the timer.
    #[must_use]
    pub fn send_after(
        &self,
        payload: rmpv::Value,
        delay: std::time::Duration,
    ) -> crate::timer::TimerRef {
        let router: Arc<dyn crate::router::MessageRouter> = self.router.clone();
        crate::timer::send_after(router, self.pid, self.pid, payload, delay)
    }

    /// Send a message to another process after `delay`.
    #[must_use]
    pub fn send_after_to(
        &self,
        dest: ProcessId,
        payload: rmpv::Value,
        delay: std::time::Duration,
    ) -> crate::timer::TimerRef {
        let router: Arc<dyn crate::router::MessageRouter> = self.router.clone();
        crate::timer::send_after(router, self.pid, dest, payload, delay)
    }

    /// Send a message to self at regular intervals.
    #[must_use]
    pub fn send_interval(
        &self,
        payload: rmpv::Value,
        interval: std::time::Duration,
    ) -> crate::timer::TimerRef {
        let router: Arc<dyn crate::router::MessageRouter> = self.router.clone();
        crate::timer::send_interval(router, self.pid, self.pid, payload, interval)
    }
}

/// The Rebar runtime, responsible for spawning processes and routing messages.
pub struct Runtime {
    node_id: u64,
    table: Arc<ProcessTable>,
    router: Arc<RouterKind>,
}

impl Runtime {
    /// Create a new runtime for the given node ID.
    #[must_use]
    pub fn new(node_id: u64) -> Self {
        let table = Arc::new(ProcessTable::new(node_id));
        let router = Arc::new(RouterKind::Local(LocalRouter::new(Arc::clone(&table))));
        Self {
            node_id,
            table,
            router,
        }
    }

    /// Create a runtime with a custom message router.
    pub fn with_router(
        node_id: u64,
        table: Arc<ProcessTable>,
        router: Arc<dyn MessageRouter>,
    ) -> Self {
        Self {
            node_id,
            table,
            router: Arc::new(RouterKind::Custom(router)),
        }
    }

    /// Return a reference to the process table.
    #[must_use]
    pub const fn table(&self) -> &Arc<ProcessTable> {
        &self.table
    }

    /// Return this runtime's node ID.
    #[must_use]
    pub const fn node_id(&self) -> u64 {
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
    #[cfg_attr(feature = "tracing", instrument(level = "trace", skip(self, handler), fields(node_id = self.node_id)))]
    #[allow(clippy::unused_async)]
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
            router: Arc::clone(&self.router),
        };

        let table = Arc::clone(&self.table);

        // Single spawn with drop guard for panic-safe cleanup.
        // The guard removes the process from the table when dropped,
        // which happens on both normal completion and panic unwind.
        // This replaces the previous double-spawn pattern, saving one
        // task allocation and one scheduler round-trip per spawn.
        tokio::spawn(async move {
            struct CleanupGuard {
                table: Arc<ProcessTable>,
                pid: ProcessId,
            }
            impl Drop for CleanupGuard {
                fn drop(&mut self) {
                    // Remove from table but defer the expensive channel
                    // deallocation (mpsc block freeing) off the hot path
                    // by moving the ProcessHandle to a spawned task.
                    if let Some((_pid, handle)) = self.table.remove(&self.pid) {
                        tokio::spawn(async move {
                            drop(handle);
                        });
                    }
                }
            }
            let _guard = CleanupGuard { table, pid };
            handler(ctx).await;
        });

        pid
    }

    /// Send a message to a process by PID from outside any process context.
    ///
    /// Uses a synthetic PID of `<node_id, 0>` as the sender.
    ///
    /// # Errors
    ///
    /// Returns `SendError` if the destination process is not reachable.
    #[cfg_attr(feature = "tracing", instrument(level = "trace", skip(self, payload)))]
    #[allow(clippy::unused_async)]
    pub async fn send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError> {
        let from = ProcessId::new(self.node_id, 0);
        self.router.route(from, dest, payload)
    }
}

/// Builder for creating a rebar Runtime with a configured tokio Runtime.
pub struct RuntimeBuilder {
    node_id: u64,
    worker_threads: Option<usize>,
    thread_name: Option<String>,
}

impl RuntimeBuilder {
    /// Create a new builder for the given node ID.
    #[must_use]
    pub const fn new(node_id: u64) -> Self {
        Self {
            node_id,
            worker_threads: None,
            thread_name: None,
        }
    }

    /// Set the number of worker threads. Defaults to the number of CPU cores.
    #[must_use]
    pub const fn worker_threads(mut self, n: usize) -> Self {
        self.worker_threads = Some(n);
        self
    }

    /// Set the thread name prefix for worker threads.
    #[must_use]
    pub fn thread_name(mut self, name: impl Into<String>) -> Self {
        self.thread_name = Some(name.into());
        self
    }

    /// Build and return a (tokio Runtime, rebar Runtime) pair.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the tokio runtime fails to build.
    pub fn build(self) -> Result<(tokio::runtime::Runtime, Runtime), std::io::Error> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.enable_all();

        if let Some(n) = self.worker_threads {
            builder.worker_threads(n);
        }
        if let Some(name) = &self.thread_name {
            builder.thread_name(name);
        }

        let tokio_rt = builder.build()?;
        let rebar_rt = Runtime::new(self.node_id);

        Ok((tokio_rt, rebar_rt))
    }

    /// Build a runtime and run a future on it. Blocks until the future completes.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the tokio runtime fails to build.
    pub fn start<F, Fut>(self, f: F) -> Result<(), std::io::Error>
    where
        F: FnOnce(Arc<Runtime>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tokio_rt, rebar_rt) = self.build()?;
        let rt = Arc::new(rebar_rt);
        tokio_rt.block_on(f(rt));
        Ok(())
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
        results.sort_unstable();
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
    async fn spawn_cleanup_after_normal_exit() {
        let rt = Runtime::new(1);
        let pid = rt.spawn(|_ctx| async {}).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(rt.table().get(&pid).is_none());
    }

    #[tokio::test]
    async fn spawn_cleanup_after_panic() {
        let rt = Runtime::new(1);
        let pid = rt.spawn(|_ctx| async { panic!("test panic") }).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(rt.table().get(&pid).is_none());
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

    #[tokio::test]
    async fn runtime_with_custom_router() {
        use crate::router::MessageRouter;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingRouter {
            count: AtomicU64,
            inner: crate::router::LocalRouter,
        }
        impl MessageRouter for CountingRouter {
            fn route(
                &self,
                from: ProcessId,
                to: ProcessId,
                payload: rmpv::Value,
            ) -> Result<(), SendError> {
                self.count.fetch_add(1, Ordering::Relaxed);
                self.inner.route(from, to, payload)
            }
        }

        let table = Arc::new(crate::process::table::ProcessTable::new(1));
        let router = Arc::new(CountingRouter {
            count: AtomicU64::new(0),
            inner: crate::router::LocalRouter::new(Arc::clone(&table)),
        });
        let counter_ref = Arc::clone(&router);

        let rt = Runtime::with_router(1, Arc::clone(&table), router as Arc<dyn MessageRouter>);
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
            ctx.send(receiver, rmpv::Value::String("routed".into()))
                .await
                .unwrap();
        })
        .await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, "routed");
        assert!(counter_ref.count.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn runtime_builder_default_builds() {
        let (tokio_rt, rebar_rt) = RuntimeBuilder::new(1).build().unwrap();
        assert_eq!(rebar_rt.node_id(), 1);
        tokio_rt.block_on(async {
            let pid = rebar_rt.spawn(|_ctx| async {}).await;
            assert_eq!(pid.node_id(), 1);
        });
    }

    #[test]
    fn runtime_builder_custom_threads() {
        let (tokio_rt, rebar_rt) = RuntimeBuilder::new(2)
            .worker_threads(2)
            .build()
            .unwrap();
        assert_eq!(rebar_rt.node_id(), 2);
        tokio_rt.block_on(async {
            let pid = rebar_rt.spawn(|_ctx| async {}).await;
            assert_eq!(pid.node_id(), 2);
        });
    }

    #[test]
    fn runtime_builder_thread_name() {
        let (tokio_rt, _rebar_rt) = RuntimeBuilder::new(1)
            .thread_name("rebar-test")
            .build()
            .unwrap();
        drop(tokio_rt);
    }

    #[test]
    fn runtime_builder_start_runs_closure() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let ran = Arc::new(AtomicBool::new(false));
        let ran_clone = Arc::clone(&ran);

        RuntimeBuilder::new(1)
            .start(move |rt| async move {
                ran_clone.store(true, Ordering::SeqCst);
                let pid = rt.spawn(|_ctx| async {}).await;
                assert_eq!(pid.node_id(), 1);
            })
            .unwrap();

        assert!(ran.load(Ordering::SeqCst));
    }
}
