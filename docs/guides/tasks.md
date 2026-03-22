# Parallel Work with Task

The `task` module provides structured concurrency for one-shot asynchronous work. When you need to run a function in the background and collect its result later -- or fan out the same operation over many inputs with bounded parallelism -- tasks are the right tool.

---

## Table of Contents

1. [Why Task](#1-why-task)
2. [How: Fetching Data from Multiple APIs](#2-how-fetching-data-from-multiple-apis)
3. [Key Points](#3-key-points)
4. [See Also](#4-see-also)

---

## 1. Why Task

Imagine a dashboard endpoint that aggregates data from three internal APIs: user profiles, recent orders, and notification counts. Calling them sequentially triples the latency. You need to:

- **Run calls concurrently** and wait for all of them.
- **Set timeouts per call** so one slow service does not block the others.
- **Cancel work that is no longer needed** if the user disconnects.
- **Limit parallelism** when fanning out to many targets so you do not overwhelm a downstream service.

Raw `tokio::spawn` solves the first point but gives you an untyped `JoinHandle` with no timeout, no cancellation, and no process identity. Rebar's task module gives you all four while keeping each task as a real process in the runtime (with a PID and a mailbox).

---

## 2. How: Fetching Data from Multiple APIs

### Individual async tasks

`async_task` spawns a function in the background and returns a `Task<T>` handle. The function runs as a process with its own PID.

```rust
use std::sync::Arc;
use std::time::Duration;

use rebar_core::runtime::Runtime;
use rebar_core::task::{async_task, TaskError};

async fn fetch_dashboard(rt: &Runtime) {
    // Spawn three concurrent API calls.
    let mut profiles = async_task(rt, || async {
        // Simulate HTTP call to user service.
        tokio::time::sleep(Duration::from_millis(80)).await;
        vec!["alice", "bob"]
    })
    .await;

    let mut orders = async_task(rt, || async {
        tokio::time::sleep(Duration::from_millis(120)).await;
        42_u64 // total recent orders
    })
    .await;

    let mut notifications = async_task(rt, || async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        7_u64 // unread count
    })
    .await;

    // Await each result with a timeout.
    let timeout = Duration::from_secs(2);

    let profile_result = profiles.await_result(timeout).await;
    let order_result = orders.await_result(timeout).await;
    let notif_result = notifications.await_result(timeout).await;

    match (profile_result, order_result, notif_result) {
        (Ok(users), Ok(count), Ok(unread)) => {
            println!("users: {users:?}, orders: {count}, unread: {unread}");
        }
        _ => {
            println!("one or more API calls failed or timed out");
        }
    }
}
```

### Progress checking with yield_result

When you cannot afford to block on a result, `yield_result` does a non-blocking check. Unlike `await_result`, it can be called repeatedly.

```rust
use rebar_core::task::{async_task, TaskError};

async fn poll_until_ready(rt: &rebar_core::runtime::Runtime) {
    let mut task = async_task(rt, || async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        "done"
    })
    .await;

    loop {
        match task.yield_result(Duration::from_millis(10)).await {
            Some(Ok(value)) => {
                println!("task completed: {value}");
                break;
            }
            Some(Err(e)) => {
                println!("task failed: {e}");
                break;
            }
            None => {
                // Still running. Do other work.
                println!("still waiting...");
            }
        }
    }
}
```

### Cancellation with shutdown

If a task is no longer needed, `shutdown` aborts it and returns the result if the task happened to finish first.

```rust
async fn with_cancellation(rt: &rebar_core::runtime::Runtime) {
    let task = async_task(rt, || async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        "this will never be returned"
    })
    .await;

    // User disconnected -- cancel the work.
    let result = task.shutdown().await;
    assert!(result.is_none()); // task was killed before completing
}
```

### Bounded parallel work with async_map

When you need to apply the same operation to many inputs, `async_map` handles concurrency limiting, per-task timeouts, and result ordering.

```rust
use rebar_core::task::{async_map, StreamOpts, TaskError};

async fn fetch_all_users(rt: Arc<Runtime>, user_ids: Vec<u64>) {
    let results: Vec<Result<String, TaskError>> = async_map(
        Arc::clone(&rt),
        user_ids,
        |user_id| async move {
            // Simulate an HTTP call per user.
            tokio::time::sleep(Duration::from_millis(50)).await;
            format!("user_{user_id}")
        },
        StreamOpts {
            max_concurrency: 10,   // at most 10 concurrent HTTP calls
            ordered: true,         // results match input order
            timeout: Duration::from_secs(5), // per-task timeout
        },
    )
    .await;

    for result in &results {
        match result {
            Ok(name) => println!("fetched: {name}"),
            Err(TaskError::Timeout) => println!("request timed out"),
            Err(TaskError::ProcessDead) => println!("task crashed"),
        }
    }
}
```

`StreamOpts::default()` sets `max_concurrency` to the number of CPU cores, `ordered` to `true`, and `timeout` to 5 seconds.

### Tasks with process context

When a task needs to send or receive messages (not just return a value), use `async_task_ctx`:

```rust
use rebar_core::task::async_task_ctx;

async fn task_that_sends(rt: &Runtime, target: rebar_core::process::ProcessId) {
    let mut task = async_task_ctx(rt, move |ctx| async move {
        ctx.send(target, rmpv::Value::String("hello from task".into()))
            .await
            .unwrap();
        true
    })
    .await;

    let sent = task.await_result(Duration::from_secs(1)).await.unwrap();
    assert!(sent);
}
```

### Fire-and-forget tasks

When you do not need the result at all, `start_task` avoids the overhead of a result channel:

```rust
use rebar_core::task::start_task;

async fn background_cleanup(rt: &Runtime) {
    let pid = start_task(rt, || async {
        // Cleanup logic that runs in the background.
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("cleanup complete");
    })
    .await;

    println!("cleanup started as process {pid:?}");
}
```

---

## 3. Key Points

- **Tasks are processes.** Each task has a PID and lives in the process table. It is cleaned up automatically when it completes.
- **`await_result` consumes the receiver.** You can only call it once. Use `yield_result` for repeated polling.
- **`yield_result` is non-blocking.** It returns `None` if the task has not finished within the given timeout, without consuming the receiver.
- **`shutdown` is immediate.** It checks for a result first, then aborts the underlying tokio task. Use it for cleanup when work is no longer needed.
- **`async_map` preserves order when `ordered: true`.** Results are sorted by input index before being returned, regardless of the order tasks complete.
- **`async_map` respects `max_concurrency`.** A semaphore limits how many tasks run simultaneously. Use this to avoid overwhelming downstream services.
- **`async_task` vs `async_task_ctx`.** Use `async_task` for pure computation. Use `async_task_ctx` when the task needs to send messages or interact with other processes.
- **`start_task` vs `async_task`.** Use `start_task` when you do not need the result. It skips the oneshot channel and is slightly cheaper.

---

## 4. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `Task`, `async_task`, `async_map`, `StreamOpts`, `TaskError`
- [Building Stateful Services with GenServer](gen-server.md) -- when you need a long-lived process, not a one-shot task
- [Back-Pressure Pipelines with GenStage](gen-stage.md) -- when tasks need to form a demand-driven pipeline
