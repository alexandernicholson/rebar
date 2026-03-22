# Plan: Process Groups (pg)

## Summary

Implement Erlang's `pg` module — distributed named process groups. Processes can join/leave named groups, and you can broadcast/multicast to all members. Unlike Registry (name→pid), pg is name→[pid]. Critical for pub/sub patterns, fan-out, and load distribution.

## Rebar Integration Points

- **ProcessId**: Group members are identified by PID
- **ProcessTable**: Used to verify process liveness
- **MessageRouter**: Used to send messages to group members
- **Registry (rebar-cluster)**: Complementary but distinct — Registry is 1:1, pg is 1:N

## Proposed API

```rust
/// A process group scope. Groups are organized into scopes to
/// partition the namespace and reduce cross-scope traffic.
pub struct PgScope {
    name: String,
    groups: DashMap<String, Vec<ProcessId>>,
    monitors: DashMap<MonitorRef, PgMonitorState>,
}

/// A reference to a group monitor subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PgMonitorRef(u64);

/// Start a new pg scope. Equivalent to pg:start_link/1.
pub fn start_scope(name: impl Into<String>) -> Arc<PgScope>;

/// Default scope (lazily initialized).
pub fn default_scope() -> Arc<PgScope>;

impl PgScope {
    // --- Membership ---

    /// Add a process to a group. Can join multiple times.
    /// Equivalent to pg:join/3.
    pub fn join(&self, group: &str, pid: ProcessId);

    /// Add multiple processes to a group.
    pub fn join_many(&self, group: &str, pids: &[ProcessId]);

    /// Remove a process from a group (one membership).
    /// Equivalent to pg:leave/3.
    pub fn leave(&self, group: &str, pid: ProcessId) -> Result<(), PgError>;

    /// Remove multiple processes from a group.
    pub fn leave_many(&self, group: &str, pids: &[ProcessId]);

    // --- Queries ---

    /// Get all members of a group (across all nodes).
    /// Equivalent to pg:get_members/2.
    pub fn get_members(&self, group: &str) -> Vec<ProcessId>;

    /// Get members of a group on the local node only.
    /// Equivalent to pg:get_local_members/2.
    pub fn get_local_members(&self, group: &str, local_node_id: u64) -> Vec<ProcessId>;

    /// List all groups in this scope.
    /// Equivalent to pg:which_groups/1.
    pub fn which_groups(&self) -> Vec<String>;

    // --- Monitoring ---

    /// Subscribe to membership changes for a specific group.
    /// Returns current members + a monitor ref.
    /// Equivalent to pg:monitor/2.
    pub fn monitor(&self, group: &str) -> (PgMonitorRef, Vec<ProcessId>);

    /// Subscribe to all group changes in this scope.
    /// Equivalent to pg:monitor_scope/1.
    pub fn monitor_scope(&self) -> (PgMonitorRef, HashMap<String, Vec<ProcessId>>);

    /// Unsubscribe from group changes.
    /// Equivalent to pg:demonitor/2.
    pub fn demonitor(&self, monitor_ref: PgMonitorRef);

    // --- Utilities ---

    /// Remove all memberships for a process (called on process death).
    pub fn remove_pid(&self, pid: ProcessId);

    /// Remove all memberships for a node (called on node disconnect).
    pub fn remove_node(&self, node_id: u64);

    // --- Convenience: messaging ---

    /// Send a message to all members of a group.
    pub fn broadcast(
        &self,
        group: &str,
        from: ProcessId,
        payload: rmpv::Value,
        router: &dyn MessageRouter,
    ) -> Vec<Result<(), SendError>>;

    /// Send a message to one random member of a group.
    pub fn send_random(
        &self,
        group: &str,
        from: ProcessId,
        payload: rmpv::Value,
        router: &dyn MessageRouter,
    ) -> Result<(), SendError>;
}

/// Events delivered to monitor subscribers.
#[derive(Debug, Clone)]
pub enum PgEvent {
    Join { group: String, pids: Vec<ProcessId> },
    Leave { group: String, pids: Vec<ProcessId> },
}

#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("process not a member of group")]
    NotJoined,
    #[error("group does not exist")]
    NoSuchGroup,
}
```

## File Structure

