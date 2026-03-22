# Plan: PartitionSupervisor

## Summary

Implement Elixir's `PartitionSupervisor` (v1.14+) — starts N identical children partitioned by key hash. Routes work to a specific partition to avoid bottlenecking a single process. Ideal for connection pools, rate limiters, caches, and any workload that can be sharded.

## Rebar Integration Points

- **Supervisor**: PartitionSupervisor IS a supervisor internally
- **GenServer**: Partitioned children are typically GenServers
- **Runtime**: Uses Runtime for process spawning
- **ProcessId**: Each partition has its own PID

## Proposed API

```rust
/// Options for starting a PartitionSupervisor.
pub struct PartitionSupervisorSpec {
    /// Number of partitions. Defaults to num_cpus::get().
    pub partitions: usize,
    /// Restart strategy for individual partitions.
    pub strategy: RestartStrategy, // defaults to OneForOne
    /// Max restarts within time window.
    pub max_restarts: u32,
    pub max_seconds: u32,
}

impl PartitionSupervisorSpec {
    pub fn new() -> Self;
    pub fn partitions(mut self, n: usize) -> Self;
    pub fn strategy(mut self, s: RestartStrategy) -> Self;
    pub fn max_restarts(mut self, n: u32) -> Self;
    pub fn max_seconds(mut self, n: u32) -> Self;
}

/// Factory that creates a child for each partition.
/// Receives the partition index (0..N).
pub type PartitionFactory = Arc<
    dyn Fn(usize) -> Pin<Box<dyn Future<Output = ExitReason> + Send>>
        + Send
        + Sync,
>;

/// A handle to a running PartitionSupervisor.
#[derive(Clone)]
pub struct PartitionSupervisorHandle {
    pid: ProcessId,
    partitions: usize,
    partition_pids: Arc<Vec<ProcessId>>,
    supervisor: SupervisorHandle,
}

impl PartitionSupervisorHandle {
    /// Get the supervisor's PID.
    pub fn pid(&self) -> ProcessId;

    /// Get the number of partitions.
    pub fn partitions(&self) -> usize;

    /// Route a key to a partition and return that partition's PID.
    /// Integer keys use `key % partitions`.
    /// Other keys are hashed.
    pub fn which_partition(&self, key: u64) -> ProcessId;

    /// Route by hash of any hashable key.
    pub fn which_partition_by_hash<K: Hash>(&self, key: &K) -> ProcessId;

    /// Get the PID of a specific partition by index.
    pub fn partition_pid(&self, index: usize) -> Option<ProcessId>;

    /// List all partition children with their status.
    /// Equivalent to PartitionSupervisor.which_children/1.
    pub async fn which_children(&self) -> Vec<PartitionChild>;

    /// Count children by status.
    /// Equivalent to PartitionSupervisor.count_children/1.
    pub async fn count_children(&self) -> PartitionCounts;

    /// Shut down the partition supervisor.
    pub fn shutdown(&self);
}

/// Information about a partition child.
#[derive(Debug, Clone)]
pub struct PartitionChild {
    pub partition: usize,
    pub pid: Option<ProcessId>,
    pub status: PartitionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStatus {
    Running,
    Restarting,
    Stopped,
}

#[derive(Debug, Clone)]
pub struct PartitionCounts {
    pub specs: usize,
    pub active: usize,
}

/// Start a PartitionSupervisor.
pub async fn start_partition_supervisor(
    runtime: Arc<Runtime>,
    spec: PartitionSupervisorSpec,
    factory: PartitionFactory,
) -> PartitionSupervisorHandle;

// --- GenServer integration ---

/// Start a PartitionSupervisor where each partition is a GenServer.
/// Returns a handle that can route calls/casts to the correct partition.
pub async fn start_partitioned_gen_server<S: GenServer>(
    runtime: Arc<Runtime>,
    spec: PartitionSupervisorSpec,
    server_factory: impl Fn(usize) -> S + Send + Sync + 'static,
) -> PartitionedGenServerRef<S>;

/// A handle that routes GenServer calls to the correct partition.
#[derive(Clone)]
pub struct PartitionedGenServerRef<S: GenServer> {
    partitions: usize,
    refs: Arc<Vec<GenServerRef<S>>>,
}

impl<S: GenServer> PartitionedGenServerRef<S> {
    /// Route a call to the partition for this key.
    pub async fn call_partition<K: Hash>(
        &self,
        key: &K,
        msg: S::Call,
        timeout: Duration,
    ) -> Result<S::Reply, CallError>;

    /// Route a cast to the partition for this key.
    pub fn cast_partition<K: Hash>(
        &self,
        key: &K,
        msg: S::Cast,
    ) -> Result<(), SendError>;

    /// Get the GenServerRef for a specific partition.
    pub fn partition(&self, index: usize) -> Option<&GenServerRef<S>>;

    /// Number of partitions.
    pub fn partitions(&self) -> usize;
}
```

