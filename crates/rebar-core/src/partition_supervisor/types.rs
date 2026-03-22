use crate::process::ProcessId;
use crate::supervisor::spec::RestartStrategy;

/// Configuration for starting a `PartitionSupervisor`.
///
/// Each partition runs an identical child produced by a factory function.
/// Work is routed to a specific partition by key, distributing load across
/// independent processes.
pub struct PartitionSupervisorSpec {
    /// Number of partitions. Defaults to the number of available CPU cores.
    pub partitions: usize,
    /// Restart strategy applied to individual partitions.
    pub strategy: RestartStrategy,
    /// Maximum number of restarts allowed within `max_seconds`.
    pub max_restarts: u32,
    /// Time window (in seconds) for the restart limiter.
    pub max_seconds: u32,
}

impl Default for PartitionSupervisorSpec {
    fn default() -> Self {
        Self::new()
    }
}

impl PartitionSupervisorSpec {
    /// Create a new spec with sensible defaults.
    ///
    /// Defaults:
    /// - `partitions`: number of available CPU cores (falls back to 1)
    /// - `strategy`: `OneForOne`
    /// - `max_restarts`: 3
    /// - `max_seconds`: 5
    #[must_use]
    pub fn new() -> Self {
        let partitions = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1);
        Self {
            partitions,
            strategy: RestartStrategy::OneForOne,
            max_restarts: 3,
            max_seconds: 5,
        }
    }

    /// Set the number of partitions.
    #[must_use]
    pub const fn partitions(mut self, n: usize) -> Self {
        self.partitions = n;
        self
    }

    /// Set the restart strategy.
    #[must_use]
    pub const fn strategy(mut self, s: RestartStrategy) -> Self {
        self.strategy = s;
        self
    }

    /// Set the maximum number of restarts within the time window.
    #[must_use]
    pub const fn max_restarts(mut self, n: u32) -> Self {
        self.max_restarts = n;
        self
    }

    /// Set the time window (in seconds) for the restart limiter.
    #[must_use]
    pub const fn max_seconds(mut self, n: u32) -> Self {
        self.max_seconds = n;
        self
    }
}

/// Information about a single partition child.
#[derive(Debug, Clone)]
pub struct PartitionChild {
    /// The zero-based partition index.
    pub partition: usize,
    /// The PID of this partition, or `None` if it is restarting/stopped.
    pub pid: Option<ProcessId>,
    /// Current status of this partition.
    pub status: PartitionStatus,
}

/// Status of a partition child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStatus {
    /// The partition is running normally.
    Running,
    /// The partition is being restarted.
    Restarting,
    /// The partition has been stopped.
    Stopped,
}

/// Aggregate child counts for a `PartitionSupervisor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionCounts {
    /// Total number of partition specs (always equals the configured partition count).
    pub specs: usize,
    /// Number of partitions that are currently active/running.
    pub active: usize,
}
