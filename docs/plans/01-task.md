# Plan: Task / Task.Supervisor

## Summary

Implement Elixir's `Task` module — lightweight async work primitives that are simpler than GenServer for one-shot computations. Includes `Task.async`/`Task.await`, `Task.async_stream`, and `Task.Supervisor` for supervised task execution.

## Rebar Integration Points

- **Runtime**: Uses `Runtime::spawn()` internally to create task processes
- **ProcessId**: Tasks have PIDs, are first-class processes
- **Supervisor/DynamicSupervisor**: `TaskSupervisor` wraps `DynamicSupervisor` with task-specific ergonomics
- **ProcessContext**: Tasks run within a process context but expose a simpler API

## Proposed API

```rust
// --- Core Task ---

/// A handle to a spawned async task.
pub struct Task<T> {
    pid: ProcessId,
    result_rx: oneshot::Receiver<T>,
}

/// Spawn a task and get a handle to await its result.
/// Equivalent to Elixir's Task.async/1.
pub async fn async_task<F, Fut, T>(runtime: &Runtime, f: F) -> Task<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static;

/// Spawn a task with access to a ProcessContext.
pub async fn async_task_with_context<F, Fut, T>(runtime: &Runtime, f: F) -> Task<T>
where
    F: FnOnce(ProcessContext) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static;

impl<T> Task<T> {
    /// Block until the task completes. Equivalent to Task.await/2.
    pub async fn await_result(self, timeout: Duration) -> Result<T, TaskError>;

    /// Non-blocking check. Equivalent to Task.yield/2.
    pub async fn yield_result(&mut self, timeout: Duration) -> Option<Result<T, TaskError>>;

    /// Shut down the task gracefully. Equivalent to Task.shutdown/2.
    pub async fn shutdown(self, timeout: Duration) -> Option<T>;

    /// Get the task's PID.
    pub fn pid(&self) -> ProcessId;
}

/// Errors from task operations.
pub enum TaskError {
    Timeout,
    ProcessDead,
}

// --- Task.async_stream ---

/// Process a collection concurrently with bounded parallelism.
/// Equivalent to Task.async_stream/3.
pub fn async_stream<I, F, Fut, T>(
    runtime: Arc<Runtime>,
    items: I,
    f: F,
    opts: StreamOpts,
) -> impl Stream<Item = Result<T, TaskError>>
where
    I: IntoIterator,
    F: Fn(I::Item) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static;

pub struct StreamOpts {
    pub max_concurrency: usize,  // default: num_cpus
    pub ordered: bool,           // default: true
    pub timeout: Duration,       // default: 5s
}

// --- Task.Supervisor ---

/// A supervisor specifically for dynamic tasks.
pub struct TaskSupervisor {
    handle: DynamicSupervisorHandle,
}

impl TaskSupervisor {
    /// Start a new TaskSupervisor.
    pub async fn start(runtime: Arc<Runtime>) -> Self;

    /// Spawn a supervised async task (equivalent to Task.Supervisor.async/2).
    pub async fn async_task<F, Fut, T>(&self, f: F) -> Task<T>;

    /// Spawn a fire-and-forget supervised task (Task.Supervisor.start_child/2).
    pub async fn start_child<F, Fut>(&self, f: F) -> Result<ProcessId, String>;
}
```

## File Structure

```
crates/rebar-core/src/task/
├── mod.rs          # pub mod + re-exports
├── types.rs        # Task<T>, TaskError, StreamOpts
├── engine.rs       # async_task(), async_task_with_context(), async_stream()
└── supervisor.rs   # TaskSupervisor
```

## TDD Test Plan

### Unit Tests (task/engine.rs)
1. `async_task_returns_result` — basic spawn + await
2. `async_task_timeout_returns_error` — await with expired timeout
3. `async_task_pid_is_valid` — task has a real PID in process table
4. `async_task_panicking_returns_error` — panic → TaskError::ProcessDead
5. `async_task_with_context_can_send_messages` — context available to task
6. `yield_returns_none_when_not_ready` — non-blocking check
7. `yield_returns_some_when_complete` — result available
8. `shutdown_returns_result_if_complete` — already done
9. `shutdown_returns_none_if_timeout` — kills running task

### Unit Tests (task/supervisor.rs)
10. `task_supervisor_starts` — basic creation
11. `supervised_task_completes` — async_task via supervisor
12. `supervised_task_restart_on_crash` — task crashes, supervisor handles it
13. `start_child_fire_and_forget` — no result tracking
14. `multiple_concurrent_supervised_tasks` — parallel execution

### Unit Tests (async_stream)
15. `async_stream_processes_all_items` — all items processed
16. `async_stream_respects_max_concurrency` — bounded parallelism
17. `async_stream_ordered_results` — ordered: true preserves order
18. `async_stream_unordered_results` — ordered: false allows reordering
19. `async_stream_timeout_per_item` — individual item timeout
20. `async_stream_empty_input` — empty iterator → empty stream

### Integration Tests
21. `task_cleanup_after_completion` — process table cleaned up
22. `task_does_not_break_existing_spawn` — existing Runtime::spawn still works
23. `task_supervisor_under_regular_supervisor` — composable supervision trees

