use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::runtime::Runtime;
use crate::supervisor::engine::SupervisorHandle;

use super::env::AppEnv;
use super::types::{AppError, AppSpec, Application};

/// A running application instance.
pub struct RunningApp {
    /// The application name.
    pub name: String,
    /// Handle to the application's top-level supervisor.
    pub supervisor: SupervisorHandle,
    /// The application's environment.
    pub env: AppEnv,
}

/// Registration entry: the spec together with the trait object that
/// implements [`Application`].
struct Registration {
    spec: AppSpec,
    app: Box<dyn Application>,
}

/// Manages the lifecycle of registered applications.
///
/// Supports starting applications with dependency ordering, stopping
/// individual applications, and orderly shutdown of all running
/// applications in reverse start order.
pub struct ApplicationManager {
    runtime: Arc<Runtime>,
    /// Registered (but not necessarily running) applications.
    registrations: DashMap<String, Registration>,
    /// Currently running applications.
    running: DashMap<String, RunningApp>,
    /// The order in which applications were started (used for reverse
    /// shutdown).
    start_order: Mutex<Vec<String>>,
}

impl ApplicationManager {
    /// Create a new `ApplicationManager` tied to the given `Runtime`.
    #[must_use]
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            registrations: DashMap::new(),
            running: DashMap::new(),
            start_order: Mutex::new(Vec::new()),
        }
    }

    /// Register an application spec and its implementation.
    ///
    /// This does **not** start the application.  Call [`start`](Self::start)
    /// or [`ensure_all_started`](Self::ensure_all_started) afterwards.
    pub fn register<A: Application>(&self, spec: AppSpec, app: A) {
        let name = spec.name.clone();
        self.registrations.insert(
            name,
            Registration {
                spec,
                app: Box::new(app),
            },
        );
    }

    /// Start a single application.
    ///
    /// All of its declared dependencies must already be running.
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] if the application is not registered, is
    /// already running, has unstarted dependencies, or if its `start`
    /// callback fails.
    pub async fn start(&self, name: &str) -> Result<(), AppError> {
        // Already running?
        if self.running.contains_key(name) {
            return Err(AppError::AlreadyStarted(name.to_string()));
        }

        // Must be registered.  We extract what we need from the
        // `DashMap` ref and drop it before any `.await` point so the
        // guard does not live across an await boundary.
        // Extract deps and initial env from the registration while
        // holding the DashMap guard, then drop it before any await.
        let (deps, initial_env) = {
            let reg = self
                .registrations
                .get(name)
                .ok_or_else(|| AppError::NotFound(name.to_string()))?;
            (reg.spec.dependencies.clone(), reg.spec.env.clone())
        };

        // All deps must be running.
        for dep in &deps {
            if !self.running.contains_key(dep.as_str()) {
                return Err(AppError::DependencyNotStarted(dep.clone()));
            }
        }

        // Build env from spec defaults.
        let env = AppEnv::new();
        for (k, v) in &initial_env {
            env.put(k, v.clone());
        }

        // Start the application (reg guard is dropped here).
        let reg = self
            .registrations
            .get(name)
            .ok_or_else(|| AppError::NotFound(name.to_string()))?;
        let handle = reg
            .app
            .start(Arc::clone(&self.runtime), &env)
            .await
            .map_err(|e| AppError::StartFailed(e.to_string()))?;
        drop(reg);

        // Record as running.
        self.running.insert(
            name.to_string(),
            RunningApp {
                name: name.to_string(),
                supervisor: handle,
                env,
            },
        );
        self.start_order.lock().await.push(name.to_string());

        Ok(())
    }

    /// Start an application and all of its transitive dependencies, in
    /// topological order.
    ///
    /// Applications that are already running are silently skipped.
    /// Returns the names of applications that were started (in start
    /// order).
    ///
    /// # Errors
    ///
    /// Returns [`AppError`] if a circular dependency is detected, a
    /// dependency is not registered, or any application fails to start.
    pub async fn ensure_all_started(&self, name: &str) -> Result<Vec<String>, AppError> {
        let order = self.topological_sort(name)?;
        let mut started = Vec::new();

        for app_name in &order {
            if self.running.contains_key(app_name.as_str()) {
                continue;
            }
            self.start(app_name).await?;
            started.push(app_name.clone());
        }

        Ok(started)
    }

    /// Stop a running application.
    ///
    /// Calls `prep_stop`, shuts down the supervisor, then calls `stop`.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::NotFound`] if the application is not running.
    pub async fn stop(&self, name: &str) -> Result<(), AppError> {
        let (_, running_app) = self
            .running
            .remove(name)
            .ok_or_else(|| AppError::NotFound(name.to_string()))?;

        // Remove from start order.
        {
            let mut order = self.start_order.lock().await;
            order.retain(|n| n != name);
        }

        // Lifecycle: prep_stop -> shutdown supervisor -> stop.
        if let Some(reg) = self.registrations.get(name) {
            reg.app.prep_stop(&running_app.env).await;
        }

        running_app.supervisor.shutdown();

        if let Some(reg) = self.registrations.get(name) {
            reg.app.stop(&running_app.env).await;
        }

        Ok(())
    }

    /// Stop all running applications in reverse start order.
    ///
    /// # Errors
    ///
    /// Returns the first [`AppError`] encountered; remaining applications
    /// are still stopped on a best-effort basis.
    pub async fn stop_all(&self) -> Result<(), AppError> {
        let order: Vec<String> = {
            let guard = self.start_order.lock().await;
            guard.iter().rev().cloned().collect()
        };

        let mut first_error: Option<AppError> = None;

        for name in &order {
            if let Err(e) = self.stop(name).await
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    /// List the names of all currently running applications.
    #[must_use]
    pub fn started_applications(&self) -> Vec<String> {
        self.running
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Get a clone of the environment for a running application.
    #[must_use]
    pub fn env(&self, name: &str) -> Option<AppEnv> {
        self.running.get(name).map(|entry| entry.env.clone())
    }

    /// Compute a topological ordering of `name` and its transitive
    /// dependencies.
    ///
    /// Uses iterative DFS with three-colour marking to detect cycles.
    fn topological_sort(&self, name: &str) -> Result<Vec<String>, AppError> {
        use std::collections::HashMap;

        // 0 = white (unvisited), 1 = grey (in-progress), 2 = black (done)
        let mut colour: HashMap<String, u8> = HashMap::new();
        let mut result: Vec<String> = Vec::new();

        // Iterative DFS avoids stack overflow on deep dependency chains.
        // Each frame records the node and the index into its dependency
        // list that we process next.
        let mut stack: Vec<(String, usize)> = vec![(name.to_string(), 0)];
        colour.insert(name.to_string(), 1);

        // Cache: clone dependency lists out of the DashMap so we never
        // hold a DashMap guard across loop iterations.
        let mut dep_cache: HashMap<String, Vec<String>> = HashMap::new();

        while let Some((node, idx)) = stack.last_mut() {
            // Lazily populate the cache for this node.
            if !dep_cache.contains_key(node.as_str()) {
                let deps = self
                    .registrations
                    .get(node.as_str())
                    .map(|r| r.spec.dependencies.clone())
                    .ok_or_else(|| AppError::DependencyNotRegistered(node.clone()))?;
                dep_cache.insert(node.clone(), deps);
            }

            let deps = &dep_cache[node.as_str()];

            if *idx < deps.len() {
                let dep = deps[*idx].clone();
                *idx += 1;

                match colour.get(&dep).copied().unwrap_or(0) {
                    1 => {
                        // Grey -> cycle.
                        return Err(AppError::CircularDependency(format!(
                            "{dep} (while resolving {name})"
                        )));
                    }
                    2 => {
                        // Already fully processed, skip.
                    }
                    _ => {
                        colour.insert(dep.clone(), 1);
                        stack.push((dep, 0));
                    }
                }
            } else {
                // All deps processed, finalise this node.
                let node = stack.pop().unwrap().0;
                colour.insert(node.clone(), 2);
                result.push(node);
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::engine::{start_supervisor, SupervisorHandle};
    use crate::supervisor::spec::{RestartStrategy, SupervisorSpec};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A minimal application that starts an empty supervisor.
    struct DummyApp;

    #[async_trait]
    impl Application for DummyApp {
        async fn start(
            &self,
            runtime: Arc<Runtime>,
            _env: &AppEnv,
        ) -> Result<SupervisorHandle, AppError> {
            let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
            Ok(start_supervisor(runtime, spec, vec![]).await)
        }
    }

    /// Application that tracks lifecycle callbacks via atomic counters.
    struct LifecycleApp {
        prep_stop_count: Arc<AtomicUsize>,
        stop_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Application for LifecycleApp {
        async fn start(
            &self,
            runtime: Arc<Runtime>,
            _env: &AppEnv,
        ) -> Result<SupervisorHandle, AppError> {
            let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
            Ok(start_supervisor(runtime, spec, vec![]).await)
        }

        async fn prep_stop(&self, _env: &AppEnv) {
            self.prep_stop_count.fetch_add(1, Ordering::SeqCst);
        }

        async fn stop(&self, _env: &AppEnv) {
            self.stop_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Application that records its start order into a shared vec.
    struct OrderApp {
        name: String,
        order: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Application for OrderApp {
        async fn start(
            &self,
            runtime: Arc<Runtime>,
            _env: &AppEnv,
        ) -> Result<SupervisorHandle, AppError> {
            self.order.lock().await.push(self.name.clone());
            let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
            Ok(start_supervisor(runtime, spec, vec![]).await)
        }
    }

    /// Application whose `start` always fails.
    struct FailApp;

    #[async_trait]
    impl Application for FailApp {
        async fn start(
            &self,
            _runtime: Arc<Runtime>,
            _env: &AppEnv,
        ) -> Result<SupervisorHandle, AppError> {
            Err(AppError::StartFailed("intentional failure".into()))
        }
    }

    /// Application that reads a value from env during start.
    struct EnvReaderApp {
        received: Arc<Mutex<Option<rmpv::Value>>>,
    }

    #[async_trait]
    impl Application for EnvReaderApp {
        async fn start(
            &self,
            runtime: Arc<Runtime>,
            env: &AppEnv,
        ) -> Result<SupervisorHandle, AppError> {
            *self.received.lock().await = env.get("config_key");
            let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
            Ok(start_supervisor(runtime, spec, vec![]).await)
        }
    }

    // ---- Tests ----

    #[tokio::test]
    async fn register_and_start() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);
        mgr.register(AppSpec::new("app1"), DummyApp);
        mgr.start("app1").await.unwrap();
        assert!(mgr.started_applications().contains(&"app1".to_string()));
    }

    #[tokio::test]
    async fn start_already_started_errors() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);
        mgr.register(AppSpec::new("app1"), DummyApp);
        mgr.start("app1").await.unwrap();
        let err = mgr.start("app1").await.unwrap_err();
        assert!(matches!(err, AppError::AlreadyStarted(_)));
    }

    #[tokio::test]
    async fn start_not_registered_errors() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);
        let err = mgr.start("nope").await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn stop_calls_callbacks() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        let prep = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicUsize::new(0));

        mgr.register(
            AppSpec::new("app1"),
            LifecycleApp {
                prep_stop_count: Arc::clone(&prep),
                stop_count: Arc::clone(&stop),
            },
        );
        mgr.start("app1").await.unwrap();
        mgr.stop("app1").await.unwrap();

        assert_eq!(prep.load(Ordering::SeqCst), 1);
        assert_eq!(stop.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stop_not_started_errors() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);
        let err = mgr.stop("nope").await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn started_applications_lists_running() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);
        mgr.register(AppSpec::new("a"), DummyApp);
        mgr.register(AppSpec::new("b"), DummyApp);

        assert!(mgr.started_applications().is_empty());

        mgr.start("a").await.unwrap();
        mgr.start("b").await.unwrap();

        let mut running = mgr.started_applications();
        running.sort();
        assert_eq!(running, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn ensure_all_started_starts_deps() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        let order = Arc::new(Mutex::new(Vec::new()));

        mgr.register(
            AppSpec::new("base"),
            OrderApp {
                name: "base".into(),
                order: Arc::clone(&order),
            },
        );
        mgr.register(
            AppSpec::new("mid").dependency("base"),
            OrderApp {
                name: "mid".into(),
                order: Arc::clone(&order),
            },
        );
        mgr.register(
            AppSpec::new("top").dependency("mid"),
            OrderApp {
                name: "top".into(),
                order: Arc::clone(&order),
            },
        );

        let started = mgr.ensure_all_started("top").await.unwrap();
        assert_eq!(started, vec!["base", "mid", "top"]);

        let recorded = order.lock().await.clone();
        assert_eq!(recorded, vec!["base", "mid", "top"]);
    }

    #[tokio::test]
    async fn ensure_all_started_skips_running() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        let order = Arc::new(Mutex::new(Vec::new()));

        mgr.register(
            AppSpec::new("base"),
            OrderApp {
                name: "base".into(),
                order: Arc::clone(&order),
            },
        );
        mgr.register(
            AppSpec::new("top").dependency("base"),
            OrderApp {
                name: "top".into(),
                order: Arc::clone(&order),
            },
        );

        // Start base first.
        mgr.start("base").await.unwrap();
        order.lock().await.clear();

        // Now ensure_all_started should skip base.
        let started = mgr.ensure_all_started("top").await.unwrap();
        assert_eq!(started, vec!["top"]);
    }

    #[tokio::test]
    async fn circular_dependency_detected() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        mgr.register(AppSpec::new("a").dependency("b"), DummyApp);
        mgr.register(AppSpec::new("b").dependency("a"), DummyApp);

        let err = mgr.ensure_all_started("a").await.unwrap_err();
        assert!(matches!(err, AppError::CircularDependency(_)));
    }

    #[tokio::test]
    async fn stop_all_reverse_order() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        let prep = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicUsize::new(0));

        for name in &["first", "second", "third"] {
            mgr.register(
                AppSpec::new(*name),
                LifecycleApp {
                    prep_stop_count: Arc::clone(&prep),
                    stop_count: Arc::clone(&stop),
                },
            );
            mgr.start(name).await.unwrap();
        }

        assert_eq!(mgr.started_applications().len(), 3);
        mgr.stop_all().await.unwrap();
        assert!(mgr.started_applications().is_empty());
        assert_eq!(prep.load(Ordering::SeqCst), 3);
        assert_eq!(stop.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn start_without_dep_errors() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        mgr.register(AppSpec::new("app").dependency("missing_dep"), DummyApp);
        mgr.register(AppSpec::new("missing_dep"), DummyApp);

        // Try starting app without its dep running.
        let err = mgr.start("app").await.unwrap_err();
        assert!(matches!(err, AppError::DependencyNotStarted(_)));
    }

    #[tokio::test]
    async fn start_receives_env() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        let received = Arc::new(Mutex::new(None));

        mgr.register(
            AppSpec::new("app").env_val("config_key", rmpv::Value::String("hello".into())),
            EnvReaderApp {
                received: Arc::clone(&received),
            },
        );

        mgr.start("app").await.unwrap();

        let val = received.lock().await.clone();
        assert_eq!(val.unwrap().as_str(), Some("hello"));
    }

    #[tokio::test]
    async fn env_accessor() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        mgr.register(
            AppSpec::new("app").env_val("k", rmpv::Value::Boolean(true)),
            DummyApp,
        );
        mgr.start("app").await.unwrap();

        let env = mgr.env("app").unwrap();
        assert_eq!(env.get("k").unwrap().as_bool(), Some(true));
        assert!(mgr.env("nonexistent").is_none());
    }

    #[tokio::test]
    async fn start_failed_app() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        mgr.register(AppSpec::new("bad"), FailApp);
        let err = mgr.start("bad").await.unwrap_err();
        assert!(matches!(err, AppError::StartFailed(_)));
        assert!(mgr.started_applications().is_empty());
    }

    #[tokio::test]
    async fn self_referencing_dependency_detected() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        mgr.register(AppSpec::new("a").dependency("a"), DummyApp);
        let err = mgr.ensure_all_started("a").await.unwrap_err();
        assert!(matches!(err, AppError::CircularDependency(_)));
    }

    #[tokio::test]
    async fn diamond_dependency_starts_once() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        let order = Arc::new(Mutex::new(Vec::new()));

        // Diamond: top -> [left, right] -> base
        mgr.register(
            AppSpec::new("base"),
            OrderApp {
                name: "base".into(),
                order: Arc::clone(&order),
            },
        );
        mgr.register(
            AppSpec::new("left").dependency("base"),
            OrderApp {
                name: "left".into(),
                order: Arc::clone(&order),
            },
        );
        mgr.register(
            AppSpec::new("right").dependency("base"),
            OrderApp {
                name: "right".into(),
                order: Arc::clone(&order),
            },
        );
        mgr.register(
            AppSpec::new("top").dependency("left").dependency("right"),
            OrderApp {
                name: "top".into(),
                order: Arc::clone(&order),
            },
        );

        let started = mgr.ensure_all_started("top").await.unwrap();

        // base should only appear once.
        let base_count = started.iter().filter(|n| *n == "base").count();
        assert_eq!(base_count, 1);

        // base must be before left, right; left and right before top.
        let pos = |n: &str| started.iter().position(|s| s == n).unwrap();
        assert!(pos("base") < pos("left"));
        assert!(pos("base") < pos("right"));
        assert!(pos("left") < pos("top"));
        assert!(pos("right") < pos("top"));
    }
}
