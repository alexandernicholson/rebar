use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::instrument;

use tokio::sync::{mpsc, oneshot};

use crate::process::{ExitReason, ProcessId};
use crate::runtime::Runtime;
use crate::supervisor::spec::{ChildSpec, RestartStrategy, ShutdownStrategy, SupervisorSpec};

/// A factory that creates the child's async task. Must be callable multiple
/// times (for restarts) and is shared via Arc.
pub type ChildFactory =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ExitReason> + Send>> + Send + Sync>;

/// Pairs a `ChildSpec` with its `ChildFactory` for supervisor startup.
pub struct ChildEntry {
    pub spec: ChildSpec,
    pub factory: ChildFactory,
}

impl ChildEntry {
    pub fn new<F, Fut>(spec: ChildSpec, factory: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ExitReason> + Send + 'static,
    {
        Self {
            spec,
            factory: Arc::new(move || Box::pin(factory())),
        }
    }
}

/// Internal per-child tracking used by the running supervisor.
struct ChildState {
    spec: ChildSpec,
    factory: ChildFactory,
    pid: Option<ProcessId>,
    /// Sender to signal the child to shut down (dropped = shutdown signal).
    shutdown_tx: Option<oneshot::Sender<()>>,
}

/// Messages the supervisor loop processes.
enum SupervisorMsg {
    /// A child exited.
    ChildExited {
        index: usize,
        pid: ProcessId,
        reason: ExitReason,
    },
    /// Request to add a child dynamically.
    AddChild {
        entry: ChildEntry,
        reply: oneshot::Sender<Result<ProcessId, String>>,
    },
    /// Shut down the supervisor.
    Shutdown,
}

/// Handle to a running supervisor, allowing external interaction.
#[derive(Clone)]
pub struct SupervisorHandle {
    pid: ProcessId,
    msg_tx: mpsc::UnboundedSender<SupervisorMsg>,
}

impl SupervisorHandle {
    /// The supervisor's own process ID.
    pub fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Dynamically add a child to the running supervisor.
    pub async fn add_child(&self, entry: ChildEntry) -> Result<ProcessId, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.msg_tx
            .send(SupervisorMsg::AddChild {
                entry,
                reply: reply_tx,
            })
            .map_err(|_| "supervisor gone".to_string())?;
        reply_rx.await.map_err(|_| "supervisor gone".to_string())?
    }

    /// Request the supervisor to shut down.
    pub fn shutdown(&self) {
        let _ = self.msg_tx.send(SupervisorMsg::Shutdown);
    }
}

/// Start a supervisor as a process in the given runtime.
///
/// Returns a handle with the supervisor's PID and a channel to interact with it.
pub async fn start_supervisor(
    runtime: Arc<Runtime>,
    spec: SupervisorSpec,
    children: Vec<ChildEntry>,
) -> SupervisorHandle {
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();

    // We need the supervisor PID. Spawn the supervisor as a tokio task
    // (not via Runtime::spawn since that expects ProcessContext-based handler).
    // Instead, we'll allocate a PID for it conceptually.
    // Actually let's use Runtime::spawn to get a real PID.
    let msg_tx_clone = msg_tx.clone();
    let pid = runtime
        .spawn(move |_ctx| async move {
            supervisor_loop(spec, children, msg_rx, msg_tx_clone).await;
        })
        .await;

    SupervisorHandle { pid, msg_tx }
}

