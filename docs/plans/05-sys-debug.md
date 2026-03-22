# Plan: Sys Debug Interface

## Summary

Implement Erlang's `:sys` debug interface — introspection into GenServer/GenStatem state at runtime. Provides `get_state`, `replace_state`, `suspend`, `resume`, `trace`, and `statistics` without stopping or restarting processes. Essential for production debugging.

## Rebar Integration Points

- **GenServer**: Existing GenServer needs to respond to sys messages
- **GenServerRef**: Extended with sys debug methods
- **GenServerContext**: May need debug state tracking
- **gen_server/engine.rs**: Event loop needs to handle sys commands

## Proposed API

```rust
/// System debug commands sent to a process.
/// These are handled by the GenServer engine, not user callbacks.
pub enum SysCommand {
    GetState,
    ReplaceState(Box<dyn FnOnce(Box<dyn Any + Send>) -> Box<dyn Any + Send> + Send>),
    Suspend,
    Resume,
    GetStatus,
}

/// Debug options that can be installed on a process.
#[derive(Debug, Clone)]
pub struct DebugOpts {
    pub trace: bool,
    pub log: Option<usize>,          // max events to keep
    pub statistics: bool,
}

/// System event types for trace/log output.
#[derive(Debug, Clone)]
pub enum SystemEvent {
    In { msg_type: String, from: ProcessId },
    Out { msg_type: String, to: ProcessId },
    StateChange { description: String },
    Noreply,
}

/// Process status returned by get_status.
#[derive(Debug)]
pub struct ProcessStatus {
    pub pid: ProcessId,
    pub status: ProcessRunState,
    pub debug: DebugOpts,
    pub state_description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRunState {
    Running,
    Suspended,
}

/// Statistics collected when enabled.
#[derive(Debug, Clone)]
pub struct ProcessStatistics {
    pub start_time: std::time::Instant,
    pub messages_in: u64,
    pub messages_out: u64,
    pub reductions: u64, // approximate work units
}

/// Sys debug operations on a GenServerRef.
/// These are purely additive methods on GenServerRef.
impl<S: GenServer> GenServerRef<S> {
    /// Get the current state of the server (for debugging).
    /// Equivalent to :sys.get_state/2.
    pub async fn sys_get_state(&self, timeout: Duration) -> Result<S::State, CallError>
    where
        S::State: Clone;

    /// Replace the server state using a transformation function.
    /// Equivalent to :sys.replace_state/3.
    pub async fn sys_replace_state<F>(
        &self,
        f: F,
        timeout: Duration,
    ) -> Result<S::State, CallError>
    where
        F: FnOnce(&mut S::State) + Send + 'static,
        S::State: Clone;

    /// Suspend the server (stops processing messages).
    /// Equivalent to :sys.suspend/2.
    pub async fn sys_suspend(&self, timeout: Duration) -> Result<(), CallError>;

    /// Resume a suspended server.
    /// Equivalent to :sys.resume/2.
    pub async fn sys_resume(&self, timeout: Duration) -> Result<(), CallError>;

    /// Get the server's status.
    /// Equivalent to :sys.get_status/2.
    pub async fn sys_get_status(&self, timeout: Duration) -> Result<ProcessStatus, CallError>;

    /// Enable/disable tracing.
    /// Equivalent to :sys.trace/3.
    pub async fn sys_trace(&self, enable: bool, timeout: Duration) -> Result<(), CallError>;

    /// Enable/disable statistics collection.
    /// Equivalent to :sys.statistics/3.
    pub async fn sys_statistics(
        &self,
        enable: bool,
        timeout: Duration,
    ) -> Result<Option<ProcessStatistics>, CallError>;

    /// Enable/disable event logging.
    /// Equivalent to :sys.log/3.
    pub async fn sys_log(
        &self,
        enable: bool,
        max_events: usize,
        timeout: Duration,
    ) -> Result<Vec<SystemEvent>, CallError>;
}
```

### Implementation Strategy

The GenServer engine loop maintains internal debug state alongside user state:

```rust
struct InternalDebugState {
    suspended: bool,
    trace: bool,
    log: Option<VecDeque<SystemEvent>>,
    log_max: usize,
    statistics: Option<ProcessStatistics>,
}
```

Sys commands are sent via a dedicated channel (separate from call/cast) with highest priority. When suspended, the engine only processes sys commands, ignoring calls/casts/info until resumed.

## File Structure

