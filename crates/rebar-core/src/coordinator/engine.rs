use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::process::ProcessId;
use crate::router::MessageRouter;
use crate::runtime::Runtime;

use super::{CoordinatorError, CoordinatorSpec, WorkerId, WorkerInfo};

// ---------------------------------------------------------------------------
// Internal messages
// ---------------------------------------------------------------------------

enum CoordMsg {
    Register {
        pid: ProcessId,
        metadata: rmpv::Value,
        reply: oneshot::Sender<Result<WorkerId, CoordinatorError>>,
    },
    Unregister {
        id: WorkerId,
        reply: oneshot::Sender<Result<(), CoordinatorError>>,
    },
    Submit {
        task: rmpv::Value,
        reply: oneshot::Sender<Result<rmpv::Value, CoordinatorError>>,
        timeout: Duration,
    },
    TaskComplete {
        worker_pid: ProcessId,
    },
    ListWorkers {
        reply: oneshot::Sender<Vec<WorkerInfo>>,
    },
    WorkerCount {
        reply: oneshot::Sender<usize>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct CoordState {
    workers: Vec<WorkerInfo>,
    next_id: u64,
    max_workers: usize,
    router: Arc<dyn MessageRouter>,
}

impl CoordState {
    fn register(
        &mut self,
        pid: ProcessId,
        metadata: rmpv::Value,
    ) -> Result<WorkerId, CoordinatorError> {
        if self.max_workers > 0 && self.workers.len() >= self.max_workers {
            return Err(CoordinatorError::PoolFull);
        }
        self.next_id += 1;
        let id = WorkerId(self.next_id);
        self.workers.push(WorkerInfo {
            id,
            pid,
            metadata,
            in_flight: 0,
        });
        Ok(id)
    }

    fn unregister(&mut self, id: WorkerId) -> Result<(), CoordinatorError> {
        let pos = self
            .workers
            .iter()
            .position(|w| w.id == id)
            .ok_or(CoordinatorError::WorkerNotFound(id))?;
        self.workers.swap_remove(pos);
        Ok(())
    }

    /// Pick the worker with the fewest in-flight tasks (least-loaded).
    fn pick_worker(&mut self) -> Option<ProcessId> {
        let (idx, _) = self
            .workers
            .iter()
            .enumerate()
            .min_by_key(|(_, w)| w.in_flight)?;
        self.workers[idx].in_flight += 1;
        Some(self.workers[idx].pid)
    }

    fn complete_task(&mut self, worker_pid: ProcessId) {
        if let Some(w) = self.workers.iter_mut().find(|w| w.pid == worker_pid) {
            w.in_flight = w.in_flight.saturating_sub(1);
        }
    }

    fn remove_worker_by_pid(&mut self, pid: ProcessId) {
        self.workers.retain(|w| w.pid != pid);
    }
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// Handle to a running coordinator. Cloneable.
#[derive(Clone)]
pub struct CoordinatorHandle {
    pid: ProcessId,
    tx: mpsc::Sender<CoordMsg>,
}

impl CoordinatorHandle {
    /// The coordinator's process ID.
    #[must_use]
    pub const fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Register a worker process with the coordinator.
    ///
    /// The worker must be a live process that handles task messages in its mailbox.
    /// Task messages are `rmpv::Value::Map` with keys: `"task"`, `"reply_to_node"`,
    /// `"reply_to_local"`. The worker should send the result back to the reply PID.
    ///
    /// # Errors
    ///
    /// Returns `CoordinatorError::PoolFull` if `max_workers` is reached, or
    /// `CoordinatorError::Shutdown` if the coordinator has stopped.
    pub async fn register_worker(
        &self,
        pid: ProcessId,
        metadata: rmpv::Value,
    ) -> Result<WorkerId, CoordinatorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(CoordMsg::Register {
                pid,
                metadata,
                reply: reply_tx,
            })
            .await
            .map_err(|_| CoordinatorError::Shutdown)?;
        reply_rx.await.map_err(|_| CoordinatorError::Shutdown)?
    }

    /// Unregister a worker by its `WorkerId`.
    ///
    /// # Errors
    ///
    /// Returns `CoordinatorError::WorkerNotFound` or `CoordinatorError::Shutdown`.
    pub async fn unregister_worker(&self, id: WorkerId) -> Result<(), CoordinatorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(CoordMsg::Unregister {
                id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| CoordinatorError::Shutdown)?;
        reply_rx.await.map_err(|_| CoordinatorError::Shutdown)?
    }

    /// Submit a task to be executed by a worker.
    ///
    /// The coordinator picks the worker with the fewest in-flight tasks
    /// (least-loaded scheduling), sends the task, and waits for the result.
    /// If the selected worker is dead, it is removed and the next-least-loaded
    /// worker is tried.
    ///
    /// # Errors
    ///
    /// Returns `CoordinatorError::NoWorkers` if no workers are registered,
    /// `CoordinatorError::Timeout` if the worker doesn't respond in time,
    /// or `CoordinatorError::Shutdown` if the coordinator has stopped.
    pub async fn submit(
        &self,
        task: rmpv::Value,
        timeout: Duration,
    ) -> Result<rmpv::Value, CoordinatorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(CoordMsg::Submit {
                task,
                reply: reply_tx,
                timeout,
            })
            .await
            .map_err(|_| CoordinatorError::Shutdown)?;
        reply_rx.await.map_err(|_| CoordinatorError::Shutdown)?
    }

    /// Submit multiple tasks concurrently, collecting results in order.
    ///
    /// # Errors
    ///
    /// Individual task errors are returned per-element.
    pub async fn submit_many(
        &self,
        tasks: Vec<rmpv::Value>,
        timeout: Duration,
    ) -> Vec<Result<rmpv::Value, CoordinatorError>> {
        let mut handles = Vec::with_capacity(tasks.len());
        for task in tasks {
            let coord = self.clone();
            handles.push(tokio::spawn(
                async move { coord.submit(task, timeout).await },
            ));
        }
        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            results.push(h.await.unwrap_or(Err(CoordinatorError::Shutdown)));
        }
        results
    }

