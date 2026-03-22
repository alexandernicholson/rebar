//! Sys debug interface for runtime introspection of processes.
//!
//! Provides Erlang-style `:sys` debugging: `get_state`, `suspend`, `resume`,
//! and `get_status` without stopping or restarting processes.

mod types;

pub use types::{DebugOpts, ProcessRunState, ProcessStatistics, ProcessStatus, SystemEvent};
