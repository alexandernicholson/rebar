# Pub/Sub with Process Groups

The `pg` module provides process groups: named sets of processes that you can join, leave, query, and broadcast to. When you need one-to-many messaging where the set of recipients changes at runtime -- chat rooms, event subscribers, service discovery -- process groups are the right abstraction.

---

## Table of Contents

1. [Why Process Groups](#1-why-process-groups)
2. [How: A Chat Room System](#2-how-a-chat-room-system)
3. [PG vs Registry](#3-pg-vs-registry)
4. [Key Points](#4-key-points)
5. [See Also](#5-see-also)

---

## 1. Why Process Groups

Picture a chat application where users join rooms, messages are broadcast to everyone in the room, and users leave at any time. You need:

- **Dynamic membership.** Users join and leave rooms continuously. The set of recipients is not known at compile time.
- **One-to-many delivery.** When a user sends a message, every member of the room receives it.
- **Multi-group membership.** A user can be in multiple rooms simultaneously. A room can have many users.
- **Cleanup on disconnect.** When a user's process dies, it should be removed from all groups automatically.

You could maintain a `HashMap<String, Vec<ProcessId>>` behind a mutex. But then you need to handle cleanup, concurrent modification, and broadcasting yourself. `PgScope` does all of this with a lock-free concurrent map and a single `broadcast` call.

---

## 2. How: A Chat Room System

### Creating a scope

Groups are organized into scopes. A scope is an isolated namespace -- groups in one scope do not interfere with groups in another. Most applications use a single scope.

```rust
use std::sync::Arc;

use rebar_core::pg::PgScope;
use rebar_core::process::ProcessId;

// Create a scope for the chat system.
let chat_scope: Arc<PgScope> = PgScope::new();
```

`PgScope::new()` returns an `Arc<PgScope>`. Share this arc across tasks and processes.

### Joining and leaving groups

```rust
fn user_joins_room(scope: &PgScope, room: &str, user_pid: ProcessId) {
    scope.join(room, user_pid);
    println!("{user_pid:?} joined #{room}");
}

fn user_leaves_room(scope: &PgScope, room: &str, user_pid: ProcessId) {
    match scope.leave(room, user_pid) {
        Ok(()) => println!("{user_pid:?} left #{room}"),
        Err(e) => println!("leave failed: {e}"),
    }
}
```

A process can join the same group multiple times. Each call to `join` adds one membership. Each call to `leave` removes one membership. This mirrors Erlang's `pg` semantics and is useful for processes that serve multiple roles in the same group.

### Querying group membership

```rust
fn list_room_members(scope: &PgScope, room: &str) {
    let members = scope.get_members(room);
    println!("#{room} has {} members: {members:?}", members.len());
}

fn list_local_members(scope: &PgScope, room: &str, node_id: u64) {
    // Only members on a specific node.
    let local = scope.get_local_members(room, node_id);
    println!("#{room} local members: {local:?}");
}

fn list_all_rooms(scope: &PgScope) {
    let rooms = scope.which_groups();
    println!("active rooms: {rooms:?}");
}

fn room_size(scope: &PgScope, room: &str) -> usize {
    scope.member_count(room)
}
```

`get_members` returns an empty `Vec` for groups that do not exist, so you do not need to check for existence first.

### Broadcasting messages

`broadcast` sends a message to every member of a group through the message router. It returns a result per member, so you can see which sends succeeded and which failed.

```rust
use rebar_core::router::MessageRouter;

fn send_chat_message(
    scope: &PgScope,
    room: &str,
    sender_pid: ProcessId,
    text: &str,
    router: &dyn MessageRouter,
) {
    let payload = rmpv::Value::Map(vec![
        (
            rmpv::Value::String("from".into()),
            rmpv::Value::String(format!("{sender_pid:?}").into()),
        ),
        (
            rmpv::Value::String("text".into()),
            rmpv::Value::String(text.into()),
        ),
    ]);

    let results = scope.broadcast(room, sender_pid, &payload, router);

    let delivered = results.iter().filter(|r| r.is_ok()).count();
    let failed = results.iter().filter(|r| r.is_err()).count();
    println!("broadcast to #{room}: {delivered} delivered, {failed} failed");
}
```

### Joining multiple processes at once

```rust
fn bulk_join(scope: &PgScope, room: &str, pids: &[ProcessId]) {
    scope.join_many(room, pids);
    println!("added {} members to #{room}", pids.len());
}
```

### Cleanup on process death

When a process dies, call `remove_pid` to clear all its memberships across all groups in the scope. This prevents stale PIDs from accumulating and broadcast failures from piling up.

```rust
fn handle_process_exit(scope: &PgScope, dead_pid: ProcessId) {
    scope.remove_pid(dead_pid);
    println!("{dead_pid:?} removed from all groups");
}
```

For node-level cleanup (e.g., when a remote node disconnects), use `remove_node`:

```rust
fn handle_node_disconnect(scope: &PgScope, node_id: u64) {
    scope.remove_node(node_id);
    println!("all processes from node {node_id} removed");
}
```

### Full example: chat room lifecycle

```rust
use rebar_core::pg::PgScope;
use rebar_core::process::ProcessId;
use rebar_core::process::table::ProcessTable;
use rebar_core::router::LocalRouter;

async fn chat_example() {
    let scope = PgScope::new();
    let table = Arc::new(ProcessTable::new(1));
    let router = LocalRouter::new(Arc::clone(&table));

    // Simulate three users.
    let alice = ProcessId::new(1, 1);
    let bob = ProcessId::new(1, 2);
    let carol = ProcessId::new(1, 3);

    // Alice and Bob join #general.
    scope.join("general", alice);
    scope.join("general", bob);

    // Carol joins #random.
    scope.join("random", carol);

    // Bob is also in #random.
    scope.join("random", bob);

    assert_eq!(scope.get_members("general").len(), 2);
    assert_eq!(scope.get_members("random").len(), 2);
    assert_eq!(scope.which_groups().len(), 2);

    // Bob disconnects -- remove from all rooms.
    scope.remove_pid(bob);
    assert_eq!(scope.get_members("general").len(), 1); // just alice
    assert_eq!(scope.get_members("random").len(), 1);  // just carol

    // Alice leaves #general explicitly.
    scope.leave("general", alice).unwrap();
    assert_eq!(scope.member_count("general"), 0);

    // Empty groups are cleaned up automatically.
    // Only "random" (with carol) remains.
    let groups = scope.which_groups();
    assert_eq!(groups, vec!["random"]);
}
```

---

## 3. PG vs Registry

The process **registry** (`register`/`whereis`) maps a **name to one process** (1:1). Process **groups** map a **group name to many processes** (1:N).

| Feature | Registry | PG |
|---|---|---|
| Cardinality | 1 name : 1 PID | 1 group : N PIDs |
| Use case | Named services, singletons | Pub/sub, fan-out, service pools |
| Duplicate membership | Not allowed (name collision) | Allowed (join same group twice) |
| Messaging | `send_named` (to one) | `broadcast` (to all members) |
| Scope | Global per runtime | Per `PgScope` instance |

Use the registry when you have a single well-known service (e.g., "config_server"). Use PG when you have a dynamic set of interested processes (e.g., "users in room X").

---

## 4. Key Points

- **`PgScope::new()` returns `Arc<PgScope>`.** Share this arc across tasks. All operations are lock-free (backed by `DashMap`).
- **Join is additive.** Calling `join` twice adds two memberships. `leave` removes one. This matches Erlang's `pg` behavior.
- **`leave` returns `PgError::NotJoined` if the PID is not a member.** Check the result or ignore it with `let _ =`.
- **`get_members` returns an empty `Vec` for unknown groups.** No need to check existence first.
- **`broadcast` returns one result per member.** Dead processes produce `SendError::ProcessDead`. The broadcast does not stop on the first failure.
- **`remove_pid` cleans all groups.** Call this on process death to prevent stale membership. Empty groups are removed automatically.
- **`remove_node` removes all processes for a node.** Use this for cluster-level cleanup when a node disconnects.
- **Groups are not durable.** They exist only in memory. When all members leave, the group disappears. When the scope is dropped, all groups are gone.

---

## 5. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `PgScope`, `PgError`, `PgEvent`
- [Building Stateful Services with GenServer](gen-server.md) -- processes that join groups and handle broadcasts in `handle_info`
- [Sharding with PartitionSupervisor](partition-supervisor.md) -- when you need deterministic routing (1:1 key-to-process) instead of broadcast (1:N)