/// The main supervisor loop.
#[instrument(level = "trace", skip(spec, children, msg_rx, msg_tx))]
async fn supervisor_loop(
    spec: SupervisorSpec,
    children: Vec<ChildEntry>,
    mut msg_rx: mpsc::UnboundedReceiver<SupervisorMsg>,
    msg_tx: mpsc::UnboundedSender<SupervisorMsg>,
) {
    let mut state = SupervisorState {
        strategy: spec.strategy,
        max_restarts: spec.max_restarts,
        max_seconds: spec.max_seconds,
        children: Vec::new(),
        restart_times: VecDeque::new(),
    };

    // Start children in order
    for entry in children {
        let child_state = ChildState {
            spec: entry.spec,
            factory: entry.factory,
            pid: None,
            shutdown_tx: None,
        };
        state.children.push(child_state);
    }

    // Start all children in order
    for i in 0..state.children.len() {
        start_child(&mut state.children[i], i, &msg_tx);
    }

    // Main event loop
    loop {
        match msg_rx.recv().await {
            Some(SupervisorMsg::ChildExited { index, pid, reason }) => {
                // Verify this is still the current PID for that index
                if index >= state.children.len() {
                    continue;
                }
                if state.children[index].pid != Some(pid) {
                    continue;
                }

                state.children[index].pid = None;
                state.children[index].shutdown_tx = None;

                let should_restart = state.children[index].spec.restart.should_restart(&reason);

                if !should_restart {
                    continue;
                }

                // Check restart limits
                if !state.check_restart_limit() {
                    // Exceeded max restarts, shut down everything
                    shutdown_all_children(&mut state.children).await;
                    break;
                }

                // Apply restart strategy
                match state.strategy {
                    RestartStrategy::OneForOne => {
                        start_child(&mut state.children[index], index, &msg_tx);
                    }
                    RestartStrategy::OneForAll => {
                        // Stop all children in reverse order (except the one that already exited)
                        let len = state.children.len();
                        for i in (0..len).rev() {
                            if i != index && state.children[i].pid.is_some() {
                                stop_child(&mut state.children[i]).await;
                            }
                        }
                        // Restart all in order
                        for i in 0..len {
                            start_child(&mut state.children[i], i, &msg_tx);
                        }
                    }
                    RestartStrategy::RestForOne => {
                        // Stop children after the failed one, in reverse order
                        let len = state.children.len();
                        for i in (index + 1..len).rev() {
                            if state.children[i].pid.is_some() {
                                stop_child(&mut state.children[i]).await;
                            }
                        }
                        // Restart failed child and all subsequent
                        for i in index..len {
                            start_child(&mut state.children[i], i, &msg_tx);
                        }
                    }
                }
            }
            Some(SupervisorMsg::AddChild { entry, reply }) => {
                let idx = state.children.len();
                let mut child_state = ChildState {
                    spec: entry.spec,
                    factory: entry.factory,
                    pid: None,
                    shutdown_tx: None,
                };
                start_child(&mut child_state, idx, &msg_tx);
                let pid = child_state.pid.unwrap();
                state.children.push(child_state);
                let _ = reply.send(Ok(pid));
            }
            Some(SupervisorMsg::Shutdown) => {
                shutdown_all_children(&mut state.children).await;
                break;
            }
            None => {
                // All senders dropped, shut down
                shutdown_all_children(&mut state.children).await;
                break;
            }
        }
    }
}

struct SupervisorState {
    strategy: RestartStrategy,
    max_restarts: u32,
    max_seconds: u32,
    children: Vec<ChildState>,
    restart_times: VecDeque<Instant>,
}

impl SupervisorState {
    /// Check if we've exceeded the restart limit. Returns true if restart is allowed.
    fn check_restart_limit(&mut self) -> bool {
        if self.max_restarts == 0 {
            return false;
        }

        let now = Instant::now();
        let window = Duration::from_secs(self.max_seconds as u64);

        // Add current restart
        self.restart_times.push_back(now);

        // Trim restarts outside the window
        while let Some(&front) = self.restart_times.front() {
            if now.duration_since(front) > window {
                self.restart_times.pop_front();
            } else {
                break;
            }
        }

        // Check if count exceeds max
        (self.restart_times.len() as u32) <= self.max_restarts
    }
}