## File Structure

```
crates/rebar-core/src/partition_supervisor/
├── mod.rs          # pub mod + re-exports
├── types.rs        # PartitionSupervisorSpec, PartitionChild, etc.
├── engine.rs       # start_partition_supervisor()
└── gen_server.rs   # start_partitioned_gen_server(), PartitionedGenServerRef
```

## TDD Test Plan

### Unit Tests (basic lifecycle)
1. `starts_correct_number_of_partitions` — N children spawned
2. `default_partitions_is_num_cpus` — sensible default
3. `custom_partition_count` — override works
4. `shutdown_stops_all_partitions` — clean shutdown
5. `partition_pids_all_valid` — each PID in process table

### Unit Tests (routing)
6. `which_partition_deterministic` — same key always same partition
7. `which_partition_distributes` — different keys hit different partitions
8. `which_partition_by_hash` — hash-based routing works
9. `integer_key_uses_modulo` — `key % N` routing
10. `partition_pid_returns_correct_pid` — by index

### Unit Tests (supervision)
11. `partition_restarts_on_crash` — crashed partition restarted
12. `other_partitions_unaffected` — one_for_one isolation
13. `max_restarts_respected` — too many restarts → supervisor stops
14. `which_children_shows_status` — correct partition info
15. `count_children_accurate` — active vs specs

### Unit Tests (GenServer integration)
16. `partitioned_gen_server_starts` — all partition servers running
17. `call_partition_routes_correctly` — call reaches right partition
18. `cast_partition_routes_correctly` — cast reaches right partition
19. `different_keys_different_partitions` — load distributed
20. `partition_ref_by_index` — direct access works

### Integration Tests
21. `partition_supervisor_does_not_break_supervisor` — existing tests pass
22. `partition_supervisor_does_not_break_gen_server` — existing tests pass
23. `partitioned_counter_example` — realistic sharded counter
24. `partitioned_cache_example` — realistic sharded cache
25. `nested_under_regular_supervisor` — composable tree

## Existing Interface Guarantees

- `Supervisor` / `SupervisorHandle` — UNCHANGED (used internally)
- `GenServer` / `GenServerRef` — UNCHANGED (used internally)
- `Runtime` — UNCHANGED
- `ChildSpec` / `ChildEntry` — UNCHANGED
- New `pub mod partition_supervisor;` added to `lib.rs` — purely additive

---

## Elixir Reference Documentation

### PartitionSupervisor Module (Elixir v1.14+)

#### Overview

"A supervisor that starts multiple partitions of the same child." Addresses bottlenecks in large systems by distributing state across independent partitions.

Processes are routed using `{:via, PartitionSupervisor, {name, key}}` tuples.

#### start_link/1

```elixir
@spec start_link([start_link_option()]) :: Supervisor.on_start()
```

**Options:**
- `:name` (required) — atom or via tuple identifying the supervisor
- `:child_spec` (required) — child specification for each partition
- `:partitions` — number of partitions (defaults to `System.schedulers_online/0`)
- `:strategy` — restart strategy, defaults to `:one_for_one`
- `:max_restarts` — maximum restarts allowed (default: 3)
- `:max_seconds` — timeframe for max_restarts (default: 5)
- `:with_arguments` — function modifying arguments per partition

#### which_children/1

Returns tuples: partition ID, child PID (or `:restarting`), type, and modules.

#### count_children/1

Returns counts for specs, active children, supervisors, and workers.

#### partitions/1

Returns the number of partitions.

#### resize!/2

Adjusts partition count dynamically (cannot exceed initial count).

#### Routing Strategy

"If key is an integer, it is routed using `rem(abs(key), partitions)`" otherwise using `:erlang.phash2(key, partitions)`.

#### :with_arguments

Injects partition information into child initialization:
```elixir
with_arguments: fn [opts], partition ->
  [Keyword.put(opts, :partition, partition)]
end
```

#### Implementation Notes

Uses ETS tables or Registry internally. Generates child specs for each partition, operates as a standard supervisor with partition numbers as IDs.
