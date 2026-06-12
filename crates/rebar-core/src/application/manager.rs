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
    /// The application implementation, held so that `prep_stop`/`stop`
    /// always run on shutdown even if the registration was replaced or
    /// removed in the meantime (see [`ApplicationManager::stop`]).
    app: Arc<dyn Application>,
}

/// State of a slot in the `running` map.
///
/// A slot transitions `Starting -> Running` on a successful start, or is
/// removed entirely on a failed start.  The `Starting` marker lets a
/// concurrent `start` for the same name observe an in-progress start and
/// reject it, closing the check-then-act race between the initial
/// "already running?" test and the final insert.
enum RunState {
    /// A start is in progress; no supervisor exists yet.
    Starting,
    /// The application is running.
    Running(RunningApp),
}

/// Registration entry: the spec together with the trait object that
/// implements [`Application`].
struct Registration {
    spec: AppSpec,
    app: Arc<dyn Application>,
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
    /// Currently running (or starting) applications, keyed by name.
    running: DashMap<String, RunState>,
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
                app: Arc::new(app),
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
        // Extract the app implementation, deps, and initial env from the
        // registration while holding the DashMap guard, then drop it
        // before any await.
        let (app, deps, initial_env) = {
            let reg = self
                .registrations
                .get(name)
                .ok_or_else(|| AppError::NotFound(name.to_string()))?;
            (
                Arc::clone(&reg.app),
                reg.spec.dependencies.clone(),
                reg.spec.env.clone(),
            )
        };

