use std::sync::Arc;

use async_trait::async_trait;

use crate::runtime::Runtime;
use crate::supervisor::engine::SupervisorHandle;

use super::env::AppEnv;

/// The Application trait: a top-level entry point that starts a supervision tree.
///
/// Implement this trait to define the lifecycle of your application.
/// The `start` method must return a [`SupervisorHandle`] that owns the
/// application's supervision tree.  Optional `prep_stop` and `stop`
/// callbacks run during orderly shutdown.
///
/// Modelled after Elixir/OTP's `Application` behaviour.
#[async_trait]
pub trait Application: Send + Sync + 'static {
    /// Start the application, returning a supervisor handle.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] if the application fails to start.
    async fn start(
        &self,
        runtime: Arc<Runtime>,
        env: &AppEnv,
    ) -> Result<SupervisorHandle, AppError>;

    /// Called before the supervision tree is terminated.
    ///
    /// The default implementation does nothing.
    #[allow(clippy::unused_async)]
    async fn prep_stop(&self, _env: &AppEnv) {}

    /// Called after the supervision tree has stopped.
    ///
    /// The default implementation does nothing.
    #[allow(clippy::unused_async)]
    async fn stop(&self, _env: &AppEnv) {}
}

/// Specification for an application, including its name, dependencies,
/// and initial environment values.
#[derive(Debug, Clone)]
pub struct AppSpec {
    /// Unique name for this application.
    pub name: String,
    /// Applications that must be started before this one.
    pub dependencies: Vec<String>,
    /// Initial environment values loaded when the app starts.
    pub env: Vec<(String, rmpv::Value)>,
}

impl AppSpec {
    /// Create a new `AppSpec` with the given name and no dependencies.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            dependencies: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Add a dependency that must be started before this application.
    #[must_use]
    pub fn dependency(mut self, dep: impl Into<String>) -> Self {
        self.dependencies.push(dep.into());
        self
    }

    /// Add an initial environment value.
    #[must_use]
    pub fn env_val(mut self, key: impl Into<String>, value: rmpv::Value) -> Self {
        self.env.push((key.into(), value));
        self
    }
}

/// Errors that can occur during application lifecycle management.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The requested application was not registered.
    #[error("application not found: {0}")]
    NotFound(String),

    /// The application is already running.
    #[error("application already started: {0}")]
    AlreadyStarted(String),

    /// A required dependency is not registered.
    #[error("dependency not registered: {0}")]
    DependencyNotRegistered(String),

    /// A required dependency has not been started.
    #[error("dependency not started: {0}")]
    DependencyNotStarted(String),

    /// A circular dependency was detected among applications.
    #[error("circular dependency detected: {0}")]
    CircularDependency(String),

    /// The application's `start` method failed.
    #[error("start failed: {0}")]
    StartFailed(String),

    /// A requested environment key was not found.
    #[error("env key not found: {0}")]
    EnvKeyNotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_spec_builder() {
        let spec = AppSpec::new("my_app");
        assert_eq!(spec.name, "my_app");
        assert!(spec.dependencies.is_empty());
        assert!(spec.env.is_empty());
    }

    #[test]
    fn app_spec_with_deps() {
        let spec = AppSpec::new("my_app")
            .dependency("dep_a")
            .dependency("dep_b");
        assert_eq!(spec.dependencies, vec!["dep_a", "dep_b"]);
    }

    #[test]
    fn app_spec_with_env() {
        let spec = AppSpec::new("my_app")
            .env_val("host", rmpv::Value::String("localhost".into()))
            .env_val("port", rmpv::Value::Integer(8080.into()));
        assert_eq!(spec.env.len(), 2);
        assert_eq!(spec.env[0].0, "host");
        assert_eq!(spec.env[1].0, "port");
    }

    #[test]
    fn app_error_display() {
        let err = AppError::NotFound("foo".into());
        assert_eq!(err.to_string(), "application not found: foo");

        let err = AppError::CircularDependency("a -> b -> a".into());
        assert_eq!(
            err.to_string(),
            "circular dependency detected: a -> b -> a"
        );
    }
}
