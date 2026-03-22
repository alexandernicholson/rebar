use crate::process::ProcessId;

/// System events emitted by the engine for tracing and logging.
#[derive(Debug, Clone)]
pub enum SystemEvent {
    /// An incoming message was received.
    In {
        /// The type of message (e.g. `"call"`, `"cast"`, `"info"`).
        msg_type: String,
        /// The PID that sent the message.
        from: ProcessId,
    },
    /// An outgoing message was sent.
    Out {
        /// The type of message.
        msg_type: String,
        /// The destination PID.
        to: ProcessId,
    },
    /// The server state changed.
    StateChange {
        /// A human-readable description of the change.
        description: String,
    },
    /// The server returned without replying (cast / info).
    Noreply,
}

/// The run state of a process (running or suspended).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRunState {
    /// The process is running normally.
    Running,
    /// The process is suspended and only handles sys commands.
    Suspended,
}

/// Status information returned by `sys_get_status`.
#[derive(Debug)]
pub struct ProcessStatus {
    /// The process ID.
    pub pid: ProcessId,
    /// Whether the process is running or suspended.
    pub status: ProcessRunState,
    /// The active debug options.
    pub debug: DebugOpts,
    /// A human-readable description of the current state.
    pub state_description: String,
}

/// Debug options that can be installed on a process.
#[derive(Debug, Clone)]
pub struct DebugOpts {
    /// Whether tracing is enabled.
    pub trace: bool,
    /// Maximum number of log events to retain, or `None` if logging is disabled.
    pub log: Option<usize>,
    /// Whether statistics collection is enabled.
    pub statistics: bool,
}

impl Default for DebugOpts {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugOpts {
    /// Create a new `DebugOpts` with everything disabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            trace: false,
            log: None,
            statistics: false,
        }
    }
}

/// Statistics collected by the engine when enabled.
#[derive(Debug, Clone)]
pub struct ProcessStatistics {
    /// When statistics collection was started.
    pub start_time: std::time::Instant,
    /// Number of incoming messages processed.
    pub messages_in: u64,
    /// Number of outgoing messages sent.
    pub messages_out: u64,
    /// Approximate work units performed.
    pub reductions: u64,
}