        // Atomically claim the slot BEFORE the await so that a concurrent
        // start of the same app observes the `Starting` marker and is
        // rejected, rather than racing the final insert and producing a
        // second (orphaned) live instance.  The `Entry` guard holds the
        // shard lock only for the duration of this synchronous block.
        match self.running.entry(name.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                return Err(AppError::AlreadyStarted(name.to_string()));
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(RunState::Starting);
            }
        }

        // From here on, any early return must release the reservation so
        // a failed start does not permanently block restarts.
        let result = self.start_reserved(name, &app, &deps, &initial_env).await;
        if result.is_err() {
            // Only drop the slot if it is still our `Starting` marker; a
            // successful concurrent path would have replaced it (it cannot,
            // because we hold the reservation, but be defensive).
            self.running
                .remove_if(name, |_, state| matches!(state, RunState::Starting));
        }
        result
    }

    /// Complete a start whose `Starting` reservation is already held in
    /// `running`.  On success the slot is promoted to `Running` and the
    /// name appended to `start_order`; on error the caller removes the
    /// reservation.
    async fn start_reserved(
        &self,
        name: &str,
        app: &Arc<dyn Application>,
        deps: &[String],
        initial_env: &[(String, rmpv::Value)],
    ) -> Result<(), AppError> {
        // All deps must be running (a `Starting` dep does not count as
        // running).
        for dep in deps {
            let dep_running = self
                .running
                .get(dep.as_str())
                .is_some_and(|state| matches!(state.value(), RunState::Running(_)));
            if !dep_running {
                return Err(AppError::DependencyNotStarted(dep.clone()));
            }
        }

        // Build env from spec defaults.
        let env = AppEnv::new();
        for (k, v) in initial_env {
            env.put(k, v.clone());
        }

        let handle = app
            .start(Arc::clone(&self.runtime), &env)
            .await
            .map_err(|e| AppError::StartFailed(e.to_string()))?;

        // Promote the reservation to a running instance and record the
        // start order under the same logical step.  We append to
        // `start_order` first (while still holding our reservation) and
        // then swap the slot, so the two stay consistent: nothing can
        // observe `Running` without a matching `start_order` entry.
        self.start_order.lock().await.push(name.to_string());
        self.running.insert(
            name.to_string(),
            RunState::Running(RunningApp {
                name: name.to_string(),
                supervisor: handle,
                env,
                app: Arc::clone(app),
            }),
        );

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
        let mut started: Vec<String> = Vec::new();

        for app_name in &order {
            if self.is_running(app_name) {
                continue;
            }
            if let Err(e) = self.start(app_name).await {
                // Roll back the applications THIS call started, in reverse
                // order, so a partial start does not leave the system in a
                // half-initialised state.  Best-effort: stop failures are
                // swallowed, but the original error is returned and the
                // names started here are no longer left silently running.
                for done in started.iter().rev() {
                    let _ = self.stop(done).await;
                }
                return Err(e);
            }
            started.push(app_name.clone());
        }

        Ok(started)
    }

    /// Whether `name` is fully running (a `Starting` reservation does not
    /// count).
    fn is_running(&self, name: &str) -> bool {
        self.running
            .get(name)
            .is_some_and(|state| matches!(state.value(), RunState::Running(_)))
    }

    /// Stop a running application.
    ///
    /// Calls `prep_stop`, signals the supervision tree to shut down, then
    /// calls `stop`.  The lifecycle hooks are taken from the
    /// [`RunningApp`] captured at start time, so `prep_stop`/`stop` always
    /// run even if the registration was replaced or removed since the app
    /// started.
    ///
    /// # Shutdown ordering
    ///
    /// `SupervisorHandle::shutdown` is fire-and-forget: it enqueues a
    /// shutdown message and returns before the supervision tree has
    /// actually torn down.  There is no ack mechanism on the supervisor
    /// API, so this method cannot block until the children have stopped.
    /// `prep_stop` runs before the shutdown is signalled and `stop` runs
    /// after, but the children may still be winding down when `stop`
    /// returns (eventual consistency).
    ///
    /// # Errors
    ///
    /// Returns [`AppError::NotFound`] if the application is not running.
    pub async fn stop(&self, name: &str) -> Result<(), AppError> {
        // Only remove a fully `Running` entry; a `Starting` reservation
        // belongs to an in-flight `start` and must not be torn down here.
        // Either absent or a `Starting` reservation: treat as not running.
        let Some((_, RunState::Running(running_app))) = self
            .running
            .remove_if(name, |_, state| matches!(state, RunState::Running(_)))
        else {
            return Err(AppError::NotFound(name.to_string()));
        };

        // Remove from start order.
        {
            let mut order = self.start_order.lock().await;
            order.retain(|n| n != name);
        }

        // Lifecycle: prep_stop -> signal supervisor shutdown -> stop.
        // Hooks come from the captured app, not the (possibly changed)
        // registration.
        running_app.app.prep_stop(&running_app.env).await;

        running_app.supervisor.shutdown();

        running_app.app.stop(&running_app.env).await;

        Ok(())
    }

    /// Stop all running applications in reverse start order.
    ///
    /// Each app's `prep_stop`/`stop` hooks are invoked in strict reverse
    /// of [`start_order`](Self::start_order), and `stop` for app `N`
    /// completes before `stop` for app `N-1` begins.  However, because
    /// `SupervisorHandle::shutdown` is fire-and-forget (no ack — see
    /// [`stop`](Self::stop)), app `N`'s supervision tree may still be
    /// winding down when app `N-1` starts tearing down.  The *hook*
    /// ordering is strict; the *child-process* teardown is only eventually
    /// consistent.
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
            .filter(|entry| matches!(entry.value(), RunState::Running(_)))
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Get a clone of the environment for a running application.
    #[must_use]
    pub fn env(&self, name: &str) -> Option<AppEnv> {
        self.running.get(name).and_then(|entry| match entry.value() {
            RunState::Running(app) => Some(app.env.clone()),
            RunState::Starting => None,
        })
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

    /// Application that counts how many times `start` ran, with a yield
    /// point inside `start` so concurrent starts interleave across the
    /// await boundary (exercising the check-then-act window).
    struct CountingApp {
        starts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Application for CountingApp {
        async fn start(
            &self,
            runtime: Arc<Runtime>,
            _env: &AppEnv,
        ) -> Result<SupervisorHandle, AppError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            // Force a yield so a concurrent start has a chance to observe
            // a missing-or-Starting slot and race the insert.
            tokio::task::yield_now().await;
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

    /// Regression (fix 1): two concurrent `start("app")` calls must result
    /// in exactly one running instance and no orphan.  `start` is invoked
    /// at most once; the loser gets `AlreadyStarted` (or the slot is held
    /// as `Starting` and the second call is rejected).
    #[tokio::test]
    async fn concurrent_start_yields_single_instance() {
        let rt = Arc::new(Runtime::new(2));
        let mgr = Arc::new(ApplicationManager::new(rt));

        let starts = Arc::new(AtomicUsize::new(0));
        mgr.register(
            AppSpec::new("app"),
            CountingApp {
                starts: Arc::clone(&starts),
            },
        );

        let m1 = Arc::clone(&mgr);
        let m2 = Arc::clone(&mgr);
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { m1.start("app").await }),
            tokio::spawn(async move { m2.start("app").await }),
        );
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();

        // Exactly one succeeded.
        assert_ne!(r1.is_ok(), r2.is_ok(), "exactly one start must succeed");
        let loser = if r1.is_err() { r1 } else { r2 };
        assert!(matches!(loser.unwrap_err(), AppError::AlreadyStarted(_)));

        // The app's `start` ran exactly once -> no orphaned supervisor.
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(mgr.started_applications(), vec!["app".to_string()]);
    }

    /// Regression (fix 1): a failed start releases the reservation so the
    /// app can be started again later.
    #[tokio::test]
    async fn failed_start_releases_reservation() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        mgr.register(AppSpec::new("bad"), FailApp);
        assert!(mgr.start("bad").await.is_err());
        // Slot must be free again (not stuck as `Starting`).
        assert!(mgr.started_applications().is_empty());
        // A second attempt reaches `start` again (still fails, but is not
        // short-circuited by a leftover reservation).
        let err = mgr.start("bad").await.unwrap_err();
        assert!(matches!(err, AppError::StartFailed(_)));
    }

    /// Regression (fix 4): when app `k` fails to start, `ensure_all_started`
    /// rolls back the apps it started earlier in the same call.
    #[tokio::test]
    async fn ensure_all_started_rolls_back_on_failure() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        let prep = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicUsize::new(0));

        // base (ok) <- top (fails). top depends on base.
        mgr.register(
            AppSpec::new("base"),
            LifecycleApp {
                prep_stop_count: Arc::clone(&prep),
                stop_count: Arc::clone(&stop),
            },
        );
        mgr.register(AppSpec::new("top").dependency("base"), FailApp);

        let err = mgr.ensure_all_started("top").await.unwrap_err();
        assert!(matches!(err, AppError::StartFailed(_)));

        // base was started then rolled back -> nothing left running.
        assert!(
            mgr.started_applications().is_empty(),
            "partial start must be rolled back, not left running"
        );
        // Rollback ran base's stop lifecycle hooks.
        assert_eq!(stop.load(Ordering::SeqCst), 1);
    }

    /// Regression (fix 2): `stop` runs `prep_stop`/`stop` from the captured
    /// app even if the registration is replaced after start.
    #[tokio::test]
    async fn stop_uses_captured_app_after_reregister() {
        let rt = Arc::new(Runtime::new(1));
        let mgr = ApplicationManager::new(rt);

        let prep = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicUsize::new(0));

        mgr.register(
            AppSpec::new("app"),
            LifecycleApp {
                prep_stop_count: Arc::clone(&prep),
                stop_count: Arc::clone(&stop),
            },
        );
        mgr.start("app").await.unwrap();

        // Replace the registration with one whose hooks do NOT touch our
        // counters. The running instance must still use the captured app.
        mgr.register(AppSpec::new("app"), DummyApp);

        mgr.stop("app").await.unwrap();

        assert_eq!(prep.load(Ordering::SeqCst), 1);
        assert_eq!(stop.load(Ordering::SeqCst), 1);
    }
}
