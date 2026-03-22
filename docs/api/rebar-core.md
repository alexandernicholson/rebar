# rebar-core API Reference

`rebar-core` provides the local actor runtime for Rebar -- process spawning, message passing, monitoring, linking, and OTP-style supervision. Everything runs on a single node with zero networking. Distribution is handled by the separate `rebar-cluster` crate.

**Crate:** `rebar-core` v0.1.0<br>
**Dependencies:** tokio, dashmap, rmpv, serde, thiserror, tracing

---

## Module: `process`

Core process primitives -- identifiers, messages, exit reasons, error types, mailboxes, process tables, monitors, and links.

---

### `ProcessId`

A globally unique identifier for a process, scoped by node. Cheap to copy and suitable for use as a hash-map key.

#### Definition

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId {
    node_id: u64,
    local_id: u64,
}
```

#### Traits

`Debug`, `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`, `Display`

#### Methods

- `new(node_id: u64, local_id: u64) -> Self` -- Construct a ProcessId from a node ID and a node-local ID.
- `node_id(&self) -> u64` -- Return the node ID component.
- `local_id(&self) -> u64` -- Return the node-local ID component.

#### Display

Formats as `<node_id.local_id>`, e.g. `<1.42>`.

#### Example

```rust
use rebar_core::process::ProcessId;

let pid = ProcessId::new(1, 42);
assert_eq!(pid.node_id(), 1);
assert_eq!(pid.local_id(), 42);
assert_eq!(format!("{pid}"), "<1.42>");

// Copy semantics -- no move
let pid2 = pid;
assert_eq!(pid, pid2);

// Usable as HashMap key
let mut map = std::collections::HashMap::new();
map.insert(pid, "worker");
```

---

### `Message`

An envelope carrying a MessagePack payload between processes. Automatically stamped with a millisecond-precision Unix timestamp on creation.

#### Definition

```rust
#[derive(Debug, Clone)]
pub struct Message {
    from: ProcessId,
    payload: rmpv::Value,
    timestamp: u64,
}
```

#### Methods

- `new(from: ProcessId, payload: rmpv::Value) -> Self` -- Create a message. The `timestamp` field is set to the current time in milliseconds since the Unix epoch.
- `from(&self) -> ProcessId` -- Return the sender's PID.
- `payload(&self) -> &rmpv::Value` -- Return a reference to the MessagePack payload.
- `timestamp(&self) -> u64` -- Return the creation timestamp (ms since epoch).

#### Example

```rust
use rebar_core::process::{ProcessId, Message};

let from = ProcessId::new(1, 1);
let msg = Message::new(from, rmpv::Value::String("hello".into()));

assert_eq!(msg.from(), from);
assert_eq!(msg.payload().as_str(), Some("hello"));
assert!(msg.timestamp() > 0);
```

---

### `ExitReason`

Describes why a process terminated. Used by the supervisor to decide whether a child should be restarted.

#### Definition

```rust
#[derive(Debug, Clone)]
pub enum ExitReason {
    Normal,
    Abnormal(String),
    Kill,
    LinkedExit(ProcessId, Box<ExitReason>),
}
```

#### Variants

| Variant | Meaning |
|---------|---------|
| `Normal` | Process finished its work successfully. |
| `Abnormal(String)` | Process crashed with the given reason string. |
| `Kill` | Process was forcibly terminated. |
| `LinkedExit(ProcessId, Box<ExitReason>)` | Process died because a linked process (`ProcessId`) exited with the boxed reason. |

#### Methods

- `is_normal(&self) -> bool` -- Returns `true` only for the `Normal` variant.

#### Example

```rust
use rebar_core::process::ExitReason;

assert!(ExitReason::Normal.is_normal());
assert!(!ExitReason::Abnormal("crashed".into()).is_normal());
assert!(!ExitReason::Kill.is_normal());
```

---

### `SendError`

Error returned when sending a message fails. Derives `thiserror::Error` for automatic `Display` and `std::error::Error` implementations.

#### Definition

```rust
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("process dead: {0}")]
    ProcessDead(ProcessId),
    #[error("mailbox full for: {0}")]
    MailboxFull(ProcessId),
}
```

#### Variants

| Variant | Meaning |
|---------|---------|
| `ProcessDead(ProcessId)` | The target process no longer exists (receiver dropped). |
| `MailboxFull(ProcessId)` | The target's bounded mailbox is at capacity. |

#### Example

```rust
use rebar_core::process::{ProcessId, SendError};

let err = SendError::ProcessDead(ProcessId::new(1, 5));
assert_eq!(format!("{err}"), "process dead: <1.5>");
```

---

### `Mailbox`

Factory for creating mailbox channel pairs. A mailbox is a tokio mpsc channel wrapped in the `MailboxTx`/`MailboxRx` pair.

#### Definition

```rust
pub struct Mailbox;
```

#### Methods

- `unbounded() -> (MailboxTx, MailboxRx)` -- Create an unbounded mailbox. The sender will never block or fail due to capacity constraints; messages are only lost if the receiver is dropped.
- `bounded(capacity: usize) -> (MailboxTx, MailboxRx)` -- Create a bounded mailbox with the given capacity. When full, `try_send` returns `SendError::MailboxFull`.

#### Example

```rust
use rebar_core::process::mailbox::Mailbox;

// Unbounded -- no backpressure
let (tx, rx) = Mailbox::unbounded();

