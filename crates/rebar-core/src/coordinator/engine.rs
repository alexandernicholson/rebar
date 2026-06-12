use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::process::ProcessId;
use crate::router::MessageRouter;
use crate::runtime::Runtime;

use super::{CoordinatorError, CoordinatorSpec, WorkerId, WorkerInfo};

/// Ceiling for a worker's stored average response time (microseconds).
/// Clamping keeps the load score `(in_flight + 1) * avg` well within `u64`
/// even for a degenerate worker, so scheduling math never overflows.
/// ~1 hour is far longer than any sane task and leaves enormous headroom.
const MAX_AVG_RESPONSE_US: u64 = 3_600_000_000;

/// Upper bound on concurrent in-flight submits from [`CoordinatorHandle::submit_many`].
/// Caps how many reply PIDs / tasks land in the global process table at once.
const SUBMIT_MANY_MAX_CONCURRENCY: usize = 64;

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
    ListWorkers {
        reply: oneshot::Sender<Vec<WorkerInfo>>,
    },
    WorkerCount {
        reply: oneshot::Sender<usize>,
    },
    Shutdown,
}

/// Completion notification — separate channel so the coordinator can
/// prioritize processing completions over new submits.
enum TaskComplete {
    /// The worker replied successfully; record its response time.
    Ok {
        worker_pid: ProcessId,
        elapsed_us: u64,
    },
    /// The worker timed out or died. Remove it from the pool and retry the
    /// task on the next-least-loaded worker (failover), if attempts remain.
    Failed {
        worker_pid: ProcessId,
        task: rmpv::Value,
        reply: oneshot::Sender<Result<rmpv::Value, CoordinatorError>>,
        timeout: Duration,
        remaining_attempts: usize,
    },
    /// The worker failed and no failover attempts remain: just retire it.
    /// The caller has already been replied to with the accurate error.
    Retire { worker_pid: ProcessId },
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
            avg_response_us: 0,
            completed: 0,
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

    /// Pick the worker with the lowest estimated load.
    ///
    /// Load score = `(in_flight + 1) * max(avg_response_us, 1)`. The `+1`
    /// accounts for the task we're about to send, so even idle workers are
    /// scored by their historical response time. A worker averaging 200ms
    /// scores 40x higher than one averaging 5ms at the same in-flight count.
    ///
    /// Workers with no history (`completed == 0`) score lowest (1), ensuring
    /// they're explored first.
    fn pick_worker(&mut self) -> Option<ProcessId> {
        let (idx, _) = self
            .workers
            .iter()
            .enumerate()
            .min_by_key(|(_, w)| {
                // Saturate to avoid integer overflow on a pathological
                // in_flight count or a huge stored response time.
                let load = (w.in_flight + 1).saturating_mul(w.avg_response_us.max(1));
                // Tie-break: prefer workers with fewer completed tasks.
                (load, w.completed)
            })?;
        self.workers[idx].in_flight += 1;
        Some(self.workers[idx].pid)
    }

    /// Pick the least-loaded worker that is still alive in `table`, removing
    /// any dead-but-not-cleaned workers encountered along the way.
    ///
    /// `route().is_err()` alone is not a sufficient liveness signal: an exited
    /// worker whose `cleanup_process` has not run still has an unbounded
    /// mailbox that accepts sends, so the task would black-hole. Checking the
    /// table directly retires such workers before they swallow a task.
    fn pick_live_worker(&mut self, table: &Arc<crate::process::table::ProcessTable>) -> Option<ProcessId> {
        loop {
            let pid = self.pick_worker()?;
            if table.get(&pid).is_some() {
                return Some(pid);
            }
            // Dead worker: undo the speculative in_flight bump and retire it.
            if let Some(w) = self.workers.iter_mut().find(|w| w.pid == pid) {
                w.in_flight = w.in_flight.saturating_sub(1);
            }
            self.remove_worker_by_pid(pid);
        }
    }