```
crates/rebar-core/src/pg/
├── mod.rs          # pub mod + re-exports
├── types.rs        # PgScope, PgMonitorRef, PgEvent, PgError
└── scope.rs        # PgScope implementation
```

## TDD Test Plan

### Unit Tests (membership)
1. `join_adds_to_group` — process appears in get_members
2. `join_same_group_twice` — process appears twice
3. `join_multiple_groups` — process in multiple groups
4. `leave_removes_from_group` — process removed
5. `leave_one_membership_when_joined_twice` — only one removed
6. `leave_not_joined_returns_error` — error for non-member
7. `join_many_adds_all` — batch join
8. `leave_many_removes_all` — batch leave

### Unit Tests (queries)
9. `get_members_empty_group` — returns empty vec
10. `get_members_returns_all` — all members returned
11. `get_local_members_filters_by_node` — only local node PIDs
12. `which_groups_lists_all` — all groups in scope
13. `which_groups_empty_scope` — empty vec

### Unit Tests (lifecycle)
14. `remove_pid_clears_all_memberships` — process death cleanup
15. `remove_node_clears_all_for_node` — node disconnect cleanup
16. `empty_group_removed_after_last_leave` — no phantom groups

### Unit Tests (monitoring)
17. `monitor_returns_current_members` — snapshot on subscribe
18. `monitor_notifies_on_join` — PgEvent::Join delivered
19. `monitor_notifies_on_leave` — PgEvent::Leave delivered
20. `monitor_scope_tracks_all_groups` — scope-wide events
21. `demonitor_stops_notifications` — no events after unsubscribe

### Unit Tests (messaging)
22. `broadcast_sends_to_all_members` — all receive message
23. `broadcast_empty_group` — no errors, no sends
24. `send_random_picks_one_member` — exactly one receives

### Integration Tests
25. `pg_with_runtime_processes` — real spawned processes join/leave
26. `pg_cleanup_on_process_exit` — automatic cleanup when process dies
27. `multiple_scopes_independent` — scopes don't see each other

## Existing Interface Guarantees

- `ProcessId` — UNCHANGED
- `ProcessTable` — UNCHANGED (pg reads only)
- `MessageRouter` — UNCHANGED (pg uses existing route())
- `Registry` — UNCHANGED (pg is complementary, not replacement)
- New `pub mod pg;` added to `lib.rs` — purely additive

---

## Elixir/Erlang Reference Documentation

### Erlang pg Module (kernel)

#### Overview

The `pg` module implements distributed named process groups. It enables messaging to one, multiple, or all group members across different nodes with "strong eventual consistency."

#### Core Concepts

**Group Management**: Groups are automatically created when processes join and removed when all leave. A process can join multiple groups and the same group multiple times.

**Scopes**: Process groups organize into independent overlay networks. The default scope `pg` starts automatically. Custom scopes can be created for traffic reduction.

#### Membership Operations

**`join(Group, PidOrPids)` / `join(Scope, Group, PidOrPids)`**
- Adds single process or list to a group
- Processes may join same group multiple times
- Returns `ok`

**`leave(Group, PidOrPids)` / `leave(Scope, Group, PidOrPids)`**
- Removes process(es) from group
- Returns `ok` or `not_joined`

**`get_members(Group)` / `get_members(Scope, Group)`**
- Returns all processes in group across all nodes
- Optimized for speed, no specific order

**`get_local_members(Group)` / `get_local_members(Scope, Group)`**
- Returns processes in group on local node only

#### Monitoring

**`monitor(Group)` / `monitor(Scope, Group)`**
- Subscribes caller to group membership updates
- Returns tuple: `{Reference, [CurrentMembers]}`

**`monitor_scope()` / `monitor_scope(Scope)`**
- Subscribes to all changes in scope
- Generates messages: `{Ref, join, Group, [Pids]}` or `{Ref, leave, Group, [Pids]}`

**`demonitor(Ref)` / `demonitor(Scope, Ref)`**
- Unsubscribes from updates, flushes pending messages

#### Important Characteristics

- **Automatic cleanup**: Members removed when process terminates
- **No transitive visibility**: Nodes must be directly connected
- **Eventual consistency**: Membership views may temporarily diverge across nodes
- **Local-only scopes**: Use unique names to keep scopes local

All functions available since OTP 23.0 except monitoring functions (OTP 25.1+).