// Bounded -- backpressure at 100 messages
let (tx, rx) = Mailbox::bounded(100);
```

---

### `MailboxTx`

Sender half of a mailbox channel. Wraps either a bounded or unbounded tokio mpsc sender. Implements `Clone` so multiple producers can send to the same mailbox.

#### Definition

```rust
#[derive(Clone)]
pub struct MailboxTx {
    inner: TxInner, // private enum: Unbounded | Bounded
}
```

#### Methods

- `send(&self, msg: Message) -> Result<(), SendError>` -- Send a message to the mailbox. For unbounded channels, only fails if the receiver has been dropped (`ProcessDead`). For bounded channels, uses `try_send` semantics: returns `MailboxFull` if at capacity, `ProcessDead` if the receiver is dropped.
- `try_send(&self, msg: Message) -> Result<(), SendError>` -- Try to send a message without blocking. Identical to `send` for unbounded channels. For bounded channels, returns `MailboxFull` if the channel is at capacity.

---

### `MailboxRx`

Receiver half of a mailbox channel. Not cloneable -- only one consumer per mailbox.

#### Definition

```rust
pub struct MailboxRx {
    inner: RxInner, // private enum: Unbounded | Bounded
}
```

#### Methods

- `async recv(&mut self) -> Option<Message>` -- Receive the next message. Returns `None` if all senders have been dropped (channel closed).
- `async recv_timeout(&mut self, duration: Duration) -> Option<Message>` -- Receive a message with a timeout. Returns `Some(msg)` if a message arrives within the duration, `None` if the timeout expires or the channel is closed.

#### Example

```rust
use rebar_core::process::mailbox::Mailbox;
use rebar_core::process::{ProcessId, Message};
use std::time::Duration;

let (tx, mut rx) = Mailbox::unbounded();

tx.send(Message::new(ProcessId::new(1, 1), rmpv::Value::Nil)).unwrap();
let msg = rx.recv().await.unwrap();

// With timeout
let msg = rx.recv_timeout(Duration::from_millis(100)).await;
```

---

### `ProcessHandle`

A handle to a process, wrapping the mailbox sender. Stored in the process table to allow sending messages to a process by PID.

#### Definition

```rust
pub struct ProcessHandle {
    tx: MailboxTx,
}
```

#### Methods

- `new(tx: MailboxTx) -> Self` -- Create a new process handle wrapping the given mailbox sender.
- `send(&self, msg: Message) -> Result<(), SendError>` -- Send a message to this process's mailbox.

#### Example

```rust
use rebar_core::process::mailbox::{Mailbox, MailboxTx};
use rebar_core::process::table::ProcessHandle;
use rebar_core::process::{ProcessId, Message};

let (tx, mut rx) = Mailbox::unbounded();
let handle = ProcessHandle::new(tx);

let msg = Message::new(ProcessId::new(1, 0), rmpv::Value::Nil);
handle.send(msg).unwrap();
```

---

### `ProcessTable`

Registry of all processes on a node. Uses `DashMap` for concurrent access and `AtomicU64` for lock-free PID allocation. All methods are safe to call from multiple threads concurrently.

#### Definition

```rust
pub struct ProcessTable {
    node_id: u64,
    next_id: AtomicU64,
    processes: DashMap<ProcessId, ProcessHandle>,
}
```

#### Methods

- `new(node_id: u64) -> Self` -- Create a new process table for the given node ID. PID allocation starts at local_id 1.
- `allocate_pid(&self) -> ProcessId` -- Allocate a new unique process ID. Uses atomic fetch-and-add for lock-free allocation. PIDs start at 1 and increment monotonically.
- `insert(&self, pid: ProcessId, handle: ProcessHandle)` -- Insert a process handle into the table under the given PID.
- `get(&self, pid: &ProcessId) -> Option<Ref<'_, ProcessId, ProcessHandle>>` -- Look up a process by PID. Returns a `DashMap` reference guard that holds a read lock on the entry.
- `remove(&self, pid: &ProcessId) -> Option<(ProcessId, ProcessHandle)>` -- Remove a process from the table. Returns the removed PID and handle, or `None` if not found.
- `send(&self, pid: ProcessId, msg: Message) -> Result<(), SendError>` -- Send a message to a process by PID. Returns `SendError::ProcessDead` if the PID is not in the table.
- `len(&self) -> usize` -- Return the number of processes currently in the table.
- `is_empty(&self) -> bool` -- Return whether the table is empty.

#### Example

```rust
use rebar_core::process::table::{ProcessTable, ProcessHandle};
use rebar_core::process::mailbox::Mailbox;
use rebar_core::process::{ProcessId, Message};

let table = ProcessTable::new(1);
let pid = table.allocate_pid(); // <1.1>

let (tx, mut rx) = Mailbox::unbounded();
table.insert(pid, ProcessHandle::new(tx));

let msg = Message::new(ProcessId::new(1, 0), rmpv::Value::Nil);
table.send(pid, msg).unwrap();

assert_eq!(table.len(), 1);
assert!(!table.is_empty());

table.remove(&pid);
assert!(table.is_empty());
```

---

### `MonitorRef`

A unique reference identifying a specific monitor relationship. Allocated from a global atomic counter, so every `MonitorRef` is globally unique.

#### Definition

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef(u64);
```

#### Traits

`Debug`, `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`

#### Methods

- `new() -> Self` -- Allocate a new globally-unique monitor reference from an atomic counter.

#### Example

```rust
use rebar_core::process::monitor::MonitorRef;

let r1 = MonitorRef::new();
let r2 = MonitorRef::new();
assert_ne!(r1, r2); // every ref is unique

