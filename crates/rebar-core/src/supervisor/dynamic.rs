use std::collections::HashSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::process::{ExitReason, ProcessId};
use crate::runtime::Runtime;
use crate::supervisor::spec::{ChildSpec, RestartType};
use crate::supervisor::engine::ChildEntry;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for a `DynamicSupervisor`.
pub struct DynamicSupervisorSpec {
    pub max_restarts: u32,
    pub max_seconds: u32,
}

impl Default for DynamicSupervisorSpec {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicSupervisorSpec {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_restarts: 3,
            max_seconds: 5,
        }
    }

    #[must_use]
    pub const fn max_restarts(mut self, n: u32) -> Self {
        self.max_restarts = n;
        self
    }

    #[must_use]
    pub const fn max_seconds(mut self, n: u32) -> Self {
        self.max_seconds = n;
        self
    }
}

/// Information about a running dynamic child, returned by `which_children`.
#[derive(Debug, Clone)]
pub struct DynChildInfo {
    pub id: String,
    pub pid: Option<ProcessId>,
    pub restart: RestartType,
}

/// Counts returned by `count_children`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynChildCounts {
    pub active: usize,
    pub specs: usize,
}

/// A cloneable handle to a running `DynamicSupervisor`.
#[derive(Clone)]
pub struct DynamicSupervisorHandle {
    pid: ProcessId,
    msg_tx: mpsc::UnboundedSender<DynSupervisorMsg>,
}

