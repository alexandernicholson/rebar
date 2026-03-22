# Plan: Application

## Summary

Implement Elixir's `Application` behavior — a top-level entry point that starts a supervision tree. Defines app lifecycle (start/stop), dependency ordering between applications, and application environment (config). This is the "main()" of OTP — the entry point that wires everything together.

## Rebar Integration Points

- **Supervisor**: Application's `start()` returns a SupervisorHandle
- **Runtime**: Application manages a Runtime instance
- **RuntimeBuilder**: Application wraps RuntimeBuilder for convenient startup

## Proposed API

```rust
/// The Application trait. Implement this as the top-level entry point.
#[async_trait]
pub trait Application: Send + Sync + 'static {
    /// Start the application. Must return a supervisor handle.
    /// Equivalent to Application.start/2.
    async fn start(
        &self,
        runtime: Arc<Runtime>,
        env: &AppEnv,
    ) -> Result<SupervisorHandle, AppError>;

    /// Called before the supervision tree is terminated.
    /// Equivalent to Application.prep_stop/1.
    async fn prep_stop(&self, _env: &AppEnv) {}

    /// Called after the supervision tree has stopped.
    /// Equivalent to Application.stop/1.
    async fn stop(&self, _env: &AppEnv) {}
}

/// Application environment (configuration).
#[derive(Debug, Clone)]
pub struct AppEnv {
    values: Arc<DashMap<String, rmpv::Value>>,
}

impl AppEnv {
    pub fn new() -> Self;

    /// Get a configuration value.
    /// Equivalent to Application.get_env/3.
    pub fn get(&self, key: &str) -> Option<rmpv::Value>;

    /// Get a configuration value or return default.
    pub fn get_or(&self, key: &str, default: rmpv::Value) -> rmpv::Value;

    /// Get a configuration value, panic if missing.
    /// Equivalent to Application.fetch_env!/2.
    pub fn fetch(&self, key: &str) -> Result<rmpv::Value, AppError>;

    /// Set a configuration value at runtime.
    /// Equivalent to Application.put_env/4.
    pub fn put(&self, key: &str, value: rmpv::Value);

    /// Delete a configuration value.
    /// Equivalent to Application.delete_env/3.
    pub fn delete(&self, key: &str);

    /// Get all key-value pairs.
    pub fn all(&self) -> Vec<(String, rmpv::Value)>;
}

/// Specification for an application.
pub struct AppSpec {
    /// Unique name for this application.
    pub name: String,
    /// Applications that must be started before this one.
    pub dependencies: Vec<String>,
    /// Initial environment values.
    pub env: Vec<(String, rmpv::Value)>,
}

impl AppSpec {
    pub fn new(name: impl Into<String>) -> Self;
    pub fn dependency(mut self, dep: impl Into<String>) -> Self;
    pub fn env(mut self, key: impl Into<String>, value: rmpv::Value) -> Self;
}

/// A running application instance.
pub struct RunningApp {
    pub name: String,
    pub supervisor: SupervisorHandle,
    pub env: AppEnv,
}

/// The application manager. Starts, stops, and manages applications.
pub struct ApplicationManager {
    runtime: Arc<Runtime>,
    apps: DashMap<String, RunningApp>,
    specs: DashMap<String, AppSpec>,
}

impl ApplicationManager {
    pub fn new(runtime: Arc<Runtime>) -> Self;

    /// Register an application spec.
    pub fn register<A: Application>(
        &self,
        spec: AppSpec,
        app: A,
    );

    /// Start an application and its dependencies.
    /// Equivalent to Application.ensure_all_started/1.
    pub async fn ensure_all_started(
        &self,
        name: &str,
    ) -> Result<Vec<String>, AppError>;

    /// Start a single application (deps must already be running).
    /// Equivalent to Application.start/2.
    pub async fn start(&self, name: &str) -> Result<(), AppError>;

    /// Stop a running application.
    /// Equivalent to Application.stop/1.
    pub async fn stop(&self, name: &str) -> Result<(), AppError>;

    /// Stop all applications in reverse dependency order.
    pub async fn stop_all(&self) -> Result<(), AppError>;

    /// List all started applications.
    /// Equivalent to Application.started_applications/0.
    pub fn started_applications(&self) -> Vec<String>;

    /// Get the environment for an application.
    pub fn env(&self, name: &str) -> Option<AppEnv>;
}

/// Application errors.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("application not found: {0}")]
    NotFound(String),
    #[error("application already started: {0}")]
    AlreadyStarted(String),
    #[error("dependency not started: {0}")]
    DependencyNotStarted(String),
    #[error("circular dependency detected: {0}")]
    CircularDependency(String),
    #[error("start failed: {0}")]
    StartFailed(String),
    #[error("env key not found: {0}")]
    EnvKeyNotFound(String),
}

/// Convenience: start a single application as the entry point.
/// Wraps RuntimeBuilder + ApplicationManager.
pub fn run_application<A: Application>(
    node_id: u64,
    spec: AppSpec,
    app: A,
) -> Result<(), AppError>;
```

