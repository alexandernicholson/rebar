# Plan: Timer (send_after, send_interval, handle_continue)

## Summary

Implement Erlang's `:timer` semantics — `send_after`, `send_interval`, `cancel`, and related time-based message delivery. Also add `handle_continue` to GenServer for deferred initialization. These are foundational primitives used constantly in BEAM applications.

## Rebar Integration Points

- **Runtime**: Timer functions need access to the router for message delivery
- **ProcessId**: Timers target specific PIDs
- **GenServer**: Add `handle_continue` callback, timer helpers on GenServerContext
- **ProcessContext**: Add `send_after`/`send_interval` convenience methods

## Proposed API

```rust
// --- Timer Module ---

/// An opaque reference to a running timer. Used with cancel().
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerRef(u64);

/// Timer errors.
#[derive(Debug, thiserror::Error)]
pub enum TimerError {
    #[error("timer not found")]
    NotFound,
    #[error("timer already cancelled")]
    AlreadyCancelled,
}

/// Send a message to `dest` after `delay`.
/// Equivalent to :timer.send_after/3.
/// Returns a TimerRef that can be used to cancel the timer.
pub fn send_after(
    router: &Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    delay: Duration,
) -> TimerRef;

/// Send a message to `dest` repeatedly at `interval`.
/// Equivalent to :timer.send_interval/3.
pub fn send_interval(
    router: &Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    interval: Duration,
) -> TimerRef;

/// Execute a function after `delay`.
/// Equivalent to :timer.apply_after/2.
pub fn apply_after<F, Fut>(delay: Duration, f: F) -> TimerRef
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static;

/// Execute a function repeatedly at `interval`.
/// Equivalent to :timer.apply_interval/2.
pub fn apply_interval<F, Fut>(interval: Duration, f: F) -> TimerRef
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static;

/// Cancel a running timer.
/// Equivalent to :timer.cancel/1.
pub fn cancel(timer: TimerRef) -> Result<(), TimerError>;

// --- ProcessContext extensions ---

impl ProcessContext {
    /// Send a message to self after a delay.
    pub fn send_after(&self, payload: rmpv::Value, delay: Duration) -> TimerRef;

    /// Send a message to a PID after a delay.
    pub fn send_after_to(&self, dest: ProcessId, payload: rmpv::Value, delay: Duration) -> TimerRef;

    /// Send a message to self at regular intervals.
    pub fn send_interval(&self, payload: rmpv::Value, interval: Duration) -> TimerRef;
}

// --- GenServer handle_continue ---

#[async_trait]
pub trait GenServer: Send + Sync + 'static {
    // ... existing associated types ...
    type Continue: Send + 'static = (); // default to () for backwards compat

    // ... existing callbacks ...

    /// Called after init() or when a handler returns ContinueWith.
    /// Equivalent to Elixir's handle_continue/2.
    /// Default: does nothing.
    async fn handle_continue(
        &self,
        _msg: Self::Continue,
        _state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {}
}

// GenServerContext gets timer helpers
impl GenServerContext {
    pub fn send_after(&self, payload: rmpv::Value, delay: Duration) -> TimerRef;
    pub fn send_interval(&self, payload: rmpv::Value, interval: Duration) -> TimerRef;
}
```

### Implementation Strategy

Timers are implemented as tokio tasks with cancellation tokens:

```rust
struct TimerState {
    cancel_tx: oneshot::Sender<()>,
}

// Global timer registry (per-runtime)
static TIMER_REGISTRY: Lazy<DashMap<TimerRef, TimerState>> = ...;
```

Each `send_after` spawns a `tokio::spawn` that sleeps then routes the message. `send_interval` spawns a loop. The `TimerRef` maps to a cancel channel. `cancel()` sends on that channel, causing the timer task to abort.

### handle_continue Implementation

The `Continue` associated type defaults to `()` via Rust's associated type defaults (requires `#![feature(associated_type_defaults)]` or a workaround). For backwards compatibility, the default implementation of `handle_continue` does nothing.

The engine loop adds a continue queue: after `init()` returns or a handler returns a continue action, the continue message is processed before the next external message.

**Backwards Compatibility Note**: Adding `type Continue` with a default and `handle_continue` with a default impl means existing GenServer implementations compile unchanged.

## File Structure

```
crates/rebar-core/src/timer/
├── mod.rs          # pub mod + re-exports
├── types.rs        # TimerRef, TimerError
└── engine.rs       # send_after, send_interval, apply_after, apply_interval, cancel
```

