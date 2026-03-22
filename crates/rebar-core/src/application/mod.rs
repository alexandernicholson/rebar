//! Application behaviour: top-level entry point for starting supervision trees.
//!
//! This module implements the OTP `Application` pattern.  An
//! [`Application`] is the "main" of a Rebar system -- it starts a
//! supervision tree and manages its lifecycle (start, prep-stop, stop).
//!
//! The [`ApplicationManager`] handles dependency ordering between
//! applications: [`ensure_all_started`](ApplicationManager::ensure_all_started)
//! performs a topological sort of dependencies and starts them in order.

pub mod env;
pub mod manager;
pub mod types;

pub use env::AppEnv;
pub use manager::{ApplicationManager, RunningApp};
pub use types::{AppError, AppSpec, Application};