// Copy semantics
let r3 = r1;
assert_eq!(r1, r3);
```

---

### `MonitorSet`

Tracks monitor relationships: who is monitoring whom. Maintains a dual-index structure (by monitor ref and by target PID) for efficient lookup in both directions.

#### Definition

```rust
pub struct MonitorSet {
    by_ref: HashMap<MonitorRef, ProcessId>,
    by_target: HashMap<ProcessId, Vec<MonitorRef>>,
}
```

#### Methods

- `new() -> Self` -- Create a new empty monitor set.
- `add_monitor(&mut self, target: ProcessId) -> MonitorRef` -- Add a monitor on the given target process. Returns a unique `MonitorRef` that can be used to cancel this specific monitor later.
- `remove_monitor(&mut self, mref: MonitorRef)` -- Remove a monitor by its reference. No-op if the reference does not exist.
- `monitors_for(&self, target: ProcessId) -> impl Iterator<Item = MonitorRef> + '_` -- Iterate over all monitor refs that are watching the given target.

#### Example

```rust
use rebar_core::process::monitor::MonitorSet;
use rebar_core::process::ProcessId;

let mut monitors = MonitorSet::new();

let target = ProcessId::new(1, 5);
let mref = monitors.add_monitor(target);

// Query who is watching this target
assert_eq!(monitors.monitors_for(target).count(), 1);

// Cancel a specific monitor
monitors.remove_monitor(mref);
assert_eq!(monitors.monitors_for(target).count(), 0);
```

---

### `LinkSet`

Tracks bidirectional link relationships for a single process. Links are idempotent -- adding the same link twice has no additional effect.

#### Definition

```rust
pub struct LinkSet {
    links: HashSet<ProcessId>,
}
```

#### Methods

- `new() -> Self` -- Create a new empty link set.
- `add_link(&mut self, pid: ProcessId)` -- Add a link to the given process. Idempotent.
- `remove_link(&mut self, pid: ProcessId)` -- Remove a link to the given process.
- `is_linked(&self, pid: ProcessId) -> bool` -- Check whether this set contains a link to the given process.
- `linked_pids(&self) -> impl Iterator<Item = ProcessId> + '_` -- Iterate over all linked process IDs.

#### Example

```rust
use rebar_core::process::monitor::LinkSet;
use rebar_core::process::ProcessId;

let mut links = LinkSet::new();

let peer = ProcessId::new(1, 10);
links.add_link(peer);
assert!(links.is_linked(peer));

// Idempotent
links.add_link(peer);
assert_eq!(links.linked_pids().count(), 1);

links.remove_link(peer);
assert!(!links.is_linked(peer));
```

---

## Module: `router`

Message routing abstraction. The `MessageRouter` trait decouples how messages are delivered from the process runtime, allowing transparent local or distributed routing.

---

### `MessageRouter` (trait)

Trait for routing messages between processes. Implementations decide whether to deliver locally or over the network.

**Definition:**

```rust
pub trait MessageRouter: Send + Sync {
    fn route(
        &self,
        from: ProcessId,
        to: ProcessId,
        payload: rmpv::Value,
    ) -> Result<(), SendError>;
}
```

**Bounds:** `Send + Sync` — routers are shared across async tasks via `Arc<dyn MessageRouter>`.

---

### `LocalRouter`

Default router that delivers messages to the local `ProcessTable`. This is the router used by `Runtime::new()`.

**Definition:**

```rust
pub struct LocalRouter {
    table: Arc<ProcessTable>,
}
```

**Methods:**

- `new(table: Arc<ProcessTable>) -> Self` — Create a local router backed by the given process table.

**Implements:** `MessageRouter` — calls `table.send(to, Message::new(from, payload))`.

**Example:**

```rust
use std::sync::Arc;
use rebar_core::process::table::ProcessTable;
use rebar_core::router::{LocalRouter, MessageRouter};
use rebar_core::process::ProcessId;

let table = Arc::new(ProcessTable::new(1));
let router = LocalRouter::new(Arc::clone(&table));

let from = ProcessId::new(1, 0);
let to = ProcessId::new(1, 1);
// Routes locally through the ProcessTable
router.route(from, to, rmpv::Value::String("hello".into()));
```

---

## Module: `runtime`

The top-level runtime that owns the process table and provides spawn/send operations.

---

### `Runtime`

The Rebar runtime, responsible for spawning processes and routing messages. Each runtime owns a `ProcessTable` behind an `Arc` for safe concurrent access.

#### Definition

```rust
pub struct Runtime {
    node_id: u64,
    table: Arc<ProcessTable>,
    router: Arc<dyn MessageRouter>,
}
```

#### Methods

- `new(node_id: u64) -> Self` -- Create a new runtime for the given node ID.
- `with_router(node_id: u64, table: Arc<ProcessTable>, router: Arc<dyn MessageRouter>) -> Self` -- Create a runtime with a custom message router. Used by `DistributedRuntime` to inject a `DistributedRouter`.
- `node_id(&self) -> u64` -- Return this runtime's node ID.
- `table(&self) -> &Arc<ProcessTable>` -- Return a reference to the runtime's process table.
- `async spawn<F, Fut>(&self, handler: F) -> ProcessId` -- Spawn a new process. Returns the new process's PID immediately. The handler runs as an async task and receives a `ProcessContext`. Panics in the handler are caught and do not crash the runtime. The process is automatically removed from the table when the handler completes.
- `async send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError>` -- Send a message from outside any process context. Uses a synthetic PID of `<node_id, 0>` as the sender.

#### Spawn Signature

```rust
pub async fn spawn<F, Fut>(&self, handler: F) -> ProcessId
where
    F: FnOnce(ProcessContext) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
```

The handler is a closure that takes a `ProcessContext` and returns a `Future<Output = ()>`. Both the closure and the future must be `Send + 'static` so they can be moved onto a tokio task.

#### Example

