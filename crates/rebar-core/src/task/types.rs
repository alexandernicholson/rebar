use std::fmt;

use crate::process::ProcessId;

/// Errors from task operations.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// The task did not complete within the specified timeout.
    #[error("task timed out")]
    Timeout,
    /// The task process died before producing a result.
    #[error("task process died")]
    ProcessDead,
}

/// A handle to a spawned async task.
///
/// Equivalent to Elixir's `%Task{}` struct. Provides `await_result()`,
/// `yield_result()`, and `shutdown()` for interacting with the task.
pub struct Task<T> {
    pub(crate) pid: ProcessId,
    pub(crate) result_rx: Option<tokio::sync::oneshot::Receiver<T>>,
    pub(crate) join_handle: Option<tokio::task::JoinHandle<()>>,
}

impl<T> Task<T> {
    /// Get the task's process ID.
    #[must_use]
    pub const fn pid(&self) -> ProcessId {
        self.pid
    }
}

impl<T> fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("pid", &self.pid)
            .field("has_result_rx", &self.result_rx.is_some())
            .field("has_join_handle", &self.join_handle.is_some())
            .finish()
    }
}

/// Options for [`async_map`](crate::task::async_map).
pub struct StreamOpts {
    /// Maximum number of concurrent tasks. Defaults to available parallelism.
    pub max_concurrency: usize,
    /// Whether to preserve input order. Default: true.
    pub ordered: bool,
    /// Per-task timeout. Default: 5 seconds.
    pub timeout: std::time::Duration,
}

impl Default for StreamOpts {
    fn default() -> Self {
        Self {
            max_concurrency: std::thread::available_parallelism()
                .map(std::num::NonZero::get)
                .unwrap_or(4),
            ordered: true,
            timeout: std::time::Duration::from_secs(5),
        }
    }
}
