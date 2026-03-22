# Application Lifecycle Management

The `application` module provides a way to define, start, and stop independent subsystems with dependency ordering. When your system has multiple components -- a database layer, a cache, an HTTP server -- and they need to start in the right order and shut down gracefully in reverse, the Application module handles the orchestration.

---

## Table of Contents

1. [Why Application](#1-why-application)
2. [How: A Web Service with Layered Dependencies](#2-how-a-web-service-with-layered-dependencies)
3. [Key Points](#3-key-points)
4. [See Also](#4-see-also)

---

## 1. Why Application

Picture a web service composed of three layers:

1. **Database** -- connection pool and migrations.
2. **Cache** -- in-memory cache that depends on the database for warm-up.
3. **HTTP** -- request handlers that depend on both the database and the cache.

Starting them in the wrong order causes failures: the cache tries to warm up before the database is ready, or HTTP handlers serve requests before the cache is populated. Stopping them in the wrong order causes data loss: the database shuts down while the HTTP layer is still writing.

The Application module solves both problems:

- **Dependency-ordered startup.** `ensure_all_started` performs a topological sort of dependencies and starts applications in the right order.
- **Reverse-order shutdown.** `stop_all` stops applications in the reverse of their start order, so downstream consumers stop before the services they depend on.
- **Per-application configuration.** Each application has an `AppEnv` for runtime-configurable key-value settings.
- **Lifecycle hooks.** `prep_stop` runs before the supervisor shuts down (for draining connections), and `stop` runs after (for final cleanup).

---

## 2. How: A Web Service with Layered Dependencies

### Defining an application

Implement the `Application` trait. The `start` method receives a `Runtime` and an `AppEnv`, and must return a `SupervisorHandle` that owns the application's process tree.

```rust
use std::sync::Arc;

use async_trait::async_trait;
use rebar_core::application::{AppEnv, AppError, AppSpec, Application, ApplicationManager};
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::engine::{start_supervisor, SupervisorHandle};
use rebar_core::supervisor::spec::{RestartStrategy, SupervisorSpec};

struct DatabaseApp;

#[async_trait]
impl Application for DatabaseApp {
    async fn start(
        &self,
        runtime: Arc<Runtime>,
        env: &AppEnv,
    ) -> Result<SupervisorHandle, AppError> {
        let host = env
            .get_or("host", rmpv::Value::String("localhost".into()));
        let port = env
            .get_or("port", rmpv::Value::Integer(5432.into()));
        println!(
            "database: connecting to {}:{}",
            host.as_str().unwrap_or("?"),
            port.as_u64().unwrap_or(0)
        );

        // Start a supervisor for database worker processes.
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        Ok(start_supervisor(runtime, spec, vec![]).await)
    }

    async fn prep_stop(&self, _env: &AppEnv) {
        println!("database: draining connections");
    }

    async fn stop(&self, _env: &AppEnv) {
        println!("database: closed");
    }
}
```

### Defining dependent applications

```rust
struct CacheApp;

#[async_trait]
impl Application for CacheApp {
    async fn start(
        &self,
        runtime: Arc<Runtime>,
        env: &AppEnv,
    ) -> Result<SupervisorHandle, AppError> {
        let ttl = env.get_or("ttl_seconds", rmpv::Value::Integer(300.into()));
        println!("cache: starting with ttl={}s", ttl.as_u64().unwrap_or(0));

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        Ok(start_supervisor(runtime, spec, vec![]).await)
    }

    async fn prep_stop(&self, _env: &AppEnv) {
        println!("cache: flushing to disk");
    }

    async fn stop(&self, _env: &AppEnv) {
        println!("cache: stopped");
    }
}

struct HttpApp;

#[async_trait]
impl Application for HttpApp {
    async fn start(
        &self,
        runtime: Arc<Runtime>,
        env: &AppEnv,
    ) -> Result<SupervisorHandle, AppError> {
        let bind = env.get_or("bind", rmpv::Value::String("0.0.0.0:8080".into()));
        println!("http: listening on {}", bind.as_str().unwrap_or("?"));

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        Ok(start_supervisor(runtime, spec, vec![]).await)
    }

    async fn prep_stop(&self, _env: &AppEnv) {
        println!("http: stopping listener, draining requests");
    }

    async fn stop(&self, _env: &AppEnv) {
        println!("http: stopped");
    }
}
```

### Registering applications with specs

`AppSpec` declares the application's name, dependencies, and initial environment values.

```rust
async fn setup() {
    let rt = Arc::new(Runtime::new(1));
    let mgr = ApplicationManager::new(Arc::clone(&rt));

    // Register the database (no dependencies).
    mgr.register(
        AppSpec::new("database")
            .env_val("host", rmpv::Value::String("db.internal".into()))
            .env_val("port", rmpv::Value::Integer(5432.into())),
        DatabaseApp,
    );

    // Register the cache (depends on database).
    mgr.register(
        AppSpec::new("cache")
            .dependency("database")
            .env_val("ttl_seconds", rmpv::Value::Integer(600.into())),
        CacheApp,
    );

    // Register HTTP (depends on both database and cache).
    mgr.register(
        AppSpec::new("http")
            .dependency("database")
            .dependency("cache")
            .env_val("bind", rmpv::Value::String("0.0.0.0:8080".into())),
        HttpApp,
    );
}
```

### Starting with dependency ordering

`ensure_all_started` resolves transitive dependencies and starts them in topological order. Applications that are already running are skipped.

```rust
async fn boot(mgr: &ApplicationManager) {
    // Starting "http" will first start "database", then "cache", then "http".
    let started = mgr.ensure_all_started("http").await.unwrap();
    println!("started: {started:?}");
    // Output: started: ["database", "cache", "http"]

    // Calling again is a no-op -- all are already running.
    let started_again = mgr.ensure_all_started("http").await.unwrap();
    assert!(started_again.is_empty());
}
```

Circular dependencies are detected and produce `AppError::CircularDependency`:

```rust
async fn circular_example(mgr: &ApplicationManager) {
    // This will fail:
    // mgr.register(AppSpec::new("a").dependency("b"), ...);
    // mgr.register(AppSpec::new("b").dependency("a"), ...);
    // mgr.ensure_all_started("a") -> Err(AppError::CircularDependency(_))
}
```

### Runtime configuration with AppEnv

Each running application has an `AppEnv` that can be read and modified at runtime.

```rust
async fn runtime_config(mgr: &ApplicationManager) {
    // Read the database host.
    if let Some(env) = mgr.env("database") {
        let host = env.get("host").unwrap();
        println!("database host: {}", host.as_str().unwrap_or("?"));

        // Update at runtime (e.g., after a failover).
        env.put("host", rmpv::Value::String("db-replica.internal".into()));
    }
}
```

`AppEnv` methods:

| Method | Behavior |
|---|---|
| `get(key)` | Returns `Option<Value>`. `None` if missing. |
| `get_or(key, default)` | Returns `Value`. Uses `default` if missing. |
| `fetch(key)` | Returns `Result<Value, AppError>`. Error if missing. |
| `put(key, value)` | Insert or overwrite a value. |
| `delete(key)` | Remove a key. |
| `all()` | Return all key-value pairs. |

### Graceful shutdown

`stop_all` stops every running application in reverse start order. Each application's `prep_stop`, supervisor shutdown, and `stop` callbacks run in sequence.

```rust
async fn shutdown(mgr: &ApplicationManager) {
    // Stops in order: http, cache, database.
    mgr.stop_all().await.unwrap();

    assert!(mgr.started_applications().is_empty());
}
```

You can also stop individual applications:

```rust
async fn stop_one(mgr: &ApplicationManager) {
    mgr.stop("cache").await.unwrap();
    // "database" and "http" are still running.
}
```

### Querying running applications

```rust
async fn introspect(mgr: &ApplicationManager) {
    let running = mgr.started_applications();
    println!("running: {running:?}");

    let db_env = mgr.env("database");
    println!("database env: {db_env:?}");
}
```

---

## 3. Key Points

- **Applications are registered, then started.** `register` stores the spec and implementation. `start` or `ensure_all_started` launches them.
- **`ensure_all_started` resolves transitive dependencies.** It performs a topological sort, starts dependencies first, and skips already-running applications.
- **Circular dependencies are detected.** The topological sort uses three-color DFS and returns `AppError::CircularDependency` on cycles.
- **`stop_all` shuts down in reverse order.** This ensures downstream consumers stop before the services they depend on.
- **Lifecycle hooks: `start`, `prep_stop`, `stop`.** `prep_stop` runs before the supervisor is shut down (drain connections). `stop` runs after (release resources).
- **`AppEnv` is a thread-safe key-value store.** Backed by `DashMap`. Values are `rmpv::Value` (MessagePack-compatible). Safe to read and write from any thread.
- **`AppSpec` is a builder.** Chain `.dependency()` and `.env_val()` calls to configure dependencies and initial environment values.
- **Each application owns a `SupervisorHandle`.** The `start` method must return one. This ties the application's lifecycle to its supervision tree.
- **`ApplicationManager` does not auto-restart failed applications.** If `start` returns an error, the application is not marked as running. Use supervisor restart strategies within each application for process-level fault tolerance.

---

## 4. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `Application`, `ApplicationManager`, `AppSpec`, `AppEnv`, `AppError`
- [Building Stateful Services with GenServer](gen-server.md) -- services that run within an application's supervision tree
- [Sharding with PartitionSupervisor](partition-supervisor.md) -- supervised partition groups that can be started as part of an application