```rust
use std::sync::Arc;
use rebar_core::runtime::Runtime;

let rt = Runtime::new(1);

// Spawn a process that waits for one message then exits
let pid = rt.spawn(|mut ctx| async move {
    let msg = ctx.recv().await.unwrap();
    println!("got: {:?}", msg.payload());
}).await;

// Send a message from outside
rt.send(pid, rmpv::Value::String("hello".into())).await.unwrap();
```

---

### `ProcessContext`

The execution context provided to each spawned process. Gives access to the process's own PID, its mailbox, and the ability to send messages to other processes.

#### Definition

```rust
pub struct ProcessContext {
    pid: ProcessId,
    rx: MailboxRx,
    router: Arc<dyn MessageRouter>,
}
```

#### Methods

- `self_pid(&self) -> ProcessId` -- Return this process's own PID.
- `async recv(&mut self) -> Option<Message>` -- Receive the next message from this process's mailbox. Returns `None` if the mailbox is closed.
- `async recv_timeout(&mut self, duration: Duration) -> Option<Message>` -- Receive a message with a timeout. Returns `None` if the timeout expires or the mailbox is closed.
- `async send(&self, dest: ProcessId, payload: rmpv::Value) -> Result<(), SendError>` -- Send a message to another process. Delegates to the runtime's `MessageRouter`, which may deliver locally or route to a remote node. The message's `from` field is set to this process's PID automatically.

#### Example

```rust
use rebar_core::runtime::Runtime;
use std::time::Duration;

let rt = Runtime::new(1);

let worker = rt.spawn(|mut ctx| async move {
    let me = ctx.self_pid();
    println!("I am {me}");

    // Send to self
    ctx.send(me, rmpv::Value::String("ping".into())).await.unwrap();

    // Receive with timeout
    match ctx.recv_timeout(Duration::from_secs(1)).await {
        Some(msg) => println!("got: {:?}", msg.payload()),
        None => println!("timed out"),
    }
}).await;
```

---

## Module: `supervisor`

OTP-style supervision trees. A supervisor monitors child processes and restarts them according to a configurable strategy when they exit.

---

### `RestartStrategy`

Determines how a supervisor responds when a child process exits abnormally.

#### Definition

```rust
#[derive(Debug, Clone, Copy)]
pub enum RestartStrategy {
    OneForOne,
    OneForAll,
    RestForOne,
}
```

#### Variants

| Variant | Behavior |
|---------|----------|
| `OneForOne` | Only the crashed child is restarted. Other children are unaffected. |
| `OneForAll` | All children are stopped (in reverse order) then all are restarted (in order). Use when children are interdependent. |
| `RestForOne` | Children started *after* the crashed child are stopped (in reverse order), then the crashed child and all subsequent children are restarted (in order). Use when later children depend on earlier ones. |

---

### `RestartType`

Controls whether a child should be restarted depending on its exit reason.

#### Definition

```rust
#[derive(Debug, Clone, Copy)]
pub enum RestartType {
    Permanent,
    Transient,
    Temporary,
}
```

#### Variants

| Variant | Behavior |
|---------|----------|
| `Permanent` | Always restarted, regardless of exit reason. |
| `Transient` | Restarted only if exit reason is not `Normal`. |
| `Temporary` | Never restarted. |

#### Methods

- `should_restart(&self, reason: &ExitReason) -> bool` -- Returns `true` if this restart type dictates a restart for the given exit reason.

#### Example

```rust
use rebar_core::supervisor::RestartType;
use rebar_core::process::ExitReason;

assert!(RestartType::Permanent.should_restart(&ExitReason::Normal));
assert!(RestartType::Permanent.should_restart(&ExitReason::Kill));

assert!(!RestartType::Transient.should_restart(&ExitReason::Normal));
assert!(RestartType::Transient.should_restart(&ExitReason::Abnormal("crash".into())));

assert!(!RestartType::Temporary.should_restart(&ExitReason::Normal));
assert!(!RestartType::Temporary.should_restart(&ExitReason::Kill));
```

---

### `ShutdownStrategy`

How the supervisor should stop a child during shutdown or restart.

#### Definition

```rust
#[derive(Debug, Clone)]
pub enum ShutdownStrategy {
    Timeout(Duration),
    BrutalKill,
}
```

#### Variants

| Variant | Behavior |
|---------|----------|
| `Timeout(Duration)` | Signal the child to shut down and wait up to `Duration`. If it has not exited by then, it is forcibly killed. |
| `BrutalKill` | Kill the child immediately without waiting. |

---

### `SupervisorSpec`

Builder for configuring a supervisor's strategy and children.

#### Definition

```rust
pub struct SupervisorSpec {
    pub strategy: RestartStrategy,
    pub max_restarts: u32,
    pub max_seconds: u32,
    pub children: Vec<ChildSpec>,
}
```

#### Methods (builder pattern)

- `new(strategy: RestartStrategy) -> Self` -- Create a new spec with the given restart strategy. Defaults: `max_restarts = 3`, `max_seconds = 5`, empty children list.
- `max_restarts(mut self, n: u32) -> Self` -- Set the maximum number of restarts allowed within the time window before the supervisor itself shuts down.
- `max_seconds(mut self, n: u32) -> Self` -- Set the time window (in seconds) for the restart counter.
- `child(mut self, spec: ChildSpec) -> Self` -- Append a child specification. Children are started in the order they are added.

#### Example

```rust
use rebar_core::supervisor::{SupervisorSpec, RestartStrategy, ChildSpec};

let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
    .max_restarts(5)
    .max_seconds(10)
    .child(ChildSpec::new("worker_1"))
    .child(ChildSpec::new("worker_2"));
```

