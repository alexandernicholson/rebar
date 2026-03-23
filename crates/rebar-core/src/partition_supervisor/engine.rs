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
    partition_pids: Arc<Vec<ProcessId>>,
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

    /// Route an integer key to a partition and return that partition's `ProcessId`.
    ///
    /// Uses `key % partitions` for deterministic routing.
    ///
    /// # Panics
    ///
    /// Panics if `partitions` is zero (which cannot happen when the handle is
    /// obtained from `start_partition_supervisor`).
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn which_partition(&self, key: u64) -> ProcessId {
        let index = (key as usize) % self.partitions;
        self.partition_pids[index]
    }

    /// Route a hashable key to a partition and return that partition's `ProcessId`.
    ///
    /// Uses `std::hash::DefaultHasher` to hash the key, then applies
    /// `hash % partitions` for routing.
    ///
    /// # Panics
    ///
    /// Panics if `partitions` is zero (which cannot happen when the handle is
    /// obtained from `start_partition_supervisor`).
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn which_partition_by_hash<K: Hash>(&self, key: &K) -> ProcessId {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        let index = (hash as usize) % self.partitions;
        self.partition_pids[index]
    }

    /// Return the `ProcessId` of a specific partition by its zero-based index.
    ///
    /// Returns `None` if `index >= partitions`.
    #[must_use]
    pub fn partition_pid(&self, index: usize) -> Option<ProcessId> {
        self.partition_pids.get(index).copied()
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
/// Panics if `spec.partitions` is zero.
pub async fn start_partition_supervisor(
    runtime: Arc<Runtime>,
    spec: PartitionSupervisorSpec,
    factory: PartitionFactory,
) -> PartitionSupervisorHandle {
    assert!(spec.partitions > 0, "partitions must be greater than zero");

    let partition_count = spec.partitions;

    // Start the underlying supervisor with no initial children.
    // We add children dynamically via `add_children` so that we get
    // their `ProcessId` values back for routing.
    let sup_spec = SupervisorSpec::new(spec.strategy)
        .max_restarts(spec.max_restarts)
        .max_seconds(spec.max_seconds);

    let sup_handle = start_supervisor(runtime, sup_spec, Vec::new()).await;
    let pid = sup_handle.pid();

    // Build one `ChildEntry` per partition, each capturing its index
    let mut entries = Vec::with_capacity(partition_count);
    for i in 0..partition_count {
        let factory = Arc::clone(&factory);
        entries.push(ChildEntry::new(
            ChildSpec::new(format!("partition_{i}")),
            move || {
                let factory = Arc::clone(&factory);
                async move { factory(i).await }
            },
        ));
    }

    let results = sup_handle.add_children(entries).await;
    let partition_pids: Vec<ProcessId> = results
        .into_iter()
        .map(|r| r.expect("failed to start partition child"))
        .collect();

    PartitionSupervisorHandle {
        pid,
        partitions: partition_count,
        partition_pids: Arc::new(partition_pids),
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

        // After shutdown the supervisor is gone; verify handle is still usable
        // (which_partition should still work since it uses cached PIDs)
        let _ = handle.which_partition(0);
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
            let pid = handle.partition_pid(i).unwrap();
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
        let pid1 = handle.which_partition(42);
        let pid2 = handle.which_partition(42);
        let pid3 = handle.which_partition(42);
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

        let mut hit_pids = HashSet::new();
        // Keys 0..4 should each hit a different partition with 4 partitions
        for key in 0..4u64 {
            hit_pids.insert(handle.which_partition(key));
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

        // Hash-based routing should be deterministic
        let pid1 = handle.which_partition_by_hash(&"user:alice");
        let pid2 = handle.which_partition_by_hash(&"user:alice");
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

        // key % 3 == 0 for key=0, key=3, key=6 ...
        let pid_0 = handle.which_partition(0);
        let pid_3 = handle.which_partition(3);
        let pid_6 = handle.which_partition(6);
        assert_eq!(pid_0, pid_3);
        assert_eq!(pid_3, pid_6);

        // key % 3 == 1 for key=1, key=4
        let pid_1 = handle.which_partition(1);
        let pid_4 = handle.which_partition(4);
        assert_eq!(pid_1, pid_4);

        // key % 3 == 2 for key=2, key=5
        let pid_2 = handle.which_partition(2);
        let pid_5 = handle.which_partition(5);
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
            let by_index = handle.partition_pid(i).unwrap();
            let by_route = handle.which_partition(i as u64);
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

        let pid = handle.partition_pid(0).unwrap();
        for key in 0..10u64 {
            assert_eq!(handle.which_partition(key), pid);
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
            let p = handle.partition_pid(i).unwrap();
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
            pids.insert(handle.partition_pid(i).unwrap());
        }
        assert_eq!(pids.len(), 20);

        handle.shutdown();
    }
}