## TDD Test Plan

### Unit Tests (timer)
1. `send_after_delivers_message` — message arrives after delay
2. `send_after_does_not_deliver_early` — no message before delay
3. `send_interval_delivers_repeatedly` — multiple messages at interval
4. `cancel_stops_send_after` — cancelled timer doesn't fire
5. `cancel_stops_send_interval` — cancelled interval stops repeating
6. `cancel_nonexistent_returns_error` — invalid TimerRef → error
7. `apply_after_runs_function` — function executes after delay
8. `apply_interval_runs_repeatedly` — function runs at interval
9. `multiple_timers_independent` — timers don't interfere
10. `send_after_to_dead_process` — message routed, no panic

### Unit Tests (ProcessContext timer methods)
11. `context_send_after_to_self` — message to self after delay
12. `context_send_after_to_other` — message to another process after delay
13. `context_send_interval` — periodic self-messages

### Unit Tests (handle_continue)
14. `handle_continue_called_after_init` — continue runs post-init
15. `handle_continue_before_next_message` — continue processed first
16. `handle_continue_default_noop` — existing GenServers unaffected
17. `handle_continue_with_state_mutation` — state updated in continue

### Integration Tests
18. `existing_gen_server_tests_pass` — no regression
19. `timer_with_supervisor` — timers work in supervised processes
20. `timer_cleanup_on_process_death` — timers cancelled when process dies

## Existing Interface Guarantees

- `GenServer` trait — EXTENDED (new optional associated type + callback with defaults)
- `ProcessContext` — EXTENDED (new methods, no changes to existing)
- `GenServerContext` — EXTENDED (new methods, no changes to existing)
- `Runtime` — UNCHANGED
- Existing GenServer implementations compile unchanged (defaults provided)

---

## Elixir/Erlang Reference Documentation

### Erlang Timer Module (stdlib)

#### Overview

The timer module provides time-related functions where time is measured in milliseconds. All timer functions return immediately. Successful calls return a timer reference (`TRef`) usable with `cancel/1`.

#### Core Functions

##### send_after/2 and send_after/3

```erlang
send_after(Time, Message) -> {ok, TRef} | {error, Reason}
send_after(Time, Destination, Message) -> {ok, TRef} | {error, Reason}
```

Sends a message to a destination after specified milliseconds. The 2-argument version defaults to `self()`.

##### send_interval/2 and send_interval/3

```erlang
send_interval(Time, Message) -> {ok, TRef} | {error, Reason}
send_interval(Time, Destination, Message) -> {ok, TRef} | {error, Reason}
```

Repeatedly sends messages at time intervals. Creates an interval timer linked to the receiving process.

##### apply_after/2, apply_after/3, apply_after/4

```erlang
apply_after(Time, Function) -> {ok, TRef} | {error, Reason}
apply_after(Time, Module, Function, Arguments) -> {ok, TRef} | {error, Reason}
```

Spawns a new process executing the function after the specified delay.

**Important:** Functions execute in a freshly-spawned process, so `self()` returns the spawned process PID, not the caller's PID.

##### apply_interval/2, apply_interval/3, apply_interval/4

```erlang
apply_interval(Time, Function) -> {ok, TRef} | {error, Reason}
```

Spawns processes repeatedly at intervals regardless of whether prior processes completed.

**Warning:** Long execution times combined with short intervals can spawn excessive processes.

##### apply_repeatedly/2, apply_repeatedly/3, apply_repeatedly/4

Spawns processes repeatedly but waits for each to finish before starting the next.

##### cancel/1

```erlang
cancel(TRef) -> {ok, cancel} | {error, Reason}
```

Cancels a timer using its reference.

##### exit_after/2 and kill_after/1

Send exit signals to processes after a delay.

#### Timer Classification

**One-shot timers** (not linked): `apply_after/*`, `send_after/*`, `exit_after/*`, `kill_after/*`
- Removed only at timeout or explicit `cancel/1` call

**Interval timers** (linked): `apply_interval/*`, `apply_repeatedly/*`, `send_interval/*`
- Linked to recipient process; removed when linked process terminates

### Elixir handle_continue

In Elixir's GenServer, `handle_continue/2` is called when `init/1` or any other callback returns `{:noreply, state, {:continue, term}}`. It runs immediately before processing the next message from the mailbox. This is used to split expensive initialization into a non-blocking init + deferred continue, preventing the calling process from blocking on GenServer.start_link.