## File Structure

```
crates/rebar-core/src/application/
├── mod.rs          # pub mod + re-exports
├── types.rs        # Application trait, AppSpec, AppError
├── env.rs          # AppEnv implementation
├── manager.rs      # ApplicationManager, dependency resolution
└── runner.rs       # run_application() convenience
```

## TDD Test Plan

### Unit Tests (AppEnv)
1. `env_get_set` — put then get returns value
2. `env_get_missing` — returns None
3. `env_get_or_default` — returns default when missing
4. `env_fetch_missing_errors` — returns AppError
5. `env_delete` — key removed
6. `env_all` — lists all pairs
7. `env_concurrent_access` — multiple threads read/write

### Unit Tests (AppSpec)
8. `app_spec_builder` — fluent builder works
9. `app_spec_with_deps` — dependencies recorded
10. `app_spec_with_env` — initial env values set

### Unit Tests (ApplicationManager)
11. `register_and_start` — basic app starts
12. `start_creates_supervisor` — supervisor handle returned
13. `stop_calls_callbacks` — prep_stop and stop called
14. `start_already_started_errors` — duplicate start error
15. `stop_not_started_errors` — stop unknown app error
16. `started_applications_lists_running` — list is accurate

### Unit Tests (dependency ordering)
17. `ensure_all_started_starts_deps` — dependencies started first
18. `ensure_all_started_skips_running` — already-started deps not restarted
19. `circular_dependency_detected` — error returned
20. `stop_all_reverse_order` — stopped in reverse start order
21. `start_without_dep_errors` — missing dep fails

### Unit Tests (Application trait)
22. `start_receives_env` — env available in start()
23. `prep_stop_called_before_shutdown` — ordering correct
24. `stop_called_after_supervisor_down` — cleanup after tree stops

### Integration Tests
25. `app_does_not_break_supervisor` — existing supervisor tests pass
26. `app_does_not_break_runtime` — existing runtime tests pass
27. `multi_app_system` — two apps with dependency
28. `run_application_convenience` — single entry point works

## Existing Interface Guarantees

- `Supervisor` / `SupervisorHandle` — UNCHANGED (Application uses them)
- `Runtime` / `RuntimeBuilder` — UNCHANGED
- `GenServer` — UNCHANGED
- New `pub mod application;` added to `lib.rs` — purely additive

---

## Elixir Reference Documentation

### Application Module (Elixir)

#### Overview

The Application module provides tools for working with Erlang/OTP applications — "the idiomatic way to package software in Erlang/OTP." Applications have standardized directory structure, configuration, and lifecycle management.

#### Application Lifecycle

Applications progress through three states:
- **Loading**: Runtime finds and processes resource files
- **Starting**: Application initializes and may run callback code
- **Stopping**: Application shuts down cleanly

#### Application Environment

Each application maintains its own keyword list environment.

**Setting in mix.exs:**
```elixir
def application do
  [env: [db_host: "localhost"]]
end
```

**Reading at runtime:**
```elixir
Application.fetch_env!(:my_app, :db_host)
```

#### Required Callbacks

**`start/2`**: Called when the application starts. Must spawn and link a supervisor, returning its PID. Arguments come from the `:mod` tuple.

**`stop/1`**: Called after supervision tree terminates. Performs cleanup. Default returns `:ok`.

#### Optional Callbacks

**`prep_stop/1`**: Invoked before supervision tree termination.

**`config_change/3`**: Called after code upgrades if app environment changed.

#### Key Functions

- `get_env(app, key, default)` — read config
- `fetch_env!(app, key)` — read config, raise on missing
- `put_env(app, key, value)` — set config at runtime
- `ensure_all_started(app)` — start app and all dependencies
- `start(app, type)` — start single app
- `stop(app)` — stop running app
- `started_applications()` — list running apps
- `loaded_applications()` — list loaded apps

#### Restart Types

- `:permanent` — failure terminates all apps and node
- `:transient` — normal exit reported, abnormal exit terminates node
- `:temporary` — failure reported only (default)

#### Important Guidelines

1. Avoid the application environment in libraries
2. Never directly access other applications' environments
3. Applications should return supervisor PIDs from `start/2`
4. Shutting down via `System.stop/1` reverses startup order
