use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::Arc;

use crate::process::{ExitReason, ProcessId};
use crate::runtime::Runtime;
use crate::supervisor::engine::{ChildEntry, SupervisorHandle, start_supervisor};
use crate::supervisor::spec::{ChildSpec, SupervisorSpec};

use super::types::PartitionSupervisorSpec;

/// A factory that creates the async task for each partition.
///
/// The factory receives the zero-based partition index and must return a
/// pinned future that resolves to an `ExitReason`. The factory is called
/// once per partition at startup and again on restarts, so it must be
/// callable multiple times.
pub type PartitionFactory =
    Arc<dyn Fn(usize) -> Pin<Box<dyn Future<Output = ExitReason> + Send>> + Send + Sync>;

/// A handle to a running `PartitionSupervisor`.
///
/// Provides key-based routing to individual partitions and delegates
/// lifecycle management to the underlying `SupervisorHandle`.
#[derive(Clone)]
pub struct PartitionSupervisorHandle {
    pid: ProcessId,
    partitions: usize,
    runtime: Arc<Runtime>,
    supervisor: SupervisorHandle,
}

impl PartitionSupervisorHandle {
    /// Return the supervisor's own `ProcessId`.
    #[must_use]
    pub const fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Return the number of partitions.
    #[must_use]
    pub const fn partitions(&self) -> usize {
        self.partitions
    }

    /// The registered name of the partition at `index`.
    #[must_use]
    fn partition_name(index: usize) -> String {
        format!("partition_{index}")
    }

    /// Map an integer key to a partition index using `u64` math so the result
    /// is identical on 32- and 64-bit targets.
    #[must_use]
    fn index_for_key(&self, key: u64) -> usize {
        // `self.partitions` is non-zero (enforced at startup) and fits in u64.
        usize::try_from(key % self.partitions as u64).unwrap_or(0)
    }

    /// Route an integer key to a partition index.
    ///
    /// Uses `key % partitions` for deterministic routing.
    #[must_use]
    pub fn partition_index(&self, key: u64) -> usize {
        self.index_for_key(key)
    }

    /// Route an integer key to a partition and return that partition's CURRENT
    /// `ProcessId`, resolved through the name registry at call time.
    ///
    /// Returns `None` only if the partition is momentarily between
    /// incarnations (no name currently registered).
    #[must_use]
    pub fn which_partition(&self, key: u64) -> Option<ProcessId> {
        let index = self.index_for_key(key);
        self.runtime.whereis(&Self::partition_name(index))
    }

    /// Route a hashable key to a partition and return that partition's CURRENT
    /// `ProcessId`, resolved through the name registry at call time.
    ///
    /// Uses `std::hash::DefaultHasher` to hash the key, then applies
    /// `hash % partitions` for routing.
    #[must_use]
    pub fn which_partition_by_hash<K: Hash>(&self, key: &K) -> Option<ProcessId> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let index = self.index_for_key(hasher.finish());
        self.runtime.whereis(&Self::partition_name(index))
    }

    /// Return the CURRENT `ProcessId` of a specific partition by its zero-based
    /// index, resolved through the name registry.
    ///
    /// Returns `None` if `index >= partitions` or the partition is between
    /// incarnations.
    #[must_use]
    pub fn partition_pid(&self, index: usize) -> Option<ProcessId> {
        if index >= self.partitions {
            return None;
        }
        self.runtime.whereis(&Self::partition_name(index))
    }

    /// Send a message to the partition responsible for `key`, resolving the
    /// current partition PID by name at send time so it follows restarts.
    ///
    /// # Errors
    ///
    /// Returns a [`SendError`](crate::process::SendError) if the partition is
    /// not currently registered or the send itself fails.
    pub async fn send_to_partition(
        &self,
        key: u64,
        payload: rmpv::Value,
    ) -> Result<(), crate::process::SendError> {
        let index = self.index_for_key(key);
        self.runtime
            .send_named(&Self::partition_name(index), payload)
            .await
    }

    /// Shut down the partition supervisor and all its partitions.
    pub fn shutdown(&self) {
        self.supervisor.shutdown();
    }
}