    /// Record task completion: decrement in-flight, update response time EMA.
    ///
    /// Uses exponential moving average with alpha = 0.3 (recent tasks weighted
    /// more heavily, but not so volatile that one outlier dominates).
    fn complete_task(&mut self, worker_pid: ProcessId, elapsed_us: u64) {
        // Never feed a bogus 0 sample (a sub-microsecond reply, or the old
        // route-failure path): clamp to at least 1us so the scheduler keeps a
        // sane, non-degenerate load estimate.
        let sample = elapsed_us.max(1);
        if let Some(w) = self.workers.iter_mut().find(|w| w.pid == worker_pid) {
            w.in_flight = w.in_flight.saturating_sub(1);
            w.completed += 1;
            if w.completed == 1 {
                // First task: seed the average
                w.avg_response_us = sample.min(MAX_AVG_RESPONSE_US);
            } else {
                // EMA: new_avg = alpha * sample + (1 - alpha) * old_avg
                // Using integer math: (3 * sample + 7 * old) / 10.
                // Compute in u128 to avoid overflow, then clamp to a ceiling
                // so a single huge sample can't poison the load score.
                let numerator =
                    3u128 * u128::from(sample) + 7u128 * u128::from(w.avg_response_us);
                let avg = u64::try_from(numerator / 10).unwrap_or(MAX_AVG_RESPONSE_US);
                w.avg_response_us = avg.min(MAX_AVG_RESPONSE_US);
            }
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
        // Bound fan-out: an unbounded spawn would insert ~N reply PIDs plus up
        // to 2N tasks into the global process table at once, swamping it. A
        // semaphore caps the number of in-flight submits while preserving
        // result order.
        let permits = SUBMIT_MANY_MAX_CONCURRENCY.min(tasks.len().max(1));
        let semaphore = Arc::new(tokio::sync::Semaphore::new(permits));
        let mut handles = Vec::with_capacity(tasks.len());
        for task in tasks {
            let coord = self.clone();
            let sem = Arc::clone(&semaphore);
            handles.push(tokio::spawn(async move {
                // If the semaphore is somehow closed, fall back to running
                // without a permit rather than dropping the task.
                let _permit = sem.acquire_owned().await;
                coord.submit(task, timeout).await
            }));
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
    let (complete_tx, mut complete_rx) = mpsc::unbounded_channel::<TaskComplete>();

    let router: Arc<dyn MessageRouter> =
        Arc::new(crate::router::LocalRouter::new(Arc::clone(runtime.table())));

    let table_for_spawn = Arc::clone(runtime.table());

    let pid = runtime
        .spawn(move |_ctx| async move {
            let runtime_table = table_for_spawn;
            let mut state = CoordState {
                workers: Vec::new(),
                next_id: 0,
                max_workers: spec.max_workers,
                router,
            };

            loop {
                // Drain ALL pending completions first — this ensures the
                // scheduler has up-to-date in_flight counts and response
                // times before dispatching the next task.
                while let Ok(tc) = complete_rx.try_recv() {
                    handle_complete(&mut state, &runtime_table, &complete_tx, tc);
                }

                tokio::select! {
                    biased;

                    // Prioritize completions over commands
                    Some(tc) = complete_rx.recv() => {
                        handle_complete(&mut state, &runtime_table, &complete_tx, tc);
                    }

                    Some(msg) = rx.recv() => {
                        match msg {
                            CoordMsg::Register { pid, metadata, reply } => {
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
                            CoordMsg::Submit { task, reply, timeout } => {
                                let attempts = state.workers.len();
                                dispatch_task(
                                    &mut state,
                                    &runtime_table,
                                    &complete_tx,
                                    task,
                                    reply,
                                    timeout,
                                    attempts,
                                );
                                // Yield after dispatch so collector tasks can run
                                // and report completions before the next Submit is
                                // processed. This is cooperative scheduling, not a
                                // sleep — it costs ~0 time.
                                tokio::task::yield_now().await;
                            }
                            CoordMsg::Shutdown => break,
                        }
                    }

                    else => break,
                }
            }
        })
        .await;

    CoordinatorHandle { pid, tx }
}

/// Process a completion notification from a collector task.
///
/// On success, records the worker's response time. On failure (timeout or
/// death), retires the worker and fails over to the next-least-loaded worker
/// while attempts remain — so a dead-but-not-cleaned worker no longer
/// black-holes a task and hangs the caller.
fn handle_complete(
    state: &mut CoordState,
    runtime_table: &Arc<crate::process::table::ProcessTable>,
    complete_tx: &mpsc::UnboundedSender<TaskComplete>,
    tc: TaskComplete,
) {
    match tc {
        TaskComplete::Ok {
            worker_pid,
            elapsed_us,
        } => {
            state.complete_task(worker_pid, elapsed_us);
        }
        TaskComplete::Failed {
            worker_pid,
            task,
            reply,
            timeout,
            remaining_attempts,
        } => {
            // The worker failed: decrement its in-flight bump and retire it.
            if let Some(w) = state.workers.iter_mut().find(|w| w.pid == worker_pid) {
                w.in_flight = w.in_flight.saturating_sub(1);
            }
            state.remove_worker_by_pid(worker_pid);
            // Fail over to the next worker, if any attempts remain.
            dispatch_task(
                state,
                runtime_table,
                complete_tx,
                task,
                reply,
                timeout,
                remaining_attempts,
            );
        }
        TaskComplete::Retire { worker_pid } => {
            if let Some(w) = state.workers.iter_mut().find(|w| w.pid == worker_pid) {
                w.in_flight = w.in_flight.saturating_sub(1);
            }
            state.remove_worker_by_pid(worker_pid);
        }
    }
}

fn dispatch_task(
    state: &mut CoordState,
    runtime_table: &Arc<crate::process::table::ProcessTable>,
    complete_tx: &mpsc::UnboundedSender<TaskComplete>,
    task: rmpv::Value,
    reply: oneshot::Sender<Result<rmpv::Value, CoordinatorError>>,
    timeout: Duration,
    attempts: usize,
) {
    if attempts == 0 {
        let _ = reply.send(Err(CoordinatorError::NoWorkers));
        return;
    }

    // Pick a worker that is actually alive, retiring any dead-but-not-cleaned
    // ones along the way.
    let Some(worker_pid) = state.pick_live_worker(runtime_table) else {
        let _ = reply.send(Err(CoordinatorError::NoWorkers));
        return;
    };

    // Create an ephemeral reply collector process.
    let reply_pid = runtime_table.allocate_pid();
    let (mb_tx, mut mb_rx) = crate::process::mailbox::Mailbox::unbounded();
    runtime_table.insert(
        reply_pid,
        crate::process::table::ProcessHandle::new(mb_tx),
    );

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

    // Try to send to the worker. A route error means the worker is gone —
    // retire it (no bogus 0 sample) and fail over immediately.
    if state
        .router
        .route(ProcessId::new(0, 0), worker_pid, task_msg)
        .is_err()
    {
        if let Some(w) = state.workers.iter_mut().find(|w| w.pid == worker_pid) {
            w.in_flight = w.in_flight.saturating_sub(1);
        }
        state.remove_worker_by_pid(worker_pid);
        runtime_table.cleanup_process(reply_pid);
        dispatch_task(state, runtime_table, complete_tx, task, reply, timeout, attempts - 1);
        return;
    }

    // Spawn collector: wait for the result, measure elapsed time, and notify
    // the coordinator. On success it forwards the reply; on timeout/death it
    // hands the task back for failover (the reply is forwarded by the final
    // failed attempt instead).
    let rt_table = Arc::clone(runtime_table);
    let ctx = complete_tx.clone();
    let remaining_attempts = attempts - 1;
    let dispatch_time = Instant::now();
    tokio::spawn(async move {
        let outcome = tokio::time::timeout(timeout, mb_rx.recv()).await;
        // Route the reply through `cleanup_process` so the ephemeral collector
        // PID fires DOWNs / unregisters like any other process death.
        rt_table.cleanup_process(reply_pid);
        match outcome {
            Ok(Some(msg)) => {
                let elapsed_us =
                    u64::try_from(dispatch_time.elapsed().as_micros()).unwrap_or(u64::MAX);
                let _ = ctx.send(TaskComplete::Ok {
                    worker_pid,
                    elapsed_us,
                });
                let _ = reply.send(Ok(msg.payload().clone()));
            }
            Ok(None) | Err(_) => {
                if remaining_attempts == 0 {
                    // No more workers to try: report the accurate failure to
                    // the caller directly, and retire the bad worker.
                    let err = if matches!(outcome, Ok(None)) {
                        CoordinatorError::WorkerDied
                    } else {
                        CoordinatorError::Timeout
                    };
                    let _ = reply.send(Err(err));
                    let _ = ctx.send(TaskComplete::Retire { worker_pid });
                } else {
                    // Hand the task back for failover onto another worker.
                    let _ = ctx.send(TaskComplete::Failed {
                        worker_pid,
                        task,
                        reply,
                        timeout,
                        remaining_attempts,
                    });
                }
            }
        }
    });
}