---

### `ChildSpec`

Builder for configuring a single child process within a supervisor.

#### Definition

```rust
pub struct ChildSpec {
    pub id: String,
    pub restart: RestartType,
    pub shutdown: ShutdownStrategy,
}
```

#### Methods (builder pattern)

- `new(id: impl Into<String>) -> Self` -- Create a child spec with the given ID. Defaults: `restart = Permanent`, `shutdown = Timeout(5s)`.
- `restart(mut self, restart: RestartType) -> Self` -- Set the restart type for this child.
- `shutdown(mut self, strategy: ShutdownStrategy) -> Self` -- Set the shutdown strategy for this child.

#### Example

```rust
use rebar_core::supervisor::{ChildSpec, RestartType, ShutdownStrategy};
use std::time::Duration;

let child = ChildSpec::new("database_worker")
    .restart(RestartType::Transient)
    .shutdown(ShutdownStrategy::Timeout(Duration::from_secs(10)));
```

---

### `ChildFactory`

Type alias for the factory function that creates a child's async task. Must be callable multiple times (for restarts) and is shared via `Arc`.

#### Definition

```rust
pub type ChildFactory = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = ExitReason> + Send>> + Send + Sync,
>;
```

The factory returns a pinned, boxed, `Send` future that resolves to an `ExitReason`. It is `Send + Sync` so it can be shared across threads and invoked on each restart.

---

### `ChildEntry`

Pairs a `ChildSpec` with its `ChildFactory` for supervisor startup.

#### Definition

```rust
pub struct ChildEntry {
    pub spec: ChildSpec,
    pub factory: ChildFactory,
}
```

#### Methods

- `new<F, Fut>(spec: ChildSpec, factory: F) -> Self` -- Create a child entry. The factory closure is wrapped in an `Arc` and its return future is boxed and pinned internally.

#### Factory Signature

```rust
pub fn new<F, Fut>(spec: ChildSpec, factory: F) -> Self
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ExitReason> + Send + 'static,
```

#### Example

```rust
use rebar_core::supervisor::{ChildEntry, ChildSpec};
use rebar_core::process::ExitReason;

let entry = ChildEntry::new(
    ChildSpec::new("counter"),
    || async {
        // child logic here
        ExitReason::Normal
    },
);
```

---

### `SupervisorHandle`

Handle to a running supervisor, allowing external interaction. Implements `Clone` so multiple callers can control the same supervisor.

#### Definition

```rust
#[derive(Clone)]
pub struct SupervisorHandle {
    pid: ProcessId,
    msg_tx: mpsc::UnboundedSender<SupervisorMsg>, // private
}
```

#### Methods

- `pid(&self) -> ProcessId` -- Return the supervisor's own process ID.
- `async add_child(&self, entry: ChildEntry) -> Result<ProcessId, String>` -- Dynamically add a child to the running supervisor. Returns the new child's PID on success or an error string if the supervisor has already shut down.
- `shutdown(&self)` -- Request the supervisor to shut down. All children are stopped in reverse order.

#### Example

```rust
use std::sync::Arc;
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::*;
use rebar_core::process::ExitReason;

let rt = Arc::new(Runtime::new(1));

let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
    .max_restarts(3)
    .max_seconds(5);

let children = vec![
    ChildEntry::new(ChildSpec::new("worker"), || async {
        // do work ...
        ExitReason::Normal
    }),
];

let handle = start_supervisor(rt, spec, children).await;
println!("supervisor pid: {}", handle.pid());

// Dynamically add a child
let child_pid = handle.add_child(ChildEntry::new(
    ChildSpec::new("dynamic_worker"),
    || async { ExitReason::Normal },
)).await.unwrap();

// Shut down
handle.shutdown();
```

---

### `start_supervisor()`

Top-level function to start a supervisor as a process in the given runtime.

#### Signature

```rust
pub async fn start_supervisor(
    runtime: Arc<Runtime>,
    spec: SupervisorSpec,
    children: Vec<ChildEntry>,
) -> SupervisorHandle
```

#### Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `runtime` | `Arc<Runtime>` | Shared reference to the runtime. The supervisor is spawned as a process within this runtime. |
| `spec` | `SupervisorSpec` | Configuration for the supervisor's restart strategy and limits. |
| `children` | `Vec<ChildEntry>` | Initial children to start, in order. Each entry pairs a `ChildSpec` with a factory closure. |

#### Returns

A `SupervisorHandle` with the supervisor's PID and a channel for sending commands (add child, shutdown).

#### Behavior

1. Children are started in the order they appear in the `children` vector.
2. The supervisor enters an event loop, listening for child exits and commands.
3. When a child exits, the supervisor checks `RestartType::should_restart()` and applies the configured `RestartStrategy`.
4. If the number of restarts within `max_seconds` exceeds `max_restarts`, the supervisor shuts down all children and exits.

---

---

## Module: `timer`

Time-based message delivery. Equivalent to Erlang's `:timer` module.

---

### `TimerRef`

An opaque reference to a running timer. Can be used to cancel the timer before it fires.

#### Methods

- `cancel(&self)` -- Cancel the timer. Idempotent — safe to call multiple times.
- `is_finished(&self) -> bool` -- Returns true if the timer has fired or been cancelled.

---

### `send_after()`

Send a message to a process after a delay.

```rust
pub fn send_after(
    router: Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    delay: Duration,
) -> TimerRef
```

Spawns a tokio task that sleeps for `delay` then routes the message. The returned `TimerRef` can cancel the timer before it fires.

---

### `send_interval()`

Send a message repeatedly at a fixed interval.

