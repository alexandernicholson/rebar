# Plan: Agent

## Summary

Implement Elixir's `Agent` module — a simple abstraction around state. Agent provides `get(fun)` / `update(fun)` / `get_and_update(fun)` operations without requiring users to define custom message types. It's a thin wrapper over GenServer, dramatically reducing boilerplate for simple shared state.

## Rebar Integration Points

- **GenServer**: Agent is implemented as a GenServer internally
- **Runtime**: Uses `spawn_gen_server()` under the hood
- **ProcessId**: Agents have PIDs, are first-class processes

## Proposed API

```rust
/// A handle to a running Agent process.
#[derive(Clone)]
pub struct AgentRef {
    pid: ProcessId,
    call_tx: mpsc::Sender<AgentCall>,
    cast_tx: mpsc::UnboundedSender<AgentCast>,
}

/// Start a new Agent with the given initial state.
/// Equivalent to Agent.start_link/2.
pub async fn start_agent<S, F>(runtime: Arc<Runtime>, init: F) -> AgentRef
where
    S: Send + 'static,
    F: FnOnce() -> S + Send + 'static;

impl AgentRef {
    /// Get the agent's PID.
    pub fn pid(&self) -> ProcessId;

    /// Read from agent state via a function.
    /// Equivalent to Agent.get/3.
    pub async fn get<F, T>(&self, f: F, timeout: Duration) -> Result<T, AgentError>
    where
        F: FnOnce(&S) -> T + Send + 'static,
        T: Send + 'static;

    /// Update the agent state via a function.
    /// Equivalent to Agent.update/3.
    pub async fn update<F>(&self, f: F, timeout: Duration) -> Result<(), AgentError>
    where
        F: FnOnce(&mut S) + Send + 'static;

    /// Get a value and update state atomically.
    /// Equivalent to Agent.get_and_update/3.
    pub async fn get_and_update<F, T>(&self, f: F, timeout: Duration) -> Result<T, AgentError>
    where
        F: FnOnce(&mut S) -> (T, S) + Send + 'static,
        T: Send + 'static;

    /// Fire-and-forget state update.
    /// Equivalent to Agent.cast/2.
    pub fn cast<F>(&self, f: F) -> Result<(), AgentError>
    where
        F: FnOnce(&mut S) + Send + 'static;

    /// Stop the agent.
    /// Equivalent to Agent.stop/3.
    pub async fn stop(self) -> Result<(), AgentError>;
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent timeout")]
    Timeout,
    #[error("agent dead")]
    Dead,
}
```

### Implementation Note: Type Erasure

The challenge in Rust is that `AgentRef` needs to be `Clone` and usable without generic state type `S` in the signature. The solution is to use boxed closures (`Box<dyn FnOnce>`) internally and type-erase the state behind the channel:

```rust
// Internal message types (not public)
type AgentCall = Box<dyn FnOnce(&mut dyn Any) -> Box<dyn Any + Send> + Send>;
type AgentCast = Box<dyn FnOnce(&mut dyn Any) + Send>;
```

The `AgentRef` methods take generic closures but box + type-erase them before sending over the channel. The agent process holds the concrete state as `Box<dyn Any>` and downcasts.

## File Structure

```
crates/rebar-core/src/agent/
├── mod.rs          # pub mod + re-exports
└── agent.rs        # AgentRef, start_agent(), internal GenServer impl
```

## TDD Test Plan

### Unit Tests
1. `agent_start_with_initial_state` — start returns valid AgentRef
2. `agent_get_reads_state` — get returns computed value
3. `agent_update_modifies_state` — update changes state, subsequent get sees it
4. `agent_get_and_update_atomic` — returns value and updates atomically
5. `agent_cast_fire_and_forget` — cast returns immediately, state updated eventually
6. `agent_stop_shuts_down` — stop returns Ok, agent no longer responds
7. `agent_timeout_on_slow_get` — get with short timeout on blocked agent
8. `agent_dead_returns_error` — operations after stop return AgentError::Dead
9. `agent_concurrent_access` — multiple tasks get/update same agent
10. `agent_complex_state` — HashMap or struct as state
11. `agent_pid_is_valid` — pid in process table during lifetime
12. `agent_cleanup_after_stop` — process removed from table

### Integration Tests
13. `agent_does_not_break_gen_server` — existing GenServer tests still pass
14. `agent_under_supervisor` — agent as supervised child
15. `multiple_agents_independent` — agents don't interfere

## Existing Interface Guarantees

- `GenServer` trait — UNCHANGED
- `spawn_gen_server()` — UNCHANGED
- `GenServerRef` — UNCHANGED
- New `pub mod agent;` added to `lib.rs` — purely additive

---

## Elixir Reference Documentation

### Agent Module (Elixir)

#### Overview

"Agents are a simple abstraction around state." The Agent module enables sharing and storing state across different processes or at various time points within the same process.

#### Types

- **agent()**: A reference that can be a PID, a tuple with atom and node, or a registered name.
- **name()**: An atom, global tuple, or via-module tuple for process registration.
- **on_start()**: Returns `{:ok, pid}` on success or `{:error, reason}` on failure.
- **state()**: Any term representing the agent's internal state.

#### Core Functions

##### start_link/2
Starts an agent linked to the calling process. The provided function initializes the agent state. Options include `:name` for registration, `:timeout` for initialization deadline, `:debug` for sys module integration, and `:spawn_opt` for process options.

Returns `{:ok, pid}` or `{:error, {:already_started, pid}}`.

##### start/2
Identical to start_link/2 but creates an unlinked process outside supervision trees.

##### get/3
"Gets an agent value via the given anonymous function." The function receives state and returns a computed value. Timeout defaults to 5000ms.

##### update/3
"Updates the agent state via the given anonymous function." The function receives current state and returns the new state. Returns `:ok` with 5000ms default timeout.

##### get_and_update/3
Combines retrieval and update: "The function must return a tuple with two elements, the first being the value to return (that is, the 'get' value) and the second one being the new state."

##### cast/2
"Performs a cast (fire and forget) operation on the agent state." Returns `:ok` immediately without waiting for completion.

##### stop/3
Synchronously halts the agent with specified reason and timeout. Returns `:ok`.

#### Key Architectural Patterns

**Client-Server Segregation**: Functions passed to Agent operations execute within the agent process (server), not the caller's process (client). This distinction matters for performance — expensive operations in the server block the agent, while computation on the client requires copying state.

**Distributed Considerations**: The API that accepts anonymous functions only works if the caller and the agent have the same version of the caller module. Use explicit module/function/args APIs for distributed systems.

#### Supervision Tree Integration

Using `use Agent` automatically generates a `child_spec/1` function. Configuration options include `:id` (defaults to module name), `:restart` (`:permanent` by default), and `:shutdown` (termination strategy).
