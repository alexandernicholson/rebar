# Sharding with PartitionSupervisor

A `PartitionSupervisor` starts N identical child processes and routes work to them by key. When you need to distribute load across independent workers -- rate limiters, connection pools, cache shards -- without building routing logic yourself, PartitionSupervisor handles the sharding.

---

## Table of Contents

1. [Why PartitionSupervisor](#1-why-partitionsupervisor)
2. [How: A Partitioned Rate Limiter](#2-how-a-partitioned-rate-limiter)
3. [PartitionSupervisor vs Regular Supervisor with N Children](#3-partitionsupervisor-vs-regular-supervisor-with-n-children)
4. [Key Points](#4-key-points)
5. [See Also](#5-see-also)

---

## 1. Why PartitionSupervisor

Picture a rate limiter for an API gateway. Each incoming request carries an API key. You need to track how many requests each key has made in the current window and reject requests that exceed the limit.

A single rate limiter process becomes a bottleneck under high load -- every request serializes through one mailbox. The solution is to shard: run N rate limiter processes, each responsible for a subset of API keys. Route each request to the right shard based on the key's hash.

You could do this manually:

1. Start N children under a regular supervisor.
2. Build a routing table mapping keys to child PIDs.
3. Update the table when a child restarts (new PID).

PartitionSupervisor does all three for you. You provide a factory function that creates each partition, and the handle gives you `which_partition(key)` to route by integer or `which_partition_by_hash(key)` to route by hash.

---

## 2. How: A Partitioned Rate Limiter

### Defining the partition factory

A `PartitionFactory` is an `Arc<dyn Fn(usize) -> Pin<Box<dyn Future<Output = ExitReason> + Send>>>`. Each invocation receives the zero-based partition index and returns a future that runs until the partition should stop.

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rebar_core::partition_supervisor::{
    start_partition_supervisor, PartitionFactory, PartitionSupervisorHandle,
    PartitionSupervisorSpec,
};
use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;

/// Create a rate limiter factory. Each partition tracks request counts
/// for a subset of API keys within a sliding window.
fn rate_limiter_factory(
    max_requests: u64,
    window: Duration,
) -> PartitionFactory {
    Arc::new(move |partition_index| {
        Box::pin(async move {
            println!("rate limiter partition {partition_index} started");

            let mut counts: HashMap<String, u64> = HashMap::new();

            // In a real implementation, this process would receive messages
            // via its mailbox (check_rate / reset commands) and respond
            // with allow/deny decisions. Here we simulate a long-lived
            // worker that processes requests.
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }

            #[allow(unreachable_code)]
            ExitReason::Normal
        })
    })
}
```

### Starting the partition supervisor

```rust
async fn start_rate_limiter(rt: Arc<Runtime>) -> PartitionSupervisorHandle {
    let spec = PartitionSupervisorSpec::new()
        .partitions(8)           // 8 shards
        .max_restarts(10)        // allow 10 restarts...
        .max_seconds(60);        // ...within 60 seconds

    let factory = rate_limiter_factory(100, Duration::from_secs(60));

    start_partition_supervisor(rt, spec, factory).await
}
```

`PartitionSupervisorSpec::new()` defaults to the number of CPU cores for `partitions`. Override with `.partitions(n)`.

### Routing requests to partitions

The handle provides two routing methods:

#### By integer key

```rust
fn route_by_integer(
    handle: &PartitionSupervisorHandle,
    api_key_id: u64,
) -> rebar_core::process::ProcessId {
    // Uses key % partitions for deterministic routing.
    handle.which_partition(api_key_id)
}
```

#### By hash of any hashable key

```rust
fn route_by_hash(
    handle: &PartitionSupervisorHandle,
    api_key: &str,
) -> rebar_core::process::ProcessId {
    // Hashes the key, then applies hash % partitions.
    handle.which_partition_by_hash(&api_key)
}
```

Both methods are deterministic: the same key always routes to the same partition. This is critical for rate limiting -- all requests for a given API key must hit the same counter.

### Using the routed PID

Once you have the partition's PID, send messages to it through the runtime's normal messaging:

```rust
async fn check_rate(
    handle: &PartitionSupervisorHandle,
    api_key: &str,
    ctx: &rebar_core::runtime::ProcessContext,
) {
    let partition_pid = handle.which_partition_by_hash(&api_key);

    // Send a rate check request to the appropriate partition.
    ctx.send(
        partition_pid,
        rmpv::Value::Map(vec![
            (
                rmpv::Value::String("action".into()),
                rmpv::Value::String("check_rate".into()),
            ),
            (
                rmpv::Value::String("api_key".into()),
                rmpv::Value::String(api_key.into()),
            ),
        ]),
    )
    .await
    .unwrap();
}
```

### Accessing partitions directly

You can get a specific partition's PID by its zero-based index:

```rust
fn inspect_partition(handle: &PartitionSupervisorHandle, index: usize) {
    match handle.partition_pid(index) {
        Some(pid) => println!("partition {index} is at {pid:?}"),
        None => println!("partition {index} does not exist"),
    }
}
```

### Restart behavior

Each partition is an independent child of the underlying supervisor. When a partition crashes, only that partition restarts (OneForOne strategy by default). Other partitions are unaffected.

```rust
async fn restart_demo() {
    let rt = Arc::new(Runtime::new(1));

    // A factory where partition 0 crashes, others stay alive.
    let factory: PartitionFactory = Arc::new(|index| {
        Box::pin(async move {
            if index == 0 {
                // Simulate a crash.
                ExitReason::Abnormal("out of memory".into())
            } else {
                loop {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                #[allow(unreachable_code)]
                ExitReason::Normal
            }
        })
    });

    let spec = PartitionSupervisorSpec::new()
        .partitions(4)
        .max_restarts(5)
        .max_seconds(10);

    let handle = start_partition_supervisor(rt, spec, factory).await;

    // Partition 0 will restart up to 5 times in 10 seconds.
    // Partitions 1, 2, 3 run undisturbed.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // All 4 partitions still have valid PIDs for routing.
    for i in 0..4 {
        let pid = handle.partition_pid(i).unwrap();
        println!("partition {i}: {pid:?}");
    }

    handle.shutdown();
}
```

### Shutdown

```rust
async fn cleanup(handle: PartitionSupervisorHandle) {
    // Shuts down all partitions and the underlying supervisor.
    handle.shutdown();
}
```

### Builder chain

The spec supports a fluent builder pattern:

```rust
use rebar_core::supervisor::spec::RestartStrategy;

let spec = PartitionSupervisorSpec::new()
    .partitions(16)
    .strategy(RestartStrategy::OneForOne)
    .max_restarts(20)
    .max_seconds(30);
```

---

## 3. PartitionSupervisor vs Regular Supervisor with N Children

You can achieve the same result by starting N children under a regular supervisor and maintaining your own routing table. PartitionSupervisor is preferable because:

| Aspect | Regular Supervisor + Manual Routing | PartitionSupervisor |
|---|---|---|
| Setup | Define N child specs, build a PID table, handle restarts | One `PartitionFactory` + one `PartitionSupervisorSpec` |
| Routing | Build and maintain a `HashMap<usize, ProcessId>` | `which_partition(key)` / `which_partition_by_hash(key)` |
| Restart handling | Update routing table when child restarts with new PID | Handled internally (PIDs are cached at startup) |
| API | Generic supervisor API | Purpose-built API with `partition_pid()`, `partitions()` |

Use a regular supervisor when:

- Children are heterogeneous (different logic per child).
- You need custom routing logic beyond `key % N`.
- You need dynamic child addition/removal at runtime.

Use PartitionSupervisor when:

- All children run the same logic, parameterized by partition index.
- You want deterministic key-based routing.
- You do not need to add or remove partitions after startup.

---

## 4. Key Points

- **Partitions default to CPU count.** `PartitionSupervisorSpec::new()` sets `partitions` to `std::thread::available_parallelism()`. Override with `.partitions(n)`.
- **`which_partition(key)` uses `key % partitions`.** Deterministic. Same key always routes to the same partition. Fast (no hashing).
- **`which_partition_by_hash(key)` uses `DefaultHasher`.** Works with any `Hash` type (strings, tuples, custom structs). Deterministic within a single process (Rust's `DefaultHasher` is not stable across builds, but is consistent within a runtime).
- **`PartitionSupervisorHandle` is `Clone`.** Share it across tasks. Routing methods read from a cached `Vec<ProcessId>` and do not contact the supervisor.
- **Each partition has a unique PID.** `partition_pid(index)` returns `None` for out-of-bounds indices.
- **Restart strategy applies per partition.** By default, `OneForOne` means only the crashed partition restarts. Use `OneForAll` if all partitions must restart together.
- **`shutdown()` stops everything.** The underlying supervisor terminates all partition children.
- **Zero partitions panics.** The spec asserts `partitions > 0` at startup. This is a programming error, not a runtime condition.
- **The factory is called on restarts.** When a partition crashes and is restarted, the factory is called again with the same index. The factory must be callable multiple times (`Fn`, not `FnOnce`).

---

## 5. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `PartitionSupervisorSpec`, `PartitionSupervisorHandle`, `start_partition_supervisor`, `PartitionFactory`
- [Pub/Sub with Process Groups](process-groups.md) -- when you need broadcast (1:N) instead of sharded routing (1:1)
- [Application Lifecycle Management](applications.md) -- starting partition supervisors as part of an application boot sequence