## Existing Interface Guarantees

- `Runtime::spawn()` — UNCHANGED
- `ProcessContext` — UNCHANGED
- `ProcessId` — UNCHANGED
- `DynamicSupervisor` — used internally, no API changes
- New `pub mod task;` added to `lib.rs` — purely additive

---

## Elixir Reference Documentation

### Task Module (Elixir v1.19.5)

#### Overview

The Task module provides conveniences for spawning and managing concurrent processes. Tasks are lightweight processes designed to execute specific actions with minimal inter-process communication.

**Primary Use Case:** Converting sequential code to concurrent execution:
```elixir
task = Task.async(fn -> do_some_work() end)
res = do_some_other_work()
res + Task.await(task)
```

Tasks include monitoring and error logging compared to raw `spawn/1` processes.

#### Key Function Signatures

##### Async/Await Pattern

**`Task.async/1`** - Spawns a linked, monitored process
- Takes a zero-arity anonymous function
- Returns a Task struct
- *Critical:* Must be awaited; always sends a message to caller
- Links caller and spawned process (both crash if one crashes)

**`Task.async/3`** - Module-based async
- Arguments: `module, function_name, args`
- Stores MFA metadata for reflection
- Same linking/monitoring behavior as async/1

**`Task.await/2`** - Blocks waiting for task completion
- Arguments: `task, timeout \\ 5000`
- Returns the task result
- Timeout in milliseconds or `:infinity`
- Exits caller if timeout exceeded or task dies
- Can only be called once per task

**`Task.await_many/2`** - Waits for multiple tasks
- Arguments: `tasks_list, timeout \\ 5000`
- Returns results in original order
- Exits if any task dies

##### Stream Processing

**`Task.async_stream/3`** - Concurrent stream with anonymous function
- Arguments: `enumerable, fun, options \\ []`
- Function must be one-arity
- Each element processed by its own task

**`Task.async_stream/5`** - Concurrent stream with MFA
- Arguments: `enumerable, module, function_name, args, options \\ []`
- Element prepended to args
- Tasks linked to intermediate process, then to caller

**Stream Options:**
- `:max_concurrency` - Number of simultaneous tasks (defaults to `System.schedulers_online/0`)
- `:ordered` - Whether results maintain input order (default: `true`)
- `:timeout` - Per-task timeout in milliseconds (default: 5000)
- `:on_timeout` - `:exit` (default, kills caller) or `:kill_task` (kills task only)
- `:zip_input_on_exit` - Include original input in exit tuples (default: `false`)

**Stream Results:** Each element emits `{:ok, value}` on success or `{:exit, reason}` on failure.

##### Yielding

**`Task.yield/2`** - Non-blocking task check
- Arguments: `task, timeout \\ 5000`
- Returns: `{:ok, result}`, `{:exit, reason}`, or `nil`
- Allows multiple calls on same task
- Monitor remains active after timeout

**`Task.yield_many/2`** - Check multiple tasks
- Returns list of `{task, result}` tuples
- Options: `:limit`, `:timeout`, `:on_timeout`
- `:on_timeout` values: `:nothing`, `:ignore`, `:kill_task`

##### Shutdown & Control

**`Task.shutdown/2`** - Graceful task termination
- Arguments: `task, shutdown \\ 5000`
- Second arg: timeout or `:brutal_kill`
- Returns: `{:ok, reply}`, `{:exit, reason}`, or `nil`
- Unlinks before shutdown
- Safe to call even if task already completed

**`Task.ignore/1`** - Detach from task
- Returns `{:ok, reply}`, `{:exit, reason}`, or `nil`
- Task continues running independently
- Cannot await/yield afterward

##### Task Lifecycle

**`Task.start/1`** and **`Task.start/3`** - Fire-and-forget
- For side-effects only (I/O, logging)
- Returns `{:ok, pid}`
- No result retrieval

**`Task.start_link/1`** and **`Task.start_link/3`** - Supervised startup
- For supervision trees
- Returns `{:ok, pid}`

**`Task.completed/1`** - Pre-completed task
- Arguments: `result_value`
- Returns Task immediately with result
- No spawned process
- Useful for mixed sync/async workflows

##### Task Struct

```
%Task{
  mfa: {module, function, arity},    # Metadata from async/3
  owner: pid,                         # Calling process PID
  pid: pid | nil,                     # Task process PID
  ref: ref                            # Opaque monitor reference
}
```

##### Supervised Tasks (Task.Supervisor)

**Key Operations:**
- `start_child/2` - Fire-and-forget task
- `async/2` or `async/4` - Await-able task
- `async_nolink/2` - Task without caller linking
- `async_stream/6` - Concurrent stream processing

##### Important Constraints

**Async/Await Requirements:**
1. Must always await or yield on async tasks (messages always sent)
2. Linking couples caller and task failure
3. Data is copied to task process (consider extraction for large data)

**OTP Behavior Compatibility:**
- Avoid `await` inside GenServer callbacks
- Instead, match task messages in `handle_info/2`
- Receive two messages: `{ref, result}` and `{:DOWN, ref, :process, pid, reason}`
- Consider `Task.Supervisor.async_nolink/3` to prevent GenServer crashes