```
crates/rebar-core/src/sys/
├── mod.rs          # pub mod + re-exports
├── types.rs        # SysCommand, DebugOpts, SystemEvent, ProcessStatus, etc.
└── debug.rs        # InternalDebugState, sys message handling logic
```

## TDD Test Plan

### Unit Tests (get_state / replace_state)
1. `get_state_returns_current` — read server state
2. `get_state_timeout` — returns error on timeout
3. `replace_state_modifies` — state changed via function
4. `replace_state_reflected_in_get` — subsequent get sees change

### Unit Tests (suspend / resume)
5. `suspend_stops_processing` — calls timeout while suspended
6. `resume_restarts_processing` — calls work after resume
7. `suspend_sys_commands_still_work` — get_state works while suspended
8. `double_suspend_idempotent` — no error
9. `resume_without_suspend_idempotent` — no error

### Unit Tests (trace)
10. `trace_enabled_logs_events` — events printed/captured
11. `trace_disabled_silent` — no output when off

### Unit Tests (statistics)
12. `statistics_tracks_messages_in` — counter increments
13. `statistics_tracks_messages_out` — counter increments
14. `statistics_disabled_returns_none` — no collection when off
15. `statistics_reset_on_enable` — fresh counters

### Unit Tests (log)
16. `log_captures_events` — events stored in ring buffer
17. `log_respects_max_events` — oldest evicted
18. `log_disabled_returns_empty` — no events when off

### Integration Tests
19. `sys_does_not_break_existing_gen_server` — all existing tests pass
20. `sys_with_supervised_gen_server` — works under supervisor
21. `sys_concurrent_debug_operations` — multiple sys calls don't race

## Existing Interface Guarantees

- `GenServer` trait — UNCHANGED (sys is engine-level, not callback-level)
- `GenServerRef<S>` — EXTENDED (new `sys_*` methods, no changes to existing)
- `spawn_gen_server()` — UNCHANGED (engine gains internal debug state)
- `CallError` — UNCHANGED
- New `pub mod sys;` added to `lib.rs` — purely additive

---

## Elixir/Erlang Reference Documentation

### Erlang sys Module (stdlib)

#### Overview

The `sys` module provides a functional interface to system messages for debugging and managing Erlang processes.

#### Process Control Functions

##### suspend/resume

```erlang
suspend(Name) -> ok
suspend(Name, Timeout) -> ok
resume(Name) -> ok
resume(Name, Timeout) -> ok
```

`suspend` pauses process execution; it responds only to system messages. `resume` restarts a suspended process.

##### get_state

```erlang
get_state(Name) -> State
get_state(Name, Timeout) -> State
```

Retrieves process state. Returns vary by process type:
- `gen_server`: callback module state
- `gen_statem`: tuple `{CurrentState, CurrentData}`
- `gen_event`: list of handler tuples

Designed for debugging only.

##### replace_state

```erlang
replace_state(Name, StateFun) -> NewState
replace_state(Name, StateFun, Timeout) -> NewState
```

Replaces process state using a provided function. `StateFun` receives current state and returns new state.

##### trace

```erlang
trace(Name, Flag) -> ok
trace(Name, Flag, Timeout) -> ok
```

Enables/disables printing all system events to standard_io.

##### log

```erlang
log(Name, Flag) -> ok | {ok, [system_event()]}
log(Name, Flag, Timeout) -> ok | {ok, [system_event()]}
```

Flags: `true` or `{true, N}` enable logging, `false` disables, `get` returns events, `print` prints events.

##### statistics

```erlang
statistics(Name, Flag) -> ok | {ok, Statistics}
```

Flags: `true` enables, `false` disables, `get` returns statistics.
Returns: `[{start_time, DateTime}, {current_time, DateTime}, {reductions, N}, {messages_in, N}, {messages_out, N}]`

##### get_status

```erlang
get_status(Name) -> Status
get_status(Name, Timeout) -> Status
```

Returns: `{status, Pid, {module, Module}, [SItem]}`
SItem elements: process dictionary, running/suspended, parent PID, debug options, module-specific Misc value.

#### System Event Types

```erlang
system_event() :: {in, Msg} | {in, Msg, State}
                | {out, Msg, To} | {out, Msg, To, State}
                | {noreply, State} | {continue, Continuation}
                | {terminate, Reason, State} | term()
```

#### Debug Installation

Custom debug functions can be installed that are called per system event:
```erlang
install(Name, {Func, FuncState}) -> ok
remove(Name, FuncOrId) -> ok
no_debug(Name) -> ok  % removes all debug functions
```

Standard behaviors (gen_server, gen_statem, gen_event) handle system messages automatically.