impl DynamicSupervisorHandle {
    /// The supervisor process's own PID.
    #[must_use]
    pub const fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Start a new child under this supervisor.
    ///
    /// # Errors
    ///
    /// Returns an error string if the supervisor has shut down.
    pub async fn start_child(&self, entry: ChildEntry) -> Result<ProcessId, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.msg_tx
            .send(DynSupervisorMsg::StartChild {
                entry,
                reply: reply_tx,
            })
            .map_err(|_| "supervisor gone".to_string())?;
        reply_rx.await.map_err(|_| "supervisor gone".to_string())?
    }

    /// Terminate a running child by PID.
    ///
    /// # Errors
    ///
    /// Returns an error string if the child is not found or the supervisor has shut down.
    pub async fn terminate_child(&self, pid: ProcessId) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.msg_tx
            .send(DynSupervisorMsg::TerminateChild {
                pid,
                reply: reply_tx,
            })
            .map_err(|_| "supervisor gone".to_string())?;
        reply_rx.await.map_err(|_| "supervisor gone".to_string())?
    }

    /// Remove a terminated child's spec from the supervisor.
    ///
    /// # Errors
    ///
    /// Returns an error string if the child is still running, not found, or the supervisor has shut down.
    pub async fn remove_child(&self, pid: ProcessId) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.msg_tx
            .send(DynSupervisorMsg::RemoveChild {
                pid,
                reply: reply_tx,
            })
            .map_err(|_| "supervisor gone".to_string())?;
        reply_rx.await.map_err(|_| "supervisor gone".to_string())?
    }

    /// List all children (active and terminated-but-not-removed).
    ///
    /// # Errors
    ///
    /// Returns an error string if the supervisor has shut down.
    pub async fn which_children(&self) -> Result<Vec<DynChildInfo>, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.msg_tx
            .send(DynSupervisorMsg::WhichChildren { reply: reply_tx })
            .map_err(|_| "supervisor gone".to_string())?;
        reply_rx.await.map_err(|_| "supervisor gone".to_string())
    }

    /// Return aggregate child counts.
    ///
    /// # Errors
    ///
    /// Returns an error string if the supervisor has shut down.
    pub async fn count_children(&self) -> Result<DynChildCounts, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.msg_tx
            .send(DynSupervisorMsg::CountChildren { reply: reply_tx })
            .map_err(|_| "supervisor gone".to_string())?;
        reply_rx.await.map_err(|_| "supervisor gone".to_string())
    }

    /// Shut down the supervisor and all its children.
    pub fn shutdown(&self) {
        let _ = self.msg_tx.send(DynSupervisorMsg::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

enum DynSupervisorMsg {
    StartChild {
        entry: ChildEntry,
        reply: oneshot::Sender<Result<ProcessId, String>>,
    },
    TerminateChild {
        pid: ProcessId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveChild {
        pid: ProcessId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    WhichChildren {
        reply: oneshot::Sender<Vec<DynChildInfo>>,
    },
    CountChildren {
        reply: oneshot::Sender<DynChildCounts>,
    },
    ChildExited {
        pid: ProcessId,
        reason: ExitReason,
    },
    Shutdown,
}

/// Per-child state tracked by the supervisor.
struct DynChildState {
    id: String,
    spec: ChildSpec,
    factory: ChildFactory,
    pid: ProcessId,
    active: bool,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

/// Factory type matching the one in engine.rs but re-used here.
type ChildFactory =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ExitReason> + Send>> + Send + Sync>;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Start a dynamic supervisor as a process in the given runtime.
pub async fn start_dynamic_supervisor(
    runtime: Arc<Runtime>,
    spec: DynamicSupervisorSpec,
) -> DynamicSupervisorHandle {
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let msg_tx_clone = msg_tx.clone();

    let pid = runtime
        .spawn(move |_ctx| async move {
            dynamic_supervisor_loop(spec, msg_rx, msg_tx_clone).await;
        })
        .await;

    DynamicSupervisorHandle { pid, msg_tx }
}

// ---------------------------------------------------------------------------
// Supervisor event loop
// ---------------------------------------------------------------------------

async fn dynamic_supervisor_loop(
    spec: DynamicSupervisorSpec,
    mut msg_rx: mpsc::UnboundedReceiver<DynSupervisorMsg>,
    msg_tx: mpsc::UnboundedSender<DynSupervisorMsg>,
) {
    let mut children: HashMap<ProcessId, DynChildState> = HashMap::new();
    let mut restart_times: VecDeque<Instant> = VecDeque::new();
    let mut terminated_pids: HashSet<ProcessId> = HashSet::new();

    loop {
        let Some(msg) = msg_rx.recv().await else {
            break;
        };

        match msg {
            DynSupervisorMsg::StartChild { entry, reply } => {
                let (pid, shutdown_tx) = start_dyn_child(&entry, &msg_tx);
                let state = DynChildState {
                    id: entry.spec.id.clone(),
                    spec: entry.spec,
                    factory: entry.factory,
                    pid,
                    active: true,
                    shutdown_tx: Some(shutdown_tx),
                };
                children.insert(pid, state);
                let _ = reply.send(Ok(pid));
            }

            DynSupervisorMsg::TerminateChild { pid, reply } => {
                if let Some(child) = children.get_mut(&pid) {
                    // Mark as deliberately terminated so ChildExited won't restart
                    terminated_pids.insert(pid);
                    // Signal shutdown by dropping the sender
                    child.shutdown_tx.take();
                    child.active = false;
                    let _ = reply.send(Ok(()));
                } else {
                    let _ = reply.send(Err("child not found".to_string()));
                }
            }

            DynSupervisorMsg::RemoveChild { pid, reply } => {
                if let Some(child) = children.get(&pid) {
                    if child.active {
                        let _ = reply.send(Err("child still running".to_string()));
                    } else {
                        children.remove(&pid);
                        let _ = reply.send(Ok(()));
                    }
                } else {
                    let _ = reply.send(Err("child not found".to_string()));
                }
            }

            DynSupervisorMsg::WhichChildren { reply } => {
                let infos: Vec<DynChildInfo> = children
                    .values()
                    .map(|c| DynChildInfo {
                        id: c.id.clone(),
                        pid: if c.active { Some(c.pid) } else { None },
                        restart: c.spec.restart,
                    })
                    .collect();
                let _ = reply.send(infos);
            }

            DynSupervisorMsg::CountChildren { reply } => {
                let active = children.values().filter(|c| c.active).count();
                let specs = children.len();
                let _ = reply.send(DynChildCounts { active, specs });
            }

            DynSupervisorMsg::ChildExited { pid, reason } => {
                if handle_dyn_child_exit(
                    pid,
                    &reason,
                    &mut children,
                    &mut terminated_pids,
                    &mut restart_times,
                    &spec,
                    &msg_tx,
                ) {
                    break;
                }
            }

            DynSupervisorMsg::Shutdown => {
                // Terminate all active children
                for child in children.values_mut() {
                    if child.active {
                        child.shutdown_tx.take();
                        child.active = false;
                    }
                }
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Child spawning helpers
// ---------------------------------------------------------------------------

static DYN_CHILD_COUNTER: AtomicU64 = AtomicU64::new(2_000_000);

/// Spawn a child from a `ChildEntry`, returning `(pid, shutdown_tx)`.
fn start_dyn_child(
    entry: &ChildEntry,
    msg_tx: &mpsc::UnboundedSender<DynSupervisorMsg>,
) -> (ProcessId, oneshot::Sender<()>) {
    start_dyn_child_from_factory(&entry.factory, msg_tx)
}

/// Spawn a child task using a factory. Returns `(pid, shutdown_tx)`.
fn start_dyn_child_from_factory(
    factory: &ChildFactory,
    msg_tx: &mpsc::UnboundedSender<DynSupervisorMsg>,
) -> (ProcessId, oneshot::Sender<()>) {
    let local_id = DYN_CHILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = ProcessId::new(1, local_id);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let future = factory();
    let msg_tx = msg_tx.clone();
    let child_pid = pid;

    tokio::spawn(async move {
        let reason = tokio::select! {
            reason = future => reason,
            _ = shutdown_rx => ExitReason::Normal,
        };
        let _ = msg_tx.send(DynSupervisorMsg::ChildExited {
            pid: child_pid,
            reason,
        });
    });

    (pid, shutdown_tx)
}

/// Handle a child exit event. Returns `true` if the supervisor should stop.
fn handle_dyn_child_exit(
    pid: ProcessId,
    reason: &ExitReason,
    children: &mut HashMap<ProcessId, DynChildState>,
    terminated_pids: &mut HashSet<ProcessId>,
    restart_times: &mut VecDeque<Instant>,
    spec: &DynamicSupervisorSpec,
    msg_tx: &mpsc::UnboundedSender<DynSupervisorMsg>,
) -> bool {
    if terminated_pids.remove(&pid) {
        return false;
    }

    let Some(child) = children.get(&pid) else {
        return false;
    };

    let should_restart = child.spec.restart.should_restart(reason);
    let is_temporary = matches!(child.spec.restart, RestartType::Temporary);

    if is_temporary {
        children.remove(&pid);
        return false;
    }

    if should_restart {
        if !check_restart_limit(restart_times, spec.max_restarts, spec.max_seconds) {
            return true;
        }

        let factory = children.get(&pid).unwrap().factory.clone();
        let old_spec = children.get(&pid).unwrap().spec.clone();
        let old_id = children.get(&pid).unwrap().id.clone();
        children.remove(&pid);

        let entry = ChildEntry {
            spec: old_spec.clone(),
            factory: factory.clone(),
        };
        let (new_pid, shutdown_tx) = start_dyn_child(&entry, msg_tx);
        let new_state = DynChildState {
            id: old_id,
            spec: old_spec,
            factory,
            pid: new_pid,
            active: true,
            shutdown_tx: Some(shutdown_tx),
        };
        children.insert(new_pid, new_state);
    } else if let Some(child) = children.get_mut(&pid) {
        child.active = false;
        child.shutdown_tx.take();
    }
    false
}

/// Sliding window restart limiter: returns `true` if a restart is allowed.
fn check_restart_limit(
    restart_times: &mut VecDeque<Instant>,
    max_restarts: u32,
    max_seconds: u32,
) -> bool {
    let now = Instant::now();
    let window = Duration::from_secs(u64::from(max_seconds));

    // Prune old entries outside the window
    while let Some(&front) = restart_times.front() {
        if now.duration_since(front) > window {
            restart_times.pop_front();
        } else {
            break;
        }
    }

    if restart_times.len() >= max_restarts as usize {
        return false;
    }

    restart_times.push_back(now);
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomOrd};
    use std::sync::Arc as StdArc;
    use tokio::time::{sleep, Duration};

    fn make_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::new(1))
    }

    /// A child factory that runs forever (until shutdown).
    fn long_running_factory() -> ChildEntry {
        ChildEntry::new(ChildSpec::new("worker"), || async {
            // Run until cancelled via shutdown_rx (which our spawn wrapper handles)
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            #[allow(unreachable_code)]
            ExitReason::Normal
        })
    }

    /// A factory that increments a counter, then immediately exits abnormally.
    fn crashing_factory(
        counter: &StdArc<AtomicUsize>,
        restart: RestartType,
    ) -> ChildEntry {
        let spec = ChildSpec::new("crasher").restart(restart);
        let counter_clone = StdArc::clone(counter);
        ChildEntry {
            spec,
            factory: Arc::new(move || {
                let c = counter_clone.clone();
                Box::pin(async move {
                    c.fetch_add(1, AtomOrd::SeqCst);
                    ExitReason::Abnormal("crash".into())
                })
            }),
        }
    }

    #[tokio::test]
    async fn start_dynamic_supervisor_returns_handle() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;
        // pid should have node_id 1 (from the runtime)
        assert_eq!(handle.pid().node_id(), 1);
    }

    #[tokio::test]
    async fn start_child_returns_pid() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;
        let pid = handle.start_child(long_running_factory()).await.unwrap();
        assert_eq!(pid.node_id(), 1);
        handle.shutdown();
    }

    #[tokio::test]
    async fn count_children_after_start() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

        for _ in 0..3 {
            handle.start_child(long_running_factory()).await.unwrap();
        }

        let counts = handle.count_children().await.unwrap();
        assert_eq!(counts.active, 3);
        assert_eq!(counts.specs, 3);

        handle.shutdown();
    }

    #[tokio::test]
    async fn terminate_child_stops_it() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

        let pid = handle.start_child(long_running_factory()).await.unwrap();
        handle.terminate_child(pid).await.unwrap();

        // Give the supervisor a moment to process
        sleep(Duration::from_millis(50)).await;

        let counts = handle.count_children().await.unwrap();
        assert_eq!(counts.active, 0);

        handle.shutdown();
    }

    #[tokio::test]
    async fn remove_child_removes_spec() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

        let pid = handle.start_child(long_running_factory()).await.unwrap();
        handle.terminate_child(pid).await.unwrap();
        sleep(Duration::from_millis(50)).await;

        handle.remove_child(pid).await.unwrap();
        let counts = handle.count_children().await.unwrap();
        assert_eq!(counts.specs, 0);

        handle.shutdown();
    }

    #[tokio::test]
    async fn which_children_lists_all() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

        handle.start_child(long_running_factory()).await.unwrap();
        handle.start_child(long_running_factory()).await.unwrap();

        let children = handle.which_children().await.unwrap();
        assert_eq!(children.len(), 2);

        handle.shutdown();
    }

    #[tokio::test]
    async fn permanent_child_is_restarted() {
        let rt = make_runtime();
        let spec = DynamicSupervisorSpec::new()
            .max_restarts(10)
            .max_seconds(5);
        let handle = start_dynamic_supervisor(rt, spec).await;

        let counter = StdArc::new(AtomicUsize::new(0));
        let entry = crashing_factory(&counter, RestartType::Permanent);
        let _ = handle.start_child(entry).await.unwrap();

        // Wait for restarts to happen
        sleep(Duration::from_millis(300)).await;

        let start_count = counter.load(AtomOrd::SeqCst);
        assert!(
            start_count >= 3,
            "expected at least 3 starts, got {start_count}",
        );

        handle.shutdown();
    }

    #[tokio::test]
    async fn temporary_child_not_restarted() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

        let counter = StdArc::new(AtomicUsize::new(0));
        let entry = crashing_factory(&counter, RestartType::Temporary);
        let _ = handle.start_child(entry).await.unwrap();

        sleep(Duration::from_millis(200)).await;

        let start_count = counter.load(AtomOrd::SeqCst);
        assert_eq!(start_count, 1, "temporary child should start exactly once");

        handle.shutdown();
    }

    #[tokio::test]
    async fn shutdown_stops_all_children() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

        for _ in 0..5 {
            handle.start_child(long_running_factory()).await.unwrap();
        }

        handle.shutdown();
        sleep(Duration::from_millis(100)).await;

        // After shutdown, the supervisor loop has exited so
        // count_children should return an error.
        let result = handle.count_children().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn handle_is_clone() {
        let rt = make_runtime();
        let handle =
            start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

        let cloned = handle.clone();
        assert_eq!(handle.pid(), cloned.pid());

        handle.shutdown();
    }
}
