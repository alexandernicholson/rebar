use crate::process::ProcessId;

/// Unique identifier for a registered worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(pub u64);

/// Information about a registered worker.
#[derive(Debug, Clone)]
pub struct WorkerInfo {
    /// The worker's unique ID (assigned at registration).
    pub id: WorkerId,
    /// The worker's process ID.
    pub pid: ProcessId,
    /// Arbitrary metadata attached at registration.
    pub metadata: rmpv::Value,
    /// Number of tasks currently in flight to this worker.
    pub in_flight: u64,
    /// Exponential moving average of response time in microseconds.
    /// Used by the scheduler to estimate how long this worker takes per task.
    pub avg_response_us: u64,
    /// Total tasks completed by this worker (for stats).
    pub completed: u64,
}

/// Configuration for a coordinator.
#[derive(Default)]
pub struct CoordinatorSpec {
    /// Maximum number of workers. 0 means unlimited.
    pub max_workers: usize,
}

impl CoordinatorSpec {
    /// Set the maximum number of workers.
    #[must_use]
    pub const fn max_workers(mut self, n: usize) -> Self {
        self.max_workers = n;
        self
    }
}

/// Errors from coordinator operations.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    /// No workers are registered to handle the task.
    #[error("no workers registered")]
    NoWorkers,
    /// The specified worker was not found.
    #[error("worker not found: {0:?}")]
    WorkerNotFound(WorkerId),
    /// The coordinator has shut down.
    #[error("coordinator is shut down")]
    Shutdown,
    /// The task timed out waiting for a result.
    #[error("task timed out")]
    Timeout,
    /// The worker pool is at capacity.
    #[error("worker pool full")]
    PoolFull,
    /// The worker process died while handling the task.
    #[error("worker died during task")]
    WorkerDied,
}