```rust
pub fn send_interval(
    router: Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    interval: Duration,
) -> TimerRef
```

The first message is sent after one interval (not immediately). Automatically stops if the destination process is dead.

---

### `apply_after()` / `apply_interval()`

Execute a function after a delay, or repeatedly at an interval.

```rust
pub fn apply_after<F, Fut>(delay: Duration, f: F) -> TimerRef
pub fn apply_interval<F, Fut>(interval: Duration, f: F) -> TimerRef
```

---

### `ProcessContext` Timer Methods

Convenience methods available inside spawned processes:

- `send_after(&self, payload, delay) -> TimerRef` -- Send to self after delay
- `send_after_to(&self, dest, payload, delay) -> TimerRef` -- Send to another process after delay
- `send_interval(&self, payload, interval) -> TimerRef` -- Send to self repeatedly

---

### `GenServerContext` Timer Methods

Available inside GenServer callbacks:

- `send_after(&self, dest, payload, delay) -> Option<TimerRef>`
- `send_after_self(&self, payload, delay) -> Option<TimerRef>`
- `send_interval(&self, dest, payload, interval) -> Option<TimerRef>`

---

### `handle_continue`

Added to the `GenServer` trait. Called when `ctx.continue_with(payload)` is invoked from any callback.

```rust
async fn handle_continue(&self, msg: rmpv::Value, state: &mut Self::State, ctx: &GenServerContext) {}
```

Continue messages are processed before the next mailbox message, enabling deferred initialization.

---

## Module: `task`

Lightweight async work primitives. Equivalent to Elixir's `Task` module.

---

### `Task<T>`

A handle to a spawned async task with a typed result.

#### Methods

- `pid(&self) -> ProcessId` -- The task's process ID.
- `async await_result(&mut self, timeout: Duration) -> Result<T, TaskError>` -- Block until the task completes. Can only be called once.
- `async yield_result(&mut self, timeout: Duration) -> Option<Result<T, TaskError>>` -- Non-blocking check. Returns `None` if still running.
- `async shutdown(self) -> Option<T>` -- Kill the task, returning the result if already complete.

---

### `async_task()` / `async_task_ctx()`

Spawn a task and get a handle to await its result.

```rust
pub async fn async_task<F, Fut, T>(runtime: &Runtime, f: F) -> Task<T>
pub async fn async_task_ctx<F, Fut, T>(runtime: &Runtime, f: F) -> Task<T>  // with ProcessContext
```

---

### `start_task()`

Fire-and-forget task (no result tracking).

```rust
pub async fn start_task<F, Fut>(runtime: &Runtime, f: F) -> ProcessId
```

---

### `async_map()`

Process a collection concurrently with bounded parallelism.

```rust
pub async fn async_map<I, F, Fut, T>(
    runtime: Arc<Runtime>,
    items: I,
    f: F,
    opts: StreamOpts,
) -> Vec<Result<T, TaskError>>
```

Options: `max_concurrency`, `ordered` (preserve input order), `timeout` (per-task).

---

## Module: `agent`

Simple shared state without custom message types. Equivalent to Elixir's `Agent` module.

---

### `AgentRef`

A cloneable handle to a running agent. All operations are type-erased internally — you specify the state type `S` at each call site.

#### Methods

- `pid(&self) -> ProcessId`
- `async get<S, F, T>(&self, f: F, timeout) -> Result<T, AgentError>` -- Read from state via a function.
- `async update<S, F>(&self, f: F, timeout) -> Result<(), AgentError>` -- Mutate state via a function.
- `async get_and_update<S, F, T>(&self, f: F, timeout) -> Result<T, AgentError>` -- Read + mutate atomically.
- `cast<S, F>(&self, f: F) -> Result<(), AgentError>` -- Fire-and-forget mutation.
- `async stop(&self, timeout) -> Result<(), AgentError>` -- Shut down the agent.

---

### `start_agent()`

Start a new agent with initial state.

```rust
pub async fn start_agent<S, F>(runtime: Arc<Runtime>, init: F) -> AgentRef
where
    S: Send + 'static,
    F: FnOnce() -> S + Send + 'static,
```

#### Example

```rust
let agent = start_agent(rt, || HashMap::new()).await;
agent.update(|s: &mut HashMap<String, u64>| { s.insert("key".into(), 42); }, timeout).await?;
let val = agent.get(|s: &HashMap<String, u64>| s["key"], timeout).await?;
```

---

## Module: `pg`

Named process groups with pub/sub semantics. Equivalent to Erlang's `pg` module.

---

### `PgScope`

A process group scope. Groups are created lazily when processes join and removed when empty.

#### Methods

- `new() -> Arc<Self>` -- Create a new scope.
- `join(&self, group: &str, pid: ProcessId)` -- Add a process to a group. Can join multiple times.
- `leave(&self, group: &str, pid: ProcessId) -> Result<(), PgError>` -- Remove one membership.
- `get_members(&self, group: &str) -> Vec<ProcessId>` -- All members of a group.
- `get_local_members(&self, group: &str, node_id: u64) -> Vec<ProcessId>` -- Members on a specific node.
- `which_groups(&self) -> Vec<String>` -- All group names.
- `remove_pid(&self, pid: ProcessId)` -- Remove all memberships for a process (on death).
- `remove_node(&self, node_id: u64)` -- Remove all memberships for a node (on disconnect).
- `broadcast(&self, group, from, payload, router) -> Vec<Result<(), SendError>>` -- Send to all members.
- `member_count(&self, group: &str) -> usize`

#### Example

```rust
let scope = PgScope::new();
scope.join("workers", pid1);
scope.join("workers", pid2);

// Fan out
scope.broadcast("workers", from, &payload, &router);

// Cleanup on process death
scope.remove_pid(pid1);
```

