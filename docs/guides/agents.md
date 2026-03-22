# Shared State with Agent

An `Agent` is a process that wraps a single piece of state with get/update/cast operations. When multiple tasks need to read and modify shared data through simple closures -- without defining a custom message protocol -- Agent is the simplest option.

---

## Table of Contents

1. [Why Agent](#1-why-agent)
2. [How: A Shared Configuration Cache](#2-how-a-shared-configuration-cache)
3. [Agent vs GenServer](#3-agent-vs-genserver)
4. [Key Points](#4-key-points)
5. [See Also](#5-see-also)

---

## 1. Why Agent

Picture a configuration cache that multiple HTTP handlers read on every request and that an admin endpoint updates at runtime. You need concurrent reads, atomic updates, and no boilerplate.

- **`Arc<Mutex<T>>` works** but requires callers to hold locks, risks poisoning on panics, and has no timeout on contested access.
- **GenServer works** but requires defining `Call`, `Cast`, and `Reply` types, implementing three callbacks, and spawning an event loop -- overkill when the operations are just "read this field" and "update that field."

Agent sits between the two. It runs a process internally (so access is serialized and timeout-safe), but the API is four closures: `get`, `update`, `get_and_update`, and `cast`. No message types, no trait implementations.

---

## 2. How: A Shared Configuration Cache

### Starting an agent

`start_agent` takes a runtime and an initializer closure. The closure runs inside the agent process and produces the initial state.

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rebar_core::agent::{start_agent, AgentError, AgentRef};
use rebar_core::runtime::Runtime;

type Config = HashMap<String, String>;

async fn create_config_cache(rt: Arc<Runtime>) -> AgentRef {
    start_agent(Arc::clone(&rt), || {
        let mut config = HashMap::new();
        config.insert("db_host".into(), "localhost".into());
        config.insert("db_port".into(), "5432".into());
        config.insert("cache_ttl".into(), "300".into());
        config
    })
    .await
}
```

### Reading state with get

`get` takes a closure that receives `&S` and returns a value. The closure runs inside the agent process, so it sees a consistent snapshot. The return value is sent back to the caller.

```rust
async fn read_db_host(agent: &AgentRef) -> Result<String, AgentError> {
    agent
        .get(
            |config: &Config| {
                config
                    .get("db_host")
                    .cloned()
                    .unwrap_or_else(|| "unknown".into())
            },
            Duration::from_secs(1),
        )
        .await
}
```

### Updating state

`update` takes a closure that receives `&mut S`. The closure mutates the state in place. The caller waits until the update is applied.

```rust
async fn set_cache_ttl(agent: &AgentRef, ttl: u64) -> Result<(), AgentError> {
    agent
        .update(
            move |config: &mut Config| {
                config.insert("cache_ttl".into(), ttl.to_string());
            },
            Duration::from_secs(1),
        )
        .await
}
```

### Atomic get-and-update

`get_and_update` reads a value and modifies the state in a single atomic operation. The closure receives `&mut S` and returns the value to send back to the caller.

```rust
async fn rotate_db_host(agent: &AgentRef) -> Result<String, AgentError> {
    agent
        .get_and_update(
            |config: &mut Config| {
                let old = config
                    .get("db_host")
                    .cloned()
                    .unwrap_or_default();
                // Rotate to a new host.
                config.insert("db_host".into(), "replica-2.db.internal".into());
                old
            },
            Duration::from_secs(1),
        )
        .await
}
```

### Fire-and-forget with cast

`cast` queues an update without waiting for it to be applied. It returns immediately. Use this for non-critical updates where the caller does not need confirmation.

```rust
fn record_last_access(agent: &AgentRef) {
    let _ = agent.cast(|config: &mut Config| {
        config.insert(
            "last_access".into(),
            chrono::Utc::now().to_rfc3339(),
        );
    });
}
```

### Stopping the agent

`stop` shuts down the agent process. Operations after stop return `AgentError::Dead`.

```rust
async fn cleanup(agent: &AgentRef) {
    agent.stop(Duration::from_secs(1)).await.unwrap();

    // This will fail:
    let result = agent.get(|c: &Config| c.len(), Duration::from_secs(1)).await;
    assert!(result.is_err());
}
```

### Concurrent access from multiple tasks

`AgentRef` is `Clone`. Pass clones to any number of tasks. The agent serializes all operations internally.

```rust
async fn concurrent_readers(rt: Arc<Runtime>) {
    let agent = create_config_cache(Arc::clone(&rt)).await;

    let mut handles = Vec::new();
    for i in 0..10 {
        let a = agent.clone();
        handles.push(tokio::spawn(async move {
            let host = a
                .get(|c: &Config| c.get("db_host").cloned(), Duration::from_secs(1))
                .await
                .unwrap();
            println!("task {i}: db_host = {host:?}");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}
```

---

## 3. Agent vs GenServer

Use **Agent** when:

- Your operations are simple closures (read a field, update a map).
- You do not need distinct message types or pattern matching on messages.
- You do not need `handle_info` for timer ticks or external messages.

Use **GenServer** when:

- You have multiple distinct operations that benefit from a typed protocol (e.g., `Get`, `Put`, `Delete`, `Scan`).
- You need periodic work via `handle_info` (e.g., expiry sweeps, heartbeats).
- You need `handle_continue` for deferred initialization.
- You need `sys_get_state`/`sys_suspend` for production debugging.
- Your server manages resources beyond a single data structure (connections, file handles, subscriptions).

The rule of thumb: if you find yourself encoding an operation name into a string inside an Agent closure to dispatch on it, you want a GenServer.

---

## 4. Key Points

- **Agent is a process.** It has a PID, processes messages sequentially, and is cleaned up when stopped or when all `AgentRef` handles are dropped.
- **All operations take a timeout.** If the agent is overloaded, the caller gets `AgentError::Timeout` instead of blocking forever. The exception is `cast`, which is fire-and-forget.
- **State type is erased internally.** The agent stores state as `Box<dyn Any + Send>`. Your closures downcast to the concrete type. If you pass a closure with the wrong state type, it panics at runtime.
- **`get` is read-only.** The closure receives `&S`. It cannot mutate state. Use `update` or `get_and_update` for mutations.
- **`get_and_update` is atomic.** The read and the mutation happen in the same message processing step. No other operation can interleave.
- **`cast` does not wait.** It enqueues the update and returns. Use it for best-effort writes where confirmation is not needed.
- **`AgentRef` is `Clone + Send + Sync`.** Share it freely across tasks and threads.

---

## 5. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `start_agent`, `AgentRef`, `AgentError`
- [Building Stateful Services with GenServer](gen-server.md) -- when you need a typed message protocol and lifecycle callbacks
- [Parallel Work with Task](tasks.md) -- when you need one-shot computation, not persistent state
