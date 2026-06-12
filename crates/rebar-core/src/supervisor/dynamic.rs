use std::collections::HashSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::process::{ExitReason, ProcessId};
use crate::runtime::Runtime;
use crate::supervisor::spec::{ChildSpec, RestartType};
use crate::supervisor::engine::{ChildEntry, ChildFactory};

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

/// Per-child state tracked by the supervisor, keyed by a stable id so a
/// restarted child (new PID) is still addressable.
struct DynChildState {
    id: String,
    spec: ChildSpec,
    factory: ChildFactory,
    pid: ProcessId,
    active: bool,
    /// Abort handle for the inner child task (force-kill on terminate/shutdown).
    abort: Option<tokio::task::AbortHandle>,
    /// Fires when the child task has fully terminated.
    done_rx: Option<oneshot::Receiver<()>>,
}

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
    let runtime_clone = Arc::clone(&runtime);

    let pid = runtime
        .spawn(move |_ctx| async move {
            dynamic_supervisor_loop(runtime_clone, spec, msg_rx, msg_tx_clone).await;
        })
        .await;

    DynamicSupervisorHandle { pid, msg_tx }
}

// ---------------------------------------------------------------------------
// Supervisor event loop
// ---------------------------------------------------------------------------

/// Mutable state owned by the dynamic supervisor loop.
///
/// Children are keyed by their stable id; `pid_to_id` resolves the current
/// incarnation's PID (used by the public PID-based API) to that id, and is
/// updated on every (re)start so a restarted child stays addressable.
struct DynSupervisorState {
    spec: DynamicSupervisorSpec,
    children: HashMap<String, DynChildState>,
    pid_to_id: HashMap<ProcessId, String>,
    restart_times: VecDeque<Instant>,
    /// Ids whose current incarnation was deliberately terminated, so the
    /// resulting `ChildExited` is not treated as a crash to restart.
    terminated_ids: HashSet<String>,
    /// Monotonic counter for minting unique internal keys (the user-facing
    /// spec id need not be unique across children).
    next_uid: u64,
}