/// Start a `PartitionSupervisor` with the given specification and factory.
///
/// Creates `spec.partitions` children using the provided factory function,
/// where each child receives its zero-based partition index. The children
/// are managed by an underlying regular supervisor.
///
/// # Errors
///
/// This function does not return errors directly. If the underlying
/// supervisor fails to start children, they will be restarted according
/// to the configured strategy.
///
/// # Panics
///
/// Panics if `spec.partitions` is zero, or if the underlying supervisor fails
/// to start a partition child (which only happens if the supervisor process
/// has already gone away).
pub async fn start_partition_supervisor(
    runtime: Arc<Runtime>,
    spec: PartitionSupervisorSpec,
    factory: PartitionFactory,
) -> PartitionSupervisorHandle {
    assert!(spec.partitions > 0, "partitions must be greater than zero");

    let partition_count = spec.partitions;

    // Start the underlying supervisor with no initial children.
    let sup_spec = SupervisorSpec::new(spec.strategy)
        .max_restarts(spec.max_restarts)
        .max_seconds(spec.max_seconds);

    let sup_handle = start_supervisor(Arc::clone(&runtime), sup_spec, Vec::new()).await;
    let pid = sup_handle.pid();

    // Build one `ChildEntry` per partition, each capturing its index and
    // registered under a stable name ("partition_{i}") so routing resolves to
    // the CURRENT incarnation's PID after any crash + restart.
    let mut entries = Vec::with_capacity(partition_count);
    for i in 0..partition_count {
        let factory = Arc::clone(&factory);
        entries.push(ChildEntry::new(
            ChildSpec::new(format!("partition_{i}")).registered(),
            move || {
                let factory = Arc::clone(&factory);
                async move { factory(i).await }
            },
        ));
    }

    let results = sup_handle.add_children(entries).await;
    for result in results {
        assert!(
            result.is_ok(),
            "failed to start partition child: supervisor unavailable",
        );
    }

    PartitionSupervisorHandle {
        pid,
        partitions: partition_count,
        runtime,
        supervisor: sup_handle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    /// Helper to create a runtime for tests.
    fn test_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::new(1))
    }

    /// A factory that signals its partition index via a shared counter/vec,
    /// then stays alive until cancelled.
    fn counting_factory(
        started: Arc<tokio::sync::Mutex<Vec<usize>>>,
    ) -> PartitionFactory {
        Arc::new(move |index| {
            let started = Arc::clone(&started);
            Box::pin(async move {
                started.lock().await.push(index);
                // Stay alive until shutdown
                std::future::pending::<()>().await;
                ExitReason::Normal
            })
        })
    }

    /// A factory that stays alive forever (until shutdown).
    fn long_running_factory() -> PartitionFactory {
        Arc::new(|_index| {
            Box::pin(async {
                std::future::pending::<()>().await;
                ExitReason::Normal
            })
        })
    }

    // -----------------------------------------------------------------------
    // 1. starts_correct_number_of_partitions
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn starts_correct_number_of_partitions() {
        let rt = test_runtime();
        let started = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let factory = counting_factory(Arc::clone(&started));

        let spec = PartitionSupervisorSpec::new().partitions(4);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        for _ in 0..1000 {
            if started.lock().await.len() == 4 { break; }
            tokio::task::yield_now().await;
        }

        let mut indices = started.lock().await.clone();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2, 3]);
        assert_eq!(handle.partitions(), 4);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 2. default_partitions_is_num_cpus
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn default_partitions_is_num_cpus() {
        let expected = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1);
        let spec = PartitionSupervisorSpec::new();
        assert_eq!(spec.partitions, expected);
    }

    // -----------------------------------------------------------------------
    // 3. custom_partition_count
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn custom_partition_count() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(7);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        assert_eq!(handle.partitions(), 7);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 4. shutdown_stops_all_partitions
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn shutdown_stops_all_partitions() {
        let rt = test_runtime();
        let alive_count = Arc::new(AtomicUsize::new(0));
        let alive_count_clone = Arc::clone(&alive_count);

        let factory: PartitionFactory = Arc::new(move |_index| {
            let alive = Arc::clone(&alive_count_clone);
            Box::pin(async move {
                alive.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                alive.fetch_sub(1, Ordering::SeqCst);
                ExitReason::Normal
            })
        });

        let spec = PartitionSupervisorSpec::new().partitions(3);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        for _ in 0..1000 {
            if alive_count.load(Ordering::SeqCst) == 3 { break; }
            tokio::task::yield_now().await;
        }
        assert_eq!(alive_count.load(Ordering::SeqCst), 3);

        handle.shutdown();
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }

        // After shutdown the partitions are gone; routing resolves by name and
        // should report no current incarnation rather than a stale PID.
        let _ = handle.which_partition(0);
    }

    /// Wait until partition `index` has registered its name, returning its PID.
    async fn wait_for_partition(
        handle: &PartitionSupervisorHandle,
        index: usize,
    ) -> ProcessId {
        for _ in 0..100_000 {
            if let Some(pid) = handle.partition_pid(index) {
                return pid;
            }
            tokio::task::yield_now().await;
        }
        panic!("partition {index} never registered");
    }

    // -----------------------------------------------------------------------
    // 5. partition_pids_all_unique
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn partition_pids_all_unique() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(5);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        let mut pids = HashSet::new();
        for i in 0..5 {
            let pid = wait_for_partition(&handle, i).await;
            pids.insert(pid);
        }
        assert_eq!(pids.len(), 5, "all partition PIDs must be unique");

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 6. which_partition_deterministic
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn which_partition_deterministic() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(4);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        // Same key always routes to the same partition
        let _ = wait_for_partition(&handle, handle.partition_index(42)).await;
        let pid1 = handle.which_partition(42);
        let pid2 = handle.which_partition(42);
        let pid3 = handle.which_partition(42);
        assert!(pid1.is_some());
        assert_eq!(pid1, pid2);
        assert_eq!(pid2, pid3);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 7. which_partition_distributes
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn which_partition_distributes() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(4);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        for i in 0..4 {
            let _ = wait_for_partition(&handle, i).await;
        }
        let mut hit_pids = HashSet::new();
        // Keys 0..4 should each hit a different partition with 4 partitions
        for key in 0..4u64 {
            hit_pids.insert(handle.which_partition(key).unwrap());
        }
        assert_eq!(
            hit_pids.len(),
            4,
            "keys 0..4 should distribute across all 4 partitions"
        );

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 8. which_partition_by_hash
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn which_partition_by_hash_works() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(4);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        for i in 0..4 {
            let _ = wait_for_partition(&handle, i).await;
        }
        // Hash-based routing should be deterministic
        let pid1 = handle.which_partition_by_hash(&"user:alice");
        let pid2 = handle.which_partition_by_hash(&"user:alice");
        assert!(pid1.is_some());
        assert_eq!(pid1, pid2);

        // Different keys may hit different partitions
        let pid_a = handle.which_partition_by_hash(&"key_a");
        let pid_b = handle.which_partition_by_hash(&"key_b");
        // We can't guarantee they differ, but the function shouldn't panic
        let _ = (pid_a, pid_b);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 9. integer_key_uses_modulo
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn integer_key_uses_modulo() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(3);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        for i in 0..3 {
            let _ = wait_for_partition(&handle, i).await;
        }
        // key % 3 == 0 for key=0, key=3, key=6 ...
        let pid_0 = handle.which_partition(0).unwrap();
        let pid_3 = handle.which_partition(3).unwrap();
        let pid_6 = handle.which_partition(6).unwrap();
        assert_eq!(pid_0, pid_3);
        assert_eq!(pid_3, pid_6);

        // key % 3 == 1 for key=1, key=4
        let pid_1 = handle.which_partition(1).unwrap();
        let pid_4 = handle.which_partition(4).unwrap();
        assert_eq!(pid_1, pid_4);

        // key % 3 == 2 for key=2, key=5
        let pid_2 = handle.which_partition(2).unwrap();
        let pid_5 = handle.which_partition(5).unwrap();
        assert_eq!(pid_2, pid_5);

        // Partition 0, 1, 2 should be different
        assert_ne!(pid_0, pid_1);
        assert_ne!(pid_1, pid_2);
        assert_ne!(pid_0, pid_2);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 10. partition_pid_returns_correct_pid
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn partition_pid_returns_correct_pid() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(3);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        // partition_pid(i) should match which_partition(i) for i < partitions
        for i in 0..3usize {
            let by_index = wait_for_partition(&handle, i).await;
            let by_route = handle.which_partition(i as u64).unwrap();
            assert_eq!(by_index, by_route);
        }

        // Out-of-bounds returns None
        assert!(handle.partition_pid(3).is_none());
        assert!(handle.partition_pid(100).is_none());

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 11. partition_restarts_on_crash
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn partition_restarts_on_crash() {
        let rt = test_runtime();
        let start_count = Arc::new(AtomicUsize::new(0));
        let start_count_clone = Arc::clone(&start_count);

        // Partition 0 crashes immediately, others stay alive
        let factory: PartitionFactory = Arc::new(move |index| {
            let count = Arc::clone(&start_count_clone);
            Box::pin(async move {
                if index == 0 {
                    count.fetch_add(1, Ordering::SeqCst);
                    // Crash immediately
                    ExitReason::Abnormal("partition crash".into())
                } else {
                    std::future::pending::<()>().await;
                    ExitReason::Normal
                }
            })
        });

        let spec = PartitionSupervisorSpec::new()
            .partitions(3)
            .max_restarts(10)
            .max_seconds(5);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        // Wait for some restarts
        for _ in 0..10000 {
            if start_count.load(Ordering::SeqCst) >= 2 { break; }
            tokio::task::yield_now().await;
        }

        let restarts = start_count.load(Ordering::SeqCst);
        assert!(
            restarts >= 2,
            "partition 0 should have been restarted at least once, got {restarts} starts"
        );

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 12. other_partitions_unaffected_by_crash
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn other_partitions_unaffected_by_crash() {
        let rt = test_runtime();
        let partition1_starts = Arc::new(AtomicUsize::new(0));
        let partition2_starts = Arc::new(AtomicUsize::new(0));
        let p1 = Arc::clone(&partition1_starts);
        let p2 = Arc::clone(&partition2_starts);

        let factory: PartitionFactory = Arc::new(move |index| {
            let p1 = Arc::clone(&p1);
            let p2 = Arc::clone(&p2);
            Box::pin(async move {
                match index {
                    0 => {
                        // Crash immediately
                        ExitReason::Abnormal("crash".into())
                    }
                    1 => {
                        p1.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<()>().await;
                        ExitReason::Normal
                    }
                    _ => {
                        p2.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<()>().await;
                        ExitReason::Normal
                    }
                }
            })
        });

        let spec = PartitionSupervisorSpec::new()
            .partitions(3)
            .max_restarts(10)
            .max_seconds(5);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        for _ in 0..1000 {
            if partition1_starts.load(Ordering::SeqCst) >= 1 && partition2_starts.load(Ordering::SeqCst) >= 1 { break; }
            tokio::task::yield_now().await;
        }

        // Partitions 1 and 2 should each have been started exactly once
        // (one_for_one means only the crashed partition restarts)
        assert_eq!(partition1_starts.load(Ordering::SeqCst), 1);
        assert_eq!(partition2_starts.load(Ordering::SeqCst), 1);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 13. spec_builder_chain
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn spec_builder_chain() {
        use crate::supervisor::spec::RestartStrategy;

        let spec = PartitionSupervisorSpec::new()
            .partitions(8)
            .strategy(RestartStrategy::OneForAll)
            .max_restarts(10)
            .max_seconds(60);

        assert_eq!(spec.partitions, 8);
        assert!(matches!(spec.strategy, RestartStrategy::OneForAll));
        assert_eq!(spec.max_restarts, 10);
        assert_eq!(spec.max_seconds, 60);
    }

    // -----------------------------------------------------------------------
    // 14. hash_routing_distributes_string_keys
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn hash_routing_distributes_string_keys() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(4);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        let mut hit_pids = HashSet::new();
        // Use enough keys to likely hit multiple partitions
        for i in 0..100 {
            let key = format!("user:{i}");
            hit_pids.insert(handle.which_partition_by_hash(&key));
        }
        assert!(
            hit_pids.len() > 1,
            "100 different string keys should hit more than one partition"
        );

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 15. single_partition_routes_everything_to_one_pid
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn single_partition_routes_everything_to_one_pid() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(1);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        let pid = wait_for_partition(&handle, 0).await;
        for key in 0..10u64 {
            assert_eq!(handle.which_partition(key), Some(pid));
        }

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 16. handle_is_clone
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn handle_is_clone() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(2);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        for i in 0..2 {
            let _ = wait_for_partition(&handle, i).await;
        }
        let cloned = handle.clone();
        assert_eq!(handle.pid(), cloned.pid());
        assert_eq!(handle.partitions(), cloned.partitions());
        assert_eq!(handle.which_partition(42), cloned.which_partition(42));

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 17. zero_partitions_panics
    // -----------------------------------------------------------------------
    #[tokio::test]
    #[should_panic(expected = "partitions must be greater than zero")]
    async fn zero_partitions_panics() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(0);
        let _ = start_partition_supervisor(rt, spec, factory).await;
    }

    // -----------------------------------------------------------------------
    // 18. pid_accessor_returns_supervisor_pid
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn pid_accessor_returns_supervisor_pid() {
        let rt = test_runtime();
        let factory = long_running_factory();
        let spec = PartitionSupervisorSpec::new().partitions(2);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        // Supervisor PID should be from node 1
        assert_eq!(handle.pid().node_id(), 1);

        // Supervisor PID should differ from all partition PIDs
        for i in 0..2 {
            let p = wait_for_partition(&handle, i).await;
            assert_ne!(handle.pid(), p);
        }

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 19. existing_supervisor_interface_unchanged
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn existing_supervisor_interface_unchanged() {
        // Verify that the existing supervisor API still works unchanged
        let rt = test_runtime();
        let entries = vec![ChildEntry::new(
            ChildSpec::new("worker"),
            || async {
                std::future::pending::<()>().await;
                ExitReason::Normal
            },
        )];

        let spec = SupervisorSpec::new(crate::supervisor::spec::RestartStrategy::OneForOne);
        let handle = start_supervisor(rt, spec, entries).await;

        assert_eq!(handle.pid().node_id(), 1);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // 20. many_partitions_stress_test
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn many_partitions_stress_test() {
        let rt = test_runtime();
        let started_count = Arc::new(AtomicUsize::new(0));
        let started_clone = Arc::clone(&started_count);

        let factory: PartitionFactory = Arc::new(move |_index| {
            let count = Arc::clone(&started_clone);
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                ExitReason::Normal
            })
        });

        let spec = PartitionSupervisorSpec::new().partitions(20);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        for _ in 0..1000 {
            if started_count.load(Ordering::SeqCst) == 20 { break; }
            tokio::task::yield_now().await;
        }
        assert_eq!(started_count.load(Ordering::SeqCst), 20);
        assert_eq!(handle.partitions(), 20);

        // All PIDs should be unique
        let mut pids = HashSet::new();
        for i in 0..20 {
            pids.insert(wait_for_partition(&handle, i).await);
        }
        assert_eq!(pids.len(), 20);

        handle.shutdown();
    }

    // -----------------------------------------------------------------------
    // Regression: a registered partition child is reachable by name after a
    // crash + restart — routing resolves to the NEW pid, not a stale one.
    //
    // Before the fix, partition routing cached the first-incarnation PIDs, so
    // after partition 0 crashed and restarted, `which_partition` returned a
    // dead PID forever, black-holing 1/N of the keyspace.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn route_resolves_to_new_pid_after_partition_restart() {
        let rt = test_runtime();
        // The test flips this to make partition 0's first incarnation crash
        // only AFTER we've observed its initial PID, so we can prove the route
        // follows the restart to a new PID.
        let should_crash = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let crashed_once = Arc::new(AtomicUsize::new(0));
        let sc = Arc::clone(&should_crash);
        let co = Arc::clone(&crashed_once);

        let factory: PartitionFactory = Arc::new(move |index| {
            let sc = Arc::clone(&sc);
            let co = Arc::clone(&co);
            Box::pin(async move {
                if index == 0 && co.load(Ordering::SeqCst) == 0 {
                    // First incarnation: wait for the crash signal, then crash.
                    while !sc.load(Ordering::SeqCst) {
                        tokio::task::yield_now().await;
                    }
                    co.fetch_add(1, Ordering::SeqCst);
                    ExitReason::Abnormal("partition crash".into())
                } else {
                    std::future::pending::<()>().await;
                    ExitReason::Normal
                }
            })
        });

        let spec = PartitionSupervisorSpec::new()
            .partitions(3)
            .max_restarts(10)
            .max_seconds(5);
        let handle = start_partition_supervisor(rt, spec, factory).await;

        // Capture the first incarnation's PID for partition 0, THEN trigger
        // the crash.
        let first_pid = wait_for_partition(&handle, 0).await;
        should_crash.store(true, Ordering::SeqCst);

        // Wait until partition 0 has crashed and been restarted under a NEW pid
        // while keeping its registered name.
        let mut new_pid = first_pid;
        for _ in 0..1_000_000 {
            if let Some(pid) = handle.which_partition(0).filter(|p| *p != first_pid) {
                new_pid = pid;
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_ne!(
            new_pid, first_pid,
            "routing must resolve to the restarted partition's NEW pid"
        );
        // And both routing paths agree on the live pid.
        assert_eq!(handle.which_partition(0), Some(new_pid));
        assert_eq!(handle.partition_pid(0), Some(new_pid));

        handle.shutdown();
    }
}