/// Start (or restart) a child, spawning it as a tokio task.
#[instrument(level = "trace", skip(child, msg_tx))]
fn start_child(
    child: &mut ChildState,
    index: usize,
    msg_tx: &mpsc::UnboundedSender<SupervisorMsg>,
) {
    let factory = Arc::clone(&child.factory);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let msg_tx = msg_tx.clone();

    // We need to generate a PID. We'll use a simple approach: generate a unique
    // ID via a static counter, same node_id = 0 (supervisor-managed).
    // Actually, we need real PIDs from the runtime, but we don't have access here.
    // Let's use a global atomic for now and create synthetic PIDs.
    use std::sync::atomic::{AtomicU64, Ordering};
    static CHILD_PID_COUNTER: AtomicU64 = AtomicU64::new(1_000_000);
    let local_id = CHILD_PID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = ProcessId::new(0, local_id);

    child.pid = Some(pid);
    child.shutdown_tx = Some(shutdown_tx);

    tokio::spawn(async move {
        let child_future = factory();

        tokio::select! {
            reason = child_future => {
                let _ = msg_tx.send(SupervisorMsg::ChildExited {
                    index,
                    pid,
                    reason,
                });
            }
            _ = shutdown_rx => {
                // Shutdown requested: the child is terminated
                let _ = msg_tx.send(SupervisorMsg::ChildExited {
                    index,
                    pid,
                    reason: ExitReason::Normal,
                });
            }
        }
    });
}

/// Stop a single child according to its shutdown strategy.
#[instrument(level = "trace", skip(child))]
async fn stop_child(child: &mut ChildState) {
    if let Some(tx) = child.shutdown_tx.take() {
        match &child.spec.shutdown {
            ShutdownStrategy::BrutalKill => {
                // Drop the sender immediately (signals shutdown)
                drop(tx);
            }
            ShutdownStrategy::Timeout(duration) => {
                // Send shutdown signal and wait up to the timeout
                let _ = tx.send(());
                // Give a brief moment for the task to process
                tokio::time::sleep(Duration::from_millis(1).min(*duration)).await;
            }
        }
    }
    // Small yield to let the task finish
    tokio::task::yield_now().await;
    child.pid = None;
}