async fn dynamic_supervisor_loop(
    runtime: Arc<Runtime>,
    spec: DynamicSupervisorSpec,
    mut msg_rx: mpsc::UnboundedReceiver<DynSupervisorMsg>,
    msg_tx: mpsc::UnboundedSender<DynSupervisorMsg>,
) {
    let mut state = DynSupervisorState {
        spec,
        children: HashMap::new(),
        pid_to_id: HashMap::new(),
        restart_times: VecDeque::new(),
        terminated_ids: HashSet::new(),
        next_uid: 0,
    };

    loop {
        let Some(msg) = msg_rx.recv().await else {
            break;
        };

        match msg {
            DynSupervisorMsg::StartChild { entry, reply } => {
                let pid = state.start_child(&runtime, entry, &msg_tx).await;
                let _ = reply.send(Ok(pid));
            }

            DynSupervisorMsg::TerminateChild { pid, reply } => {
                if let Some(key) = state.pid_to_id.get(&pid).cloned() {
                    // Mark as deliberately terminated so ChildExited won't restart.
                    state.terminated_ids.insert(key.clone());
                    state.stop_child_by_key(&key).await;
                    let _ = reply.send(Ok(()));
                } else {
                    let _ = reply.send(Err("child not found".to_string()));
                }
            }

            DynSupervisorMsg::RemoveChild { pid, reply } => {
                if let Some(key) = state.pid_to_id.get(&pid).cloned() {
                    if state.children.get(&key).is_some_and(|c| c.active) {
                        let _ = reply.send(Err("child still running".to_string()));
                    } else {
                        state.children.remove(&key);
                        state.pid_to_id.remove(&pid);
                        state.terminated_ids.remove(&key);
                        let _ = reply.send(Ok(()));
                    }
                } else {
                    let _ = reply.send(Err("child not found".to_string()));
                }
            }

            DynSupervisorMsg::WhichChildren { reply } => {
                let infos: Vec<DynChildInfo> = state
                    .children
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
                let active = state.children.values().filter(|c| c.active).count();
                let specs = state.children.len();
                let _ = reply.send(DynChildCounts { active, specs });
            }

            DynSupervisorMsg::ChildExited { pid, reason } => {
                if state
                    .handle_child_exit(&runtime, pid, &reason, &msg_tx)
                    .await
                {
                    break;
                }
            }

            DynSupervisorMsg::Shutdown => {
                state.shutdown_all_children().await;
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Child spawning helpers
// ---------------------------------------------------------------------------

/// Resources returned for a freshly spawned child: its routable PID, an abort
/// handle for force-killing the inner task, and a oneshot that fires when the
/// task has fully terminated.
type SpawnedChild = (ProcessId, tokio::task::AbortHandle, oneshot::Receiver<()>);

/// Spawn a child from a `ChildEntry` as a real runtime process.
///
/// The PID is routable; if the spec requests registration, the spec id is
/// (re-)registered to the new incarnation.
async fn start_dyn_child(
    runtime: &Arc<Runtime>,
    entry: &ChildEntry,
    msg_tx: &mpsc::UnboundedSender<DynSupervisorMsg>,
) -> SpawnedChild {
    let register_name = entry.spec.register.then(|| entry.spec.id.clone());
    start_dyn_child_from_factory(runtime, &entry.factory, register_name, msg_tx).await
}

/// Spawn a child process using a factory.
async fn start_dyn_child_from_factory(
    runtime: &Arc<Runtime>,
    factory: &ChildFactory,
    register_name: Option<String>,
    msg_tx: &mpsc::UnboundedSender<DynSupervisorMsg>,
) -> SpawnedChild {
    let (abort_tx, abort_rx) = oneshot::channel::<tokio::task::AbortHandle>();
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let factory = Arc::clone(factory);
    let msg_tx = msg_tx.clone();
    let table = Arc::clone(runtime.table());

    let pid = runtime
        .spawn(move |ctx| async move {
            let self_pid = ctx.self_pid();
            if let Some(name) = register_name {
                let _ = table.reregister(name, self_pid);
            }
            let future = factory(ctx);

            // Inner task so a panic becomes a JoinError instead of unwinding
            // (and skipping) the ChildExited send — a Permanent child that
            // panics IS restarted.
            let inner = tokio::spawn(future);
            let _ = abort_tx.send(inner.abort_handle());

            let reason = match inner.await {
                Ok(reason) => reason,
                Err(join_err) if join_err.is_cancelled() => ExitReason::Normal,
                Err(_) => ExitReason::Abnormal("panic".into()),
            };

            let _ = done_tx.send(());
            let _ = msg_tx.send(DynSupervisorMsg::ChildExited {
                pid: self_pid,
                reason,
            });
        })
        .await;

    // The abort handle is always sent before the inner task is awaited.
    let abort = abort_rx
        .await
        .unwrap_or_else(|_| tokio::spawn(std::future::ready(())).abort_handle());
    (pid, abort, done_rx)
}

impl DynSupervisorState {
    /// Spawn a new child, tracking it under a fresh stable key. Returns the
    /// new incarnation's PID.
    async fn start_child(
        &mut self,
        runtime: &Arc<Runtime>,
        entry: ChildEntry,
        msg_tx: &mpsc::UnboundedSender<DynSupervisorMsg>,
    ) -> ProcessId {
        let key = format!("{}#{}", entry.spec.id, self.next_uid);
        self.next_uid += 1;
        let (pid, abort, done_rx) = start_dyn_child(runtime, &entry, msg_tx).await;
        let child_state = DynChildState {
            id: entry.spec.id.clone(),
            spec: entry.spec,
            factory: entry.factory,
            pid,
            active: true,
            abort: Some(abort),
            done_rx: Some(done_rx),
        };
        self.children.insert(key.clone(), child_state);
        self.pid_to_id.insert(pid, key);
        pid
    }

    /// Abort the child stored under `key` and await its actual termination,
    /// marking it inactive. No-op if the key is unknown.
    async fn stop_child_by_key(&mut self, key: &str) {
        if let Some(child) = self.children.get_mut(key) {
            let done_rx = child.done_rx.take();
            if let Some(abort) = child.abort.take() {
                abort.abort();
            }
            child.active = false;
            if let Some(done_rx) = done_rx {
                let _ = done_rx.await;
            }
        }
    }

    /// Terminate every active child, awaiting termination so none outlive the
    /// supervisor as orphan tasks.
    async fn shutdown_all_children(&mut self) {
        let active_keys: Vec<String> = self
            .children
            .iter()
            .filter(|(_, c)| c.active)
            .map(|(k, _)| k.clone())
            .collect();
        for key in active_keys {
            self.stop_child_by_key(&key).await;
        }
    }

    /// Handle a child exit event. Returns `true` if the supervisor should stop.
    async fn handle_child_exit(
        &mut self,
        runtime: &Arc<Runtime>,
        pid: ProcessId,
        reason: &ExitReason,
        msg_tx: &mpsc::UnboundedSender<DynSupervisorMsg>,
    ) -> bool {
        // Resolve the stable key for this incarnation. The PID mapping is kept
        // until the entry is fully removed (so a PID-based `remove_child` after
        // termination still resolves); a restart replaces it with the new PID.
        let Some(key) = self.pid_to_id.get(&pid).cloned() else {
            return false;
        };

        // Deliberately terminated: keep the (now inactive) spec so the caller
        // can still `remove_child`, but never restart it.
        if self.terminated_ids.remove(&key) {
            if let Some(child) = self.children.get_mut(&key) {
                child.active = false;
                child.abort = None;
                child.done_rx = None;
            }
            return false;
        }

        let Some(child) = self.children.get(&key) else {
            return false;
        };

        let should_restart = child.spec.restart.should_restart(reason);
        let is_temporary = matches!(child.spec.restart, RestartType::Temporary);

        if is_temporary {
            // Temporary children are never restarted and never accumulate.
            self.children.remove(&key);
            self.pid_to_id.remove(&pid);
            return false;
        }

        if should_restart {
            if !check_restart_limit(
                &mut self.restart_times,
                self.spec.max_restarts,
                self.spec.max_seconds,
            ) {
                return true;
            }

            self.pid_to_id.remove(&pid);
            let Some(old_child) = self.children.remove(&key) else {
                return false;
            };
            let DynChildState {
                id: old_id,
                spec: old_spec,
                factory,
                ..
            } = old_child;

            let entry = ChildEntry {
                spec: old_spec.clone(),
                factory: factory.clone(),
            };
            let (new_pid, abort, done_rx) = start_dyn_child(runtime, &entry, msg_tx).await;
            let new_state = DynChildState {
                id: old_id,
                spec: old_spec,
                factory,
                pid: new_pid,
                active: true,
                abort: Some(abort),
                done_rx: Some(done_rx),
            };
            self.children.insert(key.clone(), new_state);
            self.pid_to_id.insert(new_pid, key);
        } else if let Some(child) = self.children.get_mut(&key) {
            // Transient child that exited normally: keep the spec (so it can be
            // removed by the caller) but mark inactive and release the handles.
            child.active = false;
            child.abort = None;
            child.done_rx = None;
        }
        false
    }
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
            std::future::pending::<ExitReason>().await
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
            factory: Arc::new(move |_ctx| {
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

    /// Regression: a Permanent child that PANICS (rather than returning an
    /// `ExitReason`) must still be restarted. Before the fix the panic unwound
    /// the spawn body and skipped the `ChildExited` send.
    #[tokio::test]
    async fn panicking_permanent_child_is_restarted() {
        let rt = make_runtime();
        let spec = DynamicSupervisorSpec::new().max_restarts(10).max_seconds(5);
        let handle = start_dynamic_supervisor(rt, spec).await;

        let counter = StdArc::new(AtomicUsize::new(0));
        let counter_clone = StdArc::clone(&counter);
        let entry = ChildEntry {
            spec: ChildSpec::new("panicker").restart(RestartType::Permanent),
            factory: Arc::new(move |_ctx| {
                let c = StdArc::clone(&counter_clone);
                Box::pin(async move {
                    c.fetch_add(1, AtomOrd::SeqCst);
                    panic!("boom");
                })
            }),
        };
        let _ = handle.start_child(entry).await.unwrap();

        for _ in 0..1_000_000 {
            if counter.load(AtomOrd::SeqCst) >= 3 {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(
            counter.load(AtomOrd::SeqCst) >= 3,
            "panicking permanent child must be restarted, got {}",
            counter.load(AtomOrd::SeqCst)
        );

        handle.shutdown();
    }

    /// Regression: a restarted child (new PID) accumulates no leaked spec
    /// entries — `count_children().specs` stays bounded across many restarts.
    #[tokio::test]
    async fn restarted_child_does_not_leak_specs() {
        let rt = make_runtime();
        let spec = DynamicSupervisorSpec::new().max_restarts(100).max_seconds(60);
        let handle = start_dynamic_supervisor(rt, spec).await;

        let counter = StdArc::new(AtomicUsize::new(0));
        let entry = crashing_factory(&counter, RestartType::Permanent);
        let _ = handle.start_child(entry).await.unwrap();

        for _ in 0..1_000_000 {
            if counter.load(AtomOrd::SeqCst) >= 5 {
                break;
            }
            tokio::task::yield_now().await;
        }

        // Despite many restarts, exactly one spec should be tracked.
        let counts = handle.count_children().await.unwrap();
        assert_eq!(counts.specs, 1, "restarts must not accumulate child specs");

        handle.shutdown();
    }
}
