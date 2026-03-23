# Work Coordination with the Coordinator

## Why

You have a pool of worker processes and a stream of tasks. You need to:
- Route tasks to workers efficiently
- Avoid piling small tasks behind slow workers
- Automatically remove dead workers
- Scale by registering/unregistering workers at runtime

The Coordinator solves this with **response-time-weighted least-loaded scheduling**: it tracks each worker's average response time and in-flight task count, then routes each task to the worker with the lowest estimated load.

## How It Works

### Scheduling Algorithm

The coordinator scores each worker as:

```
score = (in_flight + 1) * avg_response_us
```

- **`in_flight`**: number of tasks currently being processed by this worker
- **`+1`**: accounts for the task about to be sent, so even idle workers are ranked by their historical speed
- **`avg_response_us`**: exponential moving average of response time (alpha = 0.3)

A worker averaging 5ms per task scores **40x lower** than one averaging 200ms at the same in-flight count. This prevents small tasks from piling behind workers handling heavy work.

New workers (no history) score 1, so they're explored first.

### Example: Task Pool

```rust
use rebar_core::coordinator::{start_coordinator, CoordinatorSpec};
use rebar_core::runtime::Runtime;
use rebar_core::process::ProcessId;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let rt = Arc::new(Runtime::new(1));
    let coord = start_coordinator(rt.clone(), CoordinatorSpec::default()).await;

    // Spawn 3 worker processes
    for _ in 0..3 {
        let worker = rt.spawn(|mut ctx| async move {
            while let Some(msg) = ctx.recv().await {
                if let rmpv::Value::Map(entries) = msg.payload().clone() {
                    let mut task_val: i64 = 0;
                    let mut reply_node: u64 = 0;
                    let mut reply_local: u64 = 0;
                    for (k, v) in &entries {
                        match k.as_str().unwrap_or("") {
                            "task" => task_val = v.as_i64().unwrap_or(0),
                            "reply_to_node" => reply_node = v.as_u64().unwrap_or(0),
                            "reply_to_local" => reply_local = v.as_u64().unwrap_or(0),
                            _ => {}
                        }
                    }
                    let result = rmpv::Value::from(task_val * 2);
                    let _ = ctx.send(
                        ProcessId::new(reply_node, reply_local),
                        result,
                    ).await;
                }
            }
        }).await;

        coord.register_worker(worker, rmpv::Value::Nil).await.unwrap();
    }

    // Submit tasks — coordinator routes to least-loaded worker
    let result = coord.submit(rmpv::Value::from(21), Duration::from_secs(1)).await.unwrap();
    assert_eq!(result.as_i64().unwrap(), 42);

    // Submit a batch concurrently
    let tasks: Vec<rmpv::Value> = (1..=10).map(|i| rmpv::Value::from(i)).collect();
    let results = coord.submit_many(tasks, Duration::from_secs(5)).await;
    println!("All {} tasks completed", results.len());
}
```

### Worker Protocol

Workers receive a `rmpv::Value::Map` with three keys:

| Key | Type | Description |
|-----|------|-------------|
| `"task"` | any | The task payload from `submit()` |
| `"reply_to_node"` | u64 | Reply PID node |
| `"reply_to_local"` | u64 | Reply PID local |

Workers must send their result to `ProcessId::new(reply_to_node, reply_to_local)`.

### Mixed Workloads

With round-robin, a worker handling a 10-second task gets the same share as one handling a 10ms task. With response-time weighting:

| Worker | Avg Response | In-Flight | Score |
|--------|-------------|-----------|-------|
| A (fast) | 10ms | 0 | (0+1) * 10,000 = **10,000** |
| B (slow) | 10s | 0 | (0+1) * 10,000,000 = **10,000,000** |

Worker A scores 1000x lower — it gets the next task. Even under concurrent load:

| Worker | Avg Response | In-Flight | Score |
|--------|-------------|-----------|-------|
| A (fast) | 10ms | 5 | (5+1) * 10,000 = **60,000** |
| B (slow) | 10s | 1 | (1+1) * 10,000,000 = **20,000,000** |

Worker A with 5 tasks still scores 333x lower than B with 1 task.

### Dead Worker Removal

When `submit()` tries to send to a worker and gets `SendError::ProcessDead`, the coordinator automatically removes that worker and retries with the next-best worker. No manual cleanup needed.

## Key Points

- **No automatic discovery**: Workers must call `register_worker(pid, metadata)` explicitly. SWIM gossip integration is planned for future.
- **Response times are learned**: The first task to each worker seeds the EMA. After that, the scheduler adapts continuously.
- **In-flight tracking is exact**: `TaskComplete` messages go through a priority channel, processed before new submits.
- **Metadata is arbitrary**: Attach `rmpv::Value` metadata at registration for your own routing/filtering logic.

## See Also

- [API Reference — Coordinator](../api/rebar-core.md)
- [Building Stateful Services with GenServer](gen-server.md)
- [Sharding with PartitionSupervisor](partition-supervisor.md)