---

## Module: `sys`

Runtime introspection for `GenServer` processes. Equivalent to Erlang's `:sys` module.

The sys debug interface adds methods directly to `GenServerRef<S>`:

### `GenServerRef` Sys Methods

- `async sys_get_state(&self, timeout) -> Result<S::State, CallError>` -- Read the server's current state. Requires `S::State: Clone`.
- `async sys_suspend(&self, timeout) -> Result<(), CallError>` -- Pause message processing. Only sys commands are handled while suspended.
- `async sys_resume(&self, timeout) -> Result<(), CallError>` -- Resume a suspended server.
- `async sys_get_status(&self, timeout) -> Result<ProcessStatus, CallError>` -- Get server status.

### Types

- `ProcessStatus` -- PID, run state, and debug options.
- `ProcessRunState` -- `Running` or `Suspended`.
- `DebugOpts` -- Trace, log, and statistics flags.
- `SystemEvent` -- Trace events (`In`, `Out`, `StateChange`, `Noreply`).
- `ProcessStatistics` -- `start_time`, `messages_in`, `messages_out`.

### Example

```rust
let server = spawn_gen_server(rt, MyServer).await;

// Inspect state at runtime
let state = server.sys_get_state(Duration::from_secs(1)).await.unwrap();

// Pause the server
server.sys_suspend(Duration::from_secs(1)).await.unwrap();
let status = server.sys_get_status(Duration::from_secs(1)).await.unwrap();
// status.status == ProcessRunState::Suspended

// Resume normal operation
server.sys_resume(Duration::from_secs(1)).await.unwrap();
```

---

## Module: `application`

Top-level application lifecycle with dependency ordering and configuration. Equivalent to Elixir's `Application` module.

---

### `Application` (trait)

The entry point for an OTP-style application. Implementations start a supervision tree.

```rust
#[async_trait]
pub trait Application: Send + Sync + 'static {
    async fn start(&self, runtime: Arc<Runtime>, env: &AppEnv) -> Result<SupervisorHandle, AppError>;
    async fn prep_stop(&self, _env: &AppEnv) {}  // called before supervisor shutdown
    async fn stop(&self, _env: &AppEnv) {}        // called after supervisor shutdown
}
```

---

### `AppEnv`

Concurrent configuration store backed by `DashMap`.

#### Methods

- `new() -> Self`
- `get(&self, key: &str) -> Option<rmpv::Value>`
- `get_or(&self, key: &str, default: rmpv::Value) -> rmpv::Value`
- `fetch(&self, key: &str) -> Result<rmpv::Value, AppError>` -- errors if key missing
- `put(&self, key: &str, value: rmpv::Value)`
- `delete(&self, key: &str)`
- `all(&self) -> Vec<(String, rmpv::Value)>`

---

### `ApplicationManager`

Manages registered applications with dependency-ordered startup/shutdown.

#### Methods

- `new(runtime: Arc<Runtime>) -> Self`
- `register<A: Application>(&self, spec: AppSpec, app: A)` -- register an app
- `async start(&self, name: &str) -> Result<(), AppError>` -- start one app
- `async ensure_all_started(&self, name: &str) -> Result<Vec<String>, AppError>` -- start with all transitive deps (topological sort)
- `async stop(&self, name: &str) -> Result<(), AppError>` -- lifecycle: prep_stop -> shutdown -> stop
- `async stop_all(&self) -> Result<(), AppError>` -- reverse start order
- `started_applications(&self) -> Vec<String>`
- `env(&self, name: &str) -> Option<AppEnv>`

---

### `AppSpec`

Builder for application specification.

```rust
AppSpec::new("my_app")
    .dependency("logger")
    .dependency("database")
    .env("port", rmpv::Value::from(8080))
```

---

## Module: `gen_statem`

Generic state machine behavior with enter callbacks, timeouts, and event postponement. Equivalent to Erlang's `gen_statem`.

---

### `GenStatem` (trait)

The state machine behavior. All events flow through a single `handle_event` callback.

```rust
#[async_trait]
pub trait GenStatem: Send + Sync + 'static {
    type State: Send + Clone + PartialEq + Debug + 'static;
    type Data: Send + 'static;
    type Call: Send + 'static;
    type Cast: Send + 'static;
    type Reply: Send + 'static;

    fn callback_mode(&self) -> (CallbackMode, bool);  // (mode, state_enter)
    async fn init(&self) -> Result<(Self::State, Self::Data), String>;
    async fn handle_event(
        &self, event_type: EventType<Self::Reply>, event: rmpv::Value,
        state: &Self::State, data: &mut Self::Data,
    ) -> TransitionResult<Self::State, Self::Data, Self::Reply>;
    async fn terminate(&self, reason: ExitReason, state: &Self::State, data: &mut Self::Data) {}
}
```

---

### `TransitionResult`

What happens after handling an event:

- `NextState { state, data, actions }` -- transition to a new state
- `KeepState { data, actions }` -- stay in current state, update data
- `KeepStateAndData { actions }` -- no changes, optionally with actions
- `Stop { reason, data }` -- shut down
- `StopAndReply { reason, data, replies }` -- shut down with pending replies

---

### `Action`

Actions returned from `handle_event`:

- `Reply(sender, value)` -- reply to a caller
- `StateTimeout(duration, payload)` -- cancelled on state change
- `EventTimeout(duration, payload)` -- cancelled on any event
- `GenericTimeout(name, duration, payload)` -- independent named timeout
- `CancelTimeout(kind)` -- cancel a timeout
- `Postpone` -- re-queue event, replay on next state change
- `NextEvent(payload)` -- inject an internal event
- `Hibernate` -- memory optimization hint