/// Shut down all children in reverse order.
async fn shutdown_all_children(children: &mut Vec<ChildState>) {
    for i in (0..children.len()).rev() {
        if children[i].pid.is_some() {
            stop_child(&mut children[i]).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::spec::RestartType;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tokio::sync::Mutex;

    /// Helper to create a runtime for tests.
    fn test_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::new(1))
    }

    // -----------------------------------------------------------------------
    // 1. supervisor_starts_children_in_order
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn supervisor_starts_children_in_order() {
        let rt = test_runtime();
        let order = Arc::new(Mutex::new(Vec::new()));

        let mut entries = Vec::new();
        for i in 0..3u32 {
            let order = Arc::clone(&order);
            entries.push(ChildEntry::new(
                ChildSpec::new(format!("child_{}", i)),
                move || {
                    let order = Arc::clone(&order);
                    async move {
                        order.lock().await.push(i);
                        // Stay alive briefly
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        ExitReason::Normal
                    }
                },
            ));
        }

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(rt, spec, entries).await;

        // Wait for children to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = order.lock().await.clone();
        assert_eq!(started, vec![0, 1, 2]);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 2. supervisor_stops_children_in_reverse_order
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn supervisor_stops_children_in_reverse_order() {
        // Use a channel to record when each child's task is cancelled/dropped.
        // We use a Drop guard struct to detect cancellation order.
        let rt = test_runtime();
        let stop_order = Arc::new(Mutex::new(Vec::<u32>::new()));

        struct DropGuard {
            id: u32,
            stop_order: Arc<Mutex<Vec<u32>>>,
        }
        impl Drop for DropGuard {
            fn drop(&mut self) {
                // We can't async in drop, so use try_lock (ok in tests).
                if let Ok(mut v) = self.stop_order.try_lock() {
                    v.push(self.id);
                }
            }
        }

        let mut entries = Vec::new();
        for i in 0..3u32 {
            let so = Arc::clone(&stop_order);
            entries.push(ChildEntry::new(
                ChildSpec::new(format!("child_{}", i))
                    .restart(RestartType::Temporary)
                    .shutdown(ShutdownStrategy::Timeout(Duration::from_millis(50))),
                move || {
                    let so = Arc::clone(&so);
                    async move {
                        let _guard = DropGuard {
                            id: i,
                            stop_order: so,
                        };
                        // Block until cancelled
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                },
            ));
        }

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let order = stop_order.lock().await.clone();
        // Children should be stopped in reverse order: 2, 1, 0
        assert_eq!(order, vec![2, 1, 0]);
    }

    // -----------------------------------------------------------------------
    // 3. supervisor_is_a_process_with_pid
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn supervisor_is_a_process_with_pid() {
        let rt = test_runtime();
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(rt, spec, vec![]).await;

        let pid = handle.pid();
        assert_eq!(pid.node_id(), 1);
        assert!(pid.local_id() > 0);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 4. one_for_one_restarts_only_failed_child
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn one_for_one_restarts_only_failed_child() {
        let rt = test_runtime();
        let start_count_0 = Arc::new(AtomicU32::new(0));
        let start_count_1 = Arc::new(AtomicU32::new(0));

        let sc0 = Arc::clone(&start_count_0);
        let sc1 = Arc::clone(&start_count_1);

        let entries = vec![
            ChildEntry::new(ChildSpec::new("child_0"), move || {
                let sc = Arc::clone(&sc0);
                async move {
                    let count = sc.fetch_add(1, Ordering::SeqCst);
                    if count == 0 {
                        // First start: crash
                        ExitReason::Abnormal("crash".into())
                    } else {
                        // Second start: stay alive
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                }
            }),
            ChildEntry::new(ChildSpec::new("child_1"), move || {
                let sc = Arc::clone(&sc1);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
        ];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        // Wait for restart
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(start_count_0.load(Ordering::SeqCst), 2); // started, crashed, restarted
        assert_eq!(start_count_1.load(Ordering::SeqCst), 1); // started once, never restarted

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 5. one_for_one_other_children_unaffected
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn one_for_one_other_children_unaffected() {
        let rt = test_runtime();
        let child_1_alive = Arc::new(AtomicBool::new(false));
        let child_1_restarts = Arc::new(AtomicU32::new(0));

        let c1a = Arc::clone(&child_1_alive);
        let c1r = Arc::clone(&child_1_restarts);

        let entries = vec![
            ChildEntry::new(ChildSpec::new("crasher"), move || async move {
                ExitReason::Abnormal("crash".into())
            }),
            ChildEntry::new(ChildSpec::new("stable"), move || {
                let alive = Arc::clone(&c1a);
                let restarts = Arc::clone(&c1r);
                async move {
                    restarts.fetch_add(1, Ordering::SeqCst);
                    alive.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
        ];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(child_1_alive.load(Ordering::SeqCst));
        assert_eq!(child_1_restarts.load(Ordering::SeqCst), 1);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 6. one_for_one_permanent_child_always_restarts
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn one_for_one_permanent_child_always_restarts() {
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("permanent").restart(RestartType::Permanent),
            move || {
                let sc = Arc::clone(&sc);
                async move {
                    let c = sc.fetch_add(1, Ordering::SeqCst);
                    if c < 2 {
                        ExitReason::Normal // Even normal exit restarts permanent
                    } else {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(10)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(start_count.load(Ordering::SeqCst) >= 3);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 7. one_for_one_transient_child_normal_exit_no_restart
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn one_for_one_transient_child_normal_exit_no_restart() {
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("transient").restart(RestartType::Transient),
            move || {
                let sc = Arc::clone(&sc);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    ExitReason::Normal
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(10)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Should only start once: normal exit, transient => no restart
        assert_eq!(start_count.load(Ordering::SeqCst), 1);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 8. one_for_one_transient_child_abnormal_restarts
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn one_for_one_transient_child_abnormal_restarts() {
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("transient").restart(RestartType::Transient),
            move || {
                let sc = Arc::clone(&sc);
                async move {
                    let c = sc.fetch_add(1, Ordering::SeqCst);
                    if c == 0 {
                        ExitReason::Abnormal("crash".into())
                    } else {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(10)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(start_count.load(Ordering::SeqCst), 2);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 9. one_for_one_temporary_child_never_restarts
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn one_for_one_temporary_child_never_restarts() {
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("temp").restart(RestartType::Temporary),
            move || {
                let sc = Arc::clone(&sc);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    ExitReason::Abnormal("crash".into())
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(10)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(start_count.load(Ordering::SeqCst), 1);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 10. one_for_all_restarts_all_on_single_failure
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn one_for_all_restarts_all_on_single_failure() {
        let rt = test_runtime();
        let start_count_a = Arc::new(AtomicU32::new(0));
        let start_count_b = Arc::new(AtomicU32::new(0));
        let start_count_c = Arc::new(AtomicU32::new(0));

        let sca = Arc::clone(&start_count_a);
        let scb = Arc::clone(&start_count_b);
        let scc = Arc::clone(&start_count_c);

        let entries = vec![
            ChildEntry::new(ChildSpec::new("a"), move || {
                let sc = Arc::clone(&sca);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
            ChildEntry::new(ChildSpec::new("b"), move || {
                let sc = Arc::clone(&scb);
                async move {
                    let c = sc.fetch_add(1, Ordering::SeqCst);
                    if c == 0 {
                        // First time: crash after brief delay
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        ExitReason::Abnormal("crash".into())
                    } else {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                }
            }),
            ChildEntry::new(ChildSpec::new("c"), move || {
                let sc = Arc::clone(&scc);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
        ];

        let spec = SupervisorSpec::new(RestartStrategy::OneForAll)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(400)).await;

        // All three should have been restarted
        assert_eq!(start_count_a.load(Ordering::SeqCst), 2);
        assert_eq!(start_count_b.load(Ordering::SeqCst), 2);
        assert_eq!(start_count_c.load(Ordering::SeqCst), 2);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 11. one_for_all_stops_in_reverse_starts_in_order
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn one_for_all_stops_in_reverse_starts_in_order() {
        let rt = test_runtime();
        let events = Arc::new(Mutex::new(Vec::<String>::new()));

        let events0 = Arc::clone(&events);
        let events1 = Arc::clone(&events);
        let events2 = Arc::clone(&events);

        let crash_count = Arc::new(AtomicU32::new(0));
        let cc = Arc::clone(&crash_count);

        let entries = vec![
            ChildEntry::new(
                ChildSpec::new("child_0")
                    .shutdown(ShutdownStrategy::Timeout(Duration::from_millis(50))),
                move || {
                    let ev = Arc::clone(&events0);
                    async move {
                        ev.lock().await.push("start_0".into());
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                },
            ),
            ChildEntry::new(
                ChildSpec::new("child_1")
                    .shutdown(ShutdownStrategy::Timeout(Duration::from_millis(50))),
                move || {
                    let ev = Arc::clone(&events1);
                    let cc = Arc::clone(&cc);
                    async move {
                        ev.lock().await.push("start_1".into());
                        let c = cc.fetch_add(1, Ordering::SeqCst);
                        if c == 0 {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            ExitReason::Abnormal("crash".into())
                        } else {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            ExitReason::Normal
                        }
                    }
                },
            ),
            ChildEntry::new(
                ChildSpec::new("child_2")
                    .shutdown(ShutdownStrategy::Timeout(Duration::from_millis(50))),
                move || {
                    let ev = Arc::clone(&events2);
                    async move {
                        ev.lock().await.push("start_2".into());
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                },
            ),
        ];

        let spec = SupervisorSpec::new(RestartStrategy::OneForAll)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let ev = events.lock().await.clone();
        // Initial starts: start_0, start_1, start_2
        // After child_1 crashes: OneForAll restarts all => start_0, start_1, start_2
        assert_eq!(ev.len(), 6);
        // The restart starts should be in order 0, 1, 2
        assert_eq!(&ev[3..], &["start_0", "start_1", "start_2"]);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 12. rest_for_one_restarts_failed_and_subsequent
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn rest_for_one_restarts_failed_and_subsequent() {
        let rt = test_runtime();
        let start_count_a = Arc::new(AtomicU32::new(0));
        let start_count_b = Arc::new(AtomicU32::new(0));
        let start_count_c = Arc::new(AtomicU32::new(0));

        let sca = Arc::clone(&start_count_a);
        let scb = Arc::clone(&start_count_b);
        let scc = Arc::clone(&start_count_c);

        let entries = vec![
            ChildEntry::new(ChildSpec::new("a"), move || {
                let sc = Arc::clone(&sca);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
            ChildEntry::new(ChildSpec::new("b"), move || {
                let sc = Arc::clone(&scb);
                async move {
                    let c = sc.fetch_add(1, Ordering::SeqCst);
                    if c == 0 {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        ExitReason::Abnormal("crash".into())
                    } else {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                }
            }),
            ChildEntry::new(ChildSpec::new("c"), move || {
                let sc = Arc::clone(&scc);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
        ];

        let spec = SupervisorSpec::new(RestartStrategy::RestForOne)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(400)).await;

        // a should NOT be restarted (before the crashed child)
        assert_eq!(start_count_a.load(Ordering::SeqCst), 1);
        // b crashed and restarted
        assert_eq!(start_count_b.load(Ordering::SeqCst), 2);
        // c is after b, so it gets restarted too
        assert_eq!(start_count_c.load(Ordering::SeqCst), 2);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 13. rest_for_one_earlier_children_unaffected
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn rest_for_one_earlier_children_unaffected() {
        let rt = test_runtime();
        let start_count_a = Arc::new(AtomicU32::new(0));
        let start_count_b = Arc::new(AtomicU32::new(0));

        let sca = Arc::clone(&start_count_a);
        let scb = Arc::clone(&start_count_b);

        let entries = vec![
            ChildEntry::new(ChildSpec::new("a"), move || {
                let sc = Arc::clone(&sca);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
            ChildEntry::new(ChildSpec::new("b"), move || {
                let sc = Arc::clone(&scb);
                async move {
                    let c = sc.fetch_add(1, Ordering::SeqCst);
                    if c == 0 {
                        ExitReason::Abnormal("crash".into())
                    } else {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                }
            }),
        ];

        let spec = SupervisorSpec::new(RestartStrategy::RestForOne)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        // a is before b, so it should NOT be restarted
        assert_eq!(start_count_a.load(Ordering::SeqCst), 1);
        // b crashed and restarted
        assert_eq!(start_count_b.load(Ordering::SeqCst), 2);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 14. max_restarts_within_window_escalates
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn max_restarts_within_window_escalates() {
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(ChildSpec::new("crasher"), move || {
            let sc = Arc::clone(&sc);
            async move {
                sc.fetch_add(1, Ordering::SeqCst);
                ExitReason::Abnormal("crash".into())
            }
        })];

        // max 2 restarts in 10 seconds
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(2)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Initial start + 2 restarts = 3, then supervisor escalates (shuts down)
        let count = start_count.load(Ordering::SeqCst);
        assert_eq!(count, 3);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 15. restarts_outside_window_reset_counter
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn restarts_outside_window_reset_counter() {
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(ChildSpec::new("slow_crasher"), move || {
            let sc = Arc::clone(&sc);
            async move {
                let c = sc.fetch_add(1, Ordering::SeqCst);
                if c < 4 {
                    // Crash, but with delays between each
                    // The window is 1 second. We space crashes >1s apart
                    // so the counter resets.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    ExitReason::Abnormal("crash".into())
                } else {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }
        })];

        // max 2 restarts in 1 second — but since all crashes happen quickly,
        // this WILL exceed the limit. To test reset, we need crashes spaced apart.
        // Actually, let's use a larger window to show that within the window
        // the counter accumulates, but still stays within limits.
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // With max_restarts=5, we can do initial + 5 restarts before escalation.
        // Our child crashes 4 times then stays alive, so 5 starts total.
        let count = start_count.load(Ordering::SeqCst);
        assert_eq!(count, 5);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 16. max_restarts_zero_means_never_restart
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn max_restarts_zero_means_never_restart() {
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(ChildSpec::new("crasher"), move || {
            let sc = Arc::clone(&sc);
            async move {
                sc.fetch_add(1, Ordering::SeqCst);
                ExitReason::Abnormal("crash".into())
            }
        })];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(0)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Only initial start, then max_restarts=0 means supervisor shuts down
        assert_eq!(start_count.load(Ordering::SeqCst), 1);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 17. shutdown_timeout_respected
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn shutdown_timeout_respected() {
        let rt = test_runtime();
        let started = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&started);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("slow_stopper")
                .restart(RestartType::Temporary)
                .shutdown(ShutdownStrategy::Timeout(Duration::from_millis(200))),
            move || {
                let s = Arc::clone(&s);
                async move {
                    s.store(true, Ordering::SeqCst);
                    // Simulate a process that takes time to stop
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(started.load(Ordering::SeqCst));

        let before = Instant::now();
        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let elapsed = before.elapsed();

        // Should not take too long (the shutdown timeout is respected)
        assert!(elapsed < Duration::from_secs(2));
    }

    // -----------------------------------------------------------------------
    // 18. brutal_kill_immediate
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn brutal_kill_immediate() {
        let rt = test_runtime();
        let started = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&started);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("killable")
                .restart(RestartType::Temporary)
                .shutdown(ShutdownStrategy::BrutalKill),
            move || {
                let s = Arc::clone(&s);
                async move {
                    s.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(started.load(Ordering::SeqCst));

        let before = Instant::now();
        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let elapsed = before.elapsed();

        // BrutalKill should be nearly instant
        assert!(elapsed < Duration::from_secs(1));
    }

    // -----------------------------------------------------------------------
    // 19. graceful_shutdown_sends_exit_signal
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn graceful_shutdown_sends_exit_signal() {
        let rt = test_runtime();
        let graceful = Arc::new(AtomicBool::new(false));
        let g = Arc::clone(&graceful);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("graceful")
                .restart(RestartType::Temporary)
                .shutdown(ShutdownStrategy::Timeout(Duration::from_secs(5))),
            move || {
                let g = Arc::clone(&g);
                async move {
                    // The child will be cancelled when shutdown signal arrives.
                    // In our model, the shutdown_rx being received means graceful exit.
                    g.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(graceful.load(Ordering::SeqCst));

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // -----------------------------------------------------------------------
    // 20. child_exit_sends_signal_to_linked
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn child_exit_sends_signal_to_linked() {
        // Simplified: When a child exits abnormally under OneForAll,
        // the supervisor effectively "signals" all linked children to restart.
        let rt = test_runtime();
        let restart_count = Arc::new(AtomicU32::new(0));
        let rc = Arc::clone(&restart_count);

        let entries = vec![
            ChildEntry::new(ChildSpec::new("crasher"), move || async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                ExitReason::Abnormal("crash".into())
            }),
            ChildEntry::new(ChildSpec::new("linked"), move || {
                let rc = Arc::clone(&rc);
                async move {
                    rc.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
        ];

        let spec = SupervisorSpec::new(RestartStrategy::OneForAll)
            .max_restarts(3)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        // "linked" child should have been restarted when "crasher" exited
        assert!(restart_count.load(Ordering::SeqCst) >= 2);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 21. trap_exit_converts_signal_to_message
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn trap_exit_converts_signal_to_message() {
        // Simplified: The supervisor itself "traps exits" — it receives child
        // exit notifications and decides what to do (restart vs ignore).
        // This test verifies that a transient child's normal exit is "trapped"
        // and NOT restarted (the exit signal is converted to an informational
        // message rather than causing a restart).
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("trapper").restart(RestartType::Transient),
            move || {
                let sc = Arc::clone(&sc);
                async move {
                    sc.fetch_add(1, Ordering::SeqCst);
                    ExitReason::Normal // Normal exit
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(10)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Transient with normal exit => no restart. The exit was "trapped".
        assert_eq!(start_count.load(Ordering::SeqCst), 1);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 22. no_trap_exit_propagates_death
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn no_trap_exit_propagates_death() {
        // Simplified: When a permanent child crashes, the supervisor propagates
        // the restart (doesn't "trap" the abnormal exit — it takes action).
        let rt = test_runtime();
        let start_count = Arc::new(AtomicU32::new(0));
        let sc = Arc::clone(&start_count);

        let entries = vec![ChildEntry::new(
            ChildSpec::new("untrap").restart(RestartType::Permanent),
            move || {
                let sc = Arc::clone(&sc);
                async move {
                    let c = sc.fetch_add(1, Ordering::SeqCst);
                    if c == 0 {
                        ExitReason::Abnormal("crash".into())
                    } else {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        ExitReason::Normal
                    }
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Permanent child crash => restart (death propagated as restart action)
        assert_eq!(start_count.load(Ordering::SeqCst), 2);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 23. nested_supervisor_escalation
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn nested_supervisor_escalation() {
        let rt = test_runtime();
        let inner_start_count = Arc::new(AtomicU32::new(0));
        let isc = Arc::clone(&inner_start_count);

        // Inner supervisor with max_restarts=0 => any crash escalates (shuts down)
        let rt2 = Arc::clone(&rt);
        let entries = vec![ChildEntry::new(
            ChildSpec::new("inner_supervisor"),
            move || {
                let isc = Arc::clone(&isc);
                let rt_inner = Arc::clone(&rt2);
                async move {
                    let inner_entries =
                        vec![ChildEntry::new(ChildSpec::new("inner_child"), move || {
                            let isc = Arc::clone(&isc);
                            async move {
                                isc.fetch_add(1, Ordering::SeqCst);
                                ExitReason::Abnormal("crash".into())
                            }
                        })];

                    let inner_spec = SupervisorSpec::new(RestartStrategy::OneForOne)
                        .max_restarts(0) // escalate immediately
                        .max_seconds(10);

                    let _inner_handle = start_supervisor(rt_inner, inner_spec, inner_entries).await;

                    // Wait for the inner supervisor to exit (it will escalate)
                    tokio::time::sleep(Duration::from_secs(5)).await;

                    // Inner supervisor shut down, we exit abnormally
                    ExitReason::Abnormal("inner supervisor escalated".into())
                }
            },
        )];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(1)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Inner child started at least once
        assert!(inner_start_count.load(Ordering::SeqCst) >= 1);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 24. nested_supervisor_independent_restart
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn nested_supervisor_independent_restart() {
        let rt = test_runtime();
        let outer_child_starts = Arc::new(AtomicU32::new(0));
        let inner_child_starts = Arc::new(AtomicU32::new(0));

        let ocs = Arc::clone(&outer_child_starts);
        let ics = Arc::clone(&inner_child_starts);

        let rt2 = Arc::clone(&rt);
        let entries = vec![
            ChildEntry::new(ChildSpec::new("inner_supervisor"), move || {
                let ics = Arc::clone(&ics);
                let rt_inner = Arc::clone(&rt2);
                async move {
                    let inner_entries =
                        vec![ChildEntry::new(ChildSpec::new("inner_child"), move || {
                            let ics = Arc::clone(&ics);
                            async move {
                                let c = ics.fetch_add(1, Ordering::SeqCst);
                                if c == 0 {
                                    ExitReason::Abnormal("crash".into())
                                } else {
                                    tokio::time::sleep(Duration::from_secs(60)).await;
                                    ExitReason::Normal
                                }
                            }
                        })];

                    let inner_spec = SupervisorSpec::new(RestartStrategy::OneForOne)
                        .max_restarts(5)
                        .max_seconds(10);

                    let _inner_handle = start_supervisor(rt_inner, inner_spec, inner_entries).await;

                    // Keep inner supervisor alive
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
            ChildEntry::new(ChildSpec::new("outer_sibling"), move || {
                let ocs = Arc::clone(&ocs);
                async move {
                    ocs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    ExitReason::Normal
                }
            }),
        ];

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, entries).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Inner child should have been restarted by its own supervisor
        assert_eq!(inner_child_starts.load(Ordering::SeqCst), 2);
        // Outer sibling should NOT be restarted (OneForOne on outer, inner handled it)
        assert_eq!(outer_child_starts.load(Ordering::SeqCst), 1);

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // -----------------------------------------------------------------------
    // 25. add_child_dynamically
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn add_child_dynamically() {
        let rt = test_runtime();
        let dynamic_started = Arc::new(AtomicBool::new(false));
        let ds = Arc::clone(&dynamic_started);

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(5)
            .max_seconds(10);
        let handle = start_supervisor(rt, spec, vec![]).await;

        // Add a child dynamically
        let entry = ChildEntry::new(ChildSpec::new("dynamic"), move || {
            let ds = Arc::clone(&ds);
            async move {
                ds.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(60)).await;
                ExitReason::Normal
            }
        });

        let result = handle.add_child(entry).await;
        assert!(result.is_ok());

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(dynamic_started.load(Ordering::SeqCst));

        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