    /// Return the number of registered workers.
    ///
    /// # Errors
    ///
    /// Returns `CoordinatorError::Shutdown` if the coordinator has stopped.
    pub async fn worker_count(&self) -> Result<usize, CoordinatorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(CoordMsg::WorkerCount { reply: reply_tx })
            .await
            .map_err(|_| CoordinatorError::Shutdown)?;
        reply_rx.await.map_err(|_| CoordinatorError::Shutdown)
    }

    /// List all registered workers with their in-flight task counts.
    ///
    /// # Errors
    ///
    /// Returns `CoordinatorError::Shutdown` if the coordinator has stopped.
    pub async fn list_workers(&self) -> Result<Vec<WorkerInfo>, CoordinatorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(CoordMsg::ListWorkers { reply: reply_tx })
            .await
            .map_err(|_| CoordinatorError::Shutdown)?;
        reply_rx.await.map_err(|_| CoordinatorError::Shutdown)
    }

    /// Shut down the coordinator.
    pub fn shutdown(&self) {
        let _ = self.tx.try_send(CoordMsg::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Start function
// ---------------------------------------------------------------------------

/// Start a coordinator process.
///
/// The coordinator manages a pool of registered worker processes and distributes
/// submitted tasks using **least-loaded scheduling** — each task goes to the
/// worker with the fewest in-flight tasks, naturally balancing load across
/// workers with different speeds or task durations.
///
/// Workers are NOT automatically discovered — they must be explicitly registered
/// via [`CoordinatorHandle::register_worker`]. Rebar's SWIM gossip protocol can
/// help automate discovery in future.
#[allow(clippy::too_many_lines)]
pub async fn start_coordinator(
    runtime: Arc<Runtime>,
    spec: CoordinatorSpec,
) -> CoordinatorHandle {
    let (tx, mut rx) = mpsc::channel::<CoordMsg>(256);

    let router: Arc<dyn MessageRouter> =
        Arc::new(crate::router::LocalRouter::new(Arc::clone(runtime.table())));

    let table_for_spawn = Arc::clone(runtime.table());
    let coord_tx = tx.clone();

    let pid = runtime
        .spawn(move |_ctx| async move {
            let runtime_table = table_for_spawn;
            let mut state = CoordState {
                workers: Vec::new(),
                next_id: 0,
                max_workers: spec.max_workers,
                router,
            };

            while let Some(msg) = rx.recv().await {
                match msg {
                    CoordMsg::Register {
                        pid,
                        metadata,
                        reply,
                    } => {
                        let _ = reply.send(state.register(pid, metadata));
                    }
                    CoordMsg::Unregister { id, reply } => {
                        let _ = reply.send(state.unregister(id));
                    }
                    CoordMsg::WorkerCount { reply } => {
                        let _ = reply.send(state.workers.len());
                    }
                    CoordMsg::ListWorkers { reply } => {
                        let _ = reply.send(state.workers.clone());
                    }
                    CoordMsg::TaskComplete { worker_pid } => {
                        state.complete_task(worker_pid);
                    }
                    CoordMsg::Submit {
                        task,
                        reply,
                        timeout,
                    } => {
                        let mut attempts = state.workers.len();
                        let mut reply_opt = Some(reply);

                        while attempts > 0 {
                            let Some(worker_pid) = state.pick_worker() else {
                                break;
                            };

                            // Create an ephemeral reply collector
                            let (result_tx, result_rx) = oneshot::channel();
                            let reply_pid = runtime_table.allocate_pid();
                            let (mb_tx, mut mb_rx) =
                                crate::process::mailbox::Mailbox::unbounded();
                            runtime_table.insert(
                                reply_pid,
                                crate::process::table::ProcessHandle::new(mb_tx),
                            );

                            // Build the task message
                            let task_msg = rmpv::Value::Map(vec![
                                (rmpv::Value::from("task"), task.clone()),
                                (
                                    rmpv::Value::from("reply_to_node"),
                                    rmpv::Value::from(reply_pid.node_id()),
                                ),
                                (
                                    rmpv::Value::from("reply_to_local"),
                                    rmpv::Value::from(reply_pid.local_id()),
                                ),
                            ]);

                            // Try to send to the worker
                            if state
                                .router
                                .route(ProcessId::new(0, 0), worker_pid, task_msg)
                                .is_err()
                            {
                                // Worker is dead — undo in_flight bump, remove, try next
                                state.complete_task(worker_pid);
                                state.remove_worker_by_pid(worker_pid);
                                runtime_table.remove(&reply_pid);
                                attempts -= 1;
                                continue;
                            }

                            // Spawn collector: wait for result, then notify coordinator
                            let rt_table = Arc::clone(&runtime_table);
                            let complete_tx = coord_tx.clone();
                            tokio::spawn(async move {
                                let result =
                                    match tokio::time::timeout(timeout, mb_rx.recv()).await {
                                        Ok(Some(msg)) => Ok(msg.payload().clone()),
                                        Ok(None) => Err(CoordinatorError::WorkerDied),
                                        Err(_) => Err(CoordinatorError::Timeout),
                                    };
                                rt_table.remove(&reply_pid);
                                // Decrement in-flight count
                                let _ = complete_tx
                                    .send(CoordMsg::TaskComplete { worker_pid })
                                    .await;
                                let _ = result_tx.send(result);
                            });

                            // Forward the result to the caller
                            if let Some(reply) = reply_opt.take() {
                                tokio::spawn(async move {
                                    let result = result_rx
                                        .await
                                        .unwrap_or(Err(CoordinatorError::Shutdown));
                                    let _ = reply.send(result);
                                });
                            }

                            break;
                        }

                        if let Some(reply) = reply_opt {
                            let _ = reply.send(Err(CoordinatorError::NoWorkers));
                        }
                    }
                    CoordMsg::Shutdown => break,
                }
            }
        })
        .await;

    CoordinatorHandle { pid, tx }
}