---

### `EventType`

- `Call(reply_sender)` -- synchronous request
- `Cast` -- async fire-and-forget
- `Info` -- raw mailbox message
- `StateTimeout` / `EventTimeout` / `Timeout(name)` -- timeout fired
- `Internal` -- from `NextEvent` action
- `Enter { old_state_name }` -- state enter callback

---

### `GenStatemRef`

Handle to a running state machine. Same API pattern as `GenServerRef`.

- `pid(&self) -> ProcessId`
- `async call(&self, msg, timeout) -> Result<Reply, CallError>`
- `cast(&self, msg) -> Result<(), SendError>`

---

### Example: Connection State Machine

```rust
#[derive(Clone, PartialEq, Debug)]
enum ConnState { Connecting, Connected, Disconnected }

// handle_event dispatches on (state, event_type):
// - Connecting + StateTimeout → retry or give up
// - Connected + Cast("disconnect") → transition to Disconnected
// - Enter on any state → log transition, set state timeout
```

---

## Module: `gen_stage`

Back-pressure-aware producer/consumer pipelines. Equivalent to Elixir's `GenStage`.

---

### `GenStage` (trait)

The stage behavior. Stages are producers, consumers, or producer-consumers.

```rust
#[async_trait]
pub trait GenStage: Send + Sync + 'static {
    type State: Send + 'static;
    async fn init(&self) -> Result<(StageType, Self::State), String>;
    async fn handle_demand(&self, demand: usize, state: &mut Self::State) -> Vec<rmpv::Value>;
    async fn handle_events(&self, events: Vec<rmpv::Value>, from: SubscriptionTag, state: &mut Self::State) -> Vec<rmpv::Value>;
    async fn handle_subscribe(&self, role: StageType, opts: &SubscribeOpts, from: SubscriptionTag, state: &mut Self::State) -> DemandMode;
    async fn handle_cancel(&self, reason: CancelReason, from: SubscriptionTag, state: &mut Self::State);
}
```

### Stage Types

- `Producer` — generates events in response to demand via `handle_demand`
- `Consumer` — processes events via `handle_events`
- `ProducerConsumer` — receives events, transforms, and emits downstream

### Demand Flow

1. Consumer subscribes to producer via `GenStageRef::subscribe()`
2. Consumer sends initial demand of `max_demand`
3. Producer calls `handle_demand()`, returns events
4. Events dispatched to consumers via the dispatcher
5. Consumer re-asks when processed count hits `min_demand`

### Dispatchers

- `DemandDispatcher` (default) — distribute events to consumers based on pending demand
- `BroadcastDispatcher` — send all events to all consumers

### `GenStageRef`

- `pid() -> ProcessId`
- `async subscribe(&self, producer, opts) -> Result<SubscriptionTag, StageError>`
- `async cancel(&self, tag) -> Result<(), StageError>`
- `async ask(&self, tag, demand) -> Result<(), StageError>` — for manual demand mode
- `async call(&self, msg, timeout) -> Result<rmpv::Value, StageError>`
- `cast(&self, msg) -> Result<(), StageError>`

### Example: ETL Pipeline

```rust
// Producer: generates numbers
let producer = spawn_stage(rt.clone(), NumberProducer).await;

// Consumer: processes numbers
let consumer = spawn_stage(rt.clone(), NumberConsumer).await;

// Wire them together
let tag = consumer.subscribe(&producer, SubscribeOpts::default()).await?;
// Events now flow automatically with back-pressure
```

---

## Module: `partition_supervisor`

Starts N identical children partitioned by key hash. Equivalent to Elixir's `PartitionSupervisor`.

---

### `PartitionSupervisorHandle`

Handle to a running partition supervisor with key-based routing.

#### Methods

- `pid(&self) -> ProcessId` -- supervisor PID
- `partitions(&self) -> usize` -- number of partitions
- `which_partition(&self, key: u64) -> ProcessId` -- route by integer (`key % N`)
- `which_partition_by_hash<K: Hash>(&self, key: &K) -> ProcessId` -- route by hash
- `partition_pid(&self, index: usize) -> Option<ProcessId>` -- get PID by index
- `shutdown(&self)` -- stop all partitions

---

### `start_partition_supervisor()`

```rust
pub async fn start_partition_supervisor(
    runtime: Arc<Runtime>,
    spec: PartitionSupervisorSpec,
    factory: PartitionFactory,
) -> PartitionSupervisorHandle
```

The factory receives the partition index (0..N):
```rust
let factory: PartitionFactory = Arc::new(|partition_idx| {
    Box::pin(async move {
        println!("Partition {partition_idx} running");
        loop { tokio::time::sleep(Duration::from_secs(3600)).await; }
        ExitReason::Normal
    })
});
```

#### Example

```rust
let spec = PartitionSupervisorSpec::new().partitions(4);
let handle = start_partition_supervisor(rt, spec, factory).await;

// Route work to a partition by key
let worker = handle.which_partition(user_id);
runtime.send(worker, payload).await?;
```

---

## See Also

- [Supervisor Engine Internals](../internals/supervisor-engine.md) -- deep dive into restart limiting, child lifecycle, and shutdown strategies
- [Architecture](../architecture.md) -- overview of the crate structure and process model
- [Getting Started](../getting-started.md) -- progressive examples using the APIs documented here
- [rebar-cluster API Reference](rebar-cluster.md) -- the distribution layer built on top of rebar-core
- [rebar-ffi API Reference](rebar-ffi.md) -- C-ABI bindings that wrap these APIs
