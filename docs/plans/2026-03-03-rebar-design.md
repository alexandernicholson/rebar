# Rebar Design Document

A BEAM-inspired distributed actor runtime for Rust with polyglot FFI support.

**Date:** 2026-03-03
**Status:** Approved

## Overview

Rebar is a general-purpose distributed actor runtime built in Rust. It provides BEAM-style lightweight processes, OTP-style supervision trees, transparent node clustering via SWIM gossip, and polyglot process hosting via C-ABI FFI. It can serve as both an embeddable actor runtime for Rust applications and a transparent service mesh replacement for microservice architectures.

### Design Decisions

| Decision | Choice |
|----------|--------|
| Use case | General-purpose actor runtime + internal service mesh |
| Discovery | SWIM gossip protocol |
| Serialization | MessagePack |
| Transport | Pluggable: QUIC (production) + TCP (development) |
| Supervision | OTP-style supervisor trees |
| Interop | Polyglot via C-ABI FFI (Go, Python, TypeScript) |
| Registry | Eventually consistent (OR-Set CRDT) |
| Architecture | Multi-crate workspace: rebar-core, rebar-cluster, rebar (facade), rebar-ffi |

### Crate Structure

```
crates/
  rebar-core/     # ProcessId, Message, Mailbox, ProcessTable, Supervisor, Runtime (local only)
                  # Zero networking dependencies
  rebar-cluster/  # SWIM, Transport (TCP + QUIC), Registry (CRDT), ConnectionManager, Wire Protocol
                  # Depends on rebar-core
  rebar/          # Facade crate: re-exports core + cluster, provides unified distributed Runtime
                  # Depends on rebar-core + rebar-cluster
  rebar-ffi/      # C-ABI shared library (librebar.so / .dylib / .dll)
                  # Depends on rebar
```

Users who only need local actors can depend on `rebar-core` without pulling in QUIC/TLS/networking. The full `rebar` crate wires everything together for distributed deployments.

---

## 1. Core Primitives

### ProcessId

```rust
struct ProcessId {
    node_id: u64,    // SWIM-assigned, unique per node
    local_id: u64,   // monotonically increasing per node
}
```

Globally unique, cheaply copyable, comparable. A PID is valid even after the process dies — sending to a dead PID is a no-op (or triggers a monitor notification).

### Mailbox

Each process owns a bounded `mpsc` channel. Configurable capacity per process (default unbounded, optional backpressure).

```rust
struct Message {
    from: ProcessId,
    payload: rmp_serde::Value,  // msgpack dynamic value
    timestamp: u64,
}
```

For Rust-to-Rust local messages, a typed fast path skips serialization entirely — only serialize when crossing node or language boundaries.

### Process Lifecycle

```rust
spawn(handler) -> ProcessId
send(pid, message) -> Result<(), SendError>
receive() -> Message              // blocking
receive_timeout(Duration) -> Option<Message>
self_pid() -> ProcessId
```

A process is a tokio task with:
- Its own mailbox (channel receiver)
- Process-local state (owned, not shared)
- A `trap_exit` flag (determines if exit signals become messages or kill the process)

### Process Table

Each node maintains a `DashMap<ProcessId, ProcessHandle>` where `ProcessHandle` holds the mailbox sender, `JoinHandle`, metadata (registered name, linked PIDs, monitors).

---

## 2. Supervisor Trees

### Restart Strategies

Three strategies, matching OTP:

- `OneForOne` — only restart the failed child
- `OneForAll` — if one child dies, restart all children
- `RestForOne` — restart the failed child and all children started after it

### Supervisor Spec

```rust
struct SupervisorSpec {
    strategy: RestartStrategy,
    max_restarts: u32,        // max restarts within...
    max_seconds: u32,         // ...this window, then supervisor itself fails
    children: Vec<ChildSpec>,
}

struct ChildSpec {
    id: String,
    start: Box<dyn ProcessFactory>,  // how to spawn the process
    restart: RestartType,            // permanent | transient | temporary
    shutdown: ShutdownStrategy,      // timeout(Duration) | brutal_kill
}
```

- `Permanent` — always restart, regardless of exit reason
- `Transient` — restart only on abnormal exit (panic/error)
- `Temporary` — never restart

### Shutdown Ordering

When a supervisor shuts down, it stops children in reverse start order. Each child gets a shutdown signal and a timeout to clean up. If the timeout expires, it is killed.

### Supervisor as a Process

A supervisor is itself a process with a PID. It can be supervised by a parent supervisor, forming the tree. The root supervisor is started by `rebar::Runtime::start()`.

```
Runtime
  └─ RootSupervisor
       ├─ ConnectionSupervisor (one_for_one)
       │    ├─ conn_1
       │    ├─ conn_2
       │    └─ conn_3
       ├─ WorkerPool (one_for_all)
       │    ├─ worker_1
       │    └─ worker_2
       └─ RegistrySupervisor (rest_for_one)
            ├─ registry
            └─ registry_sync
```

### Exit Signals

When a process dies, it sends exit signals to all linked processes. If the linked process has `trap_exit = true`, the signal becomes a message in its mailbox. If `trap_exit = false`, the linked process also dies (propagation). Supervisors always trap exits.

### Supervisor Loop

1. Start all children in order
2. `select!` on: child exit notifications, incoming messages (shutdown requests, new child specs)
3. On child exit: apply restart strategy, increment restart counter, check `max_restarts`/`max_seconds`
4. If restart limit exceeded: supervisor exits with error, propagating up the tree

---

## 3. Transport Layer

### Pluggable Transport Trait

All node-to-node communication goes through a `Transport` trait:

```rust
#[async_trait]
trait Transport: Send + Sync + 'static {
    type Connection: TransportConnection;
    async fn listen(&self, addr: SocketAddr) -> Result<Listener>;
    async fn connect(&self, addr: SocketAddr) -> Result<Self::Connection>;
}

#[async_trait]
trait TransportConnection: Send + Sync + 'static {
    async fn send(&self, frame: Frame) -> Result<()>;
    async fn recv(&self) -> Result<Frame>;
    async fn close(&self) -> Result<()>;
}
```

Two implementations ship with Rebar: `QuicTransport` and `TcpTransport`.

### QUIC Transport (production default)

Uses the `quinn` crate. Each node-to-node connection is a single QUIC connection with multiple bidirectional streams.

- No head-of-line blocking — each message stream is independent
- Built-in TLS 1.3 — encrypted by default
- Connection migration — handles IP changes gracefully
- 0-RTT reconnection for known peers

Stream allocation:
- One long-lived stream for control messages (heartbeats, monitors, links)
- Short-lived streams for individual message sends (opened per-message or batched)

### TCP Transport (development / compatibility)

Plain TCP with optional TLS via `rustls`. Single connection per node pair, length-prefixed framing.

```
TCP Frame:
┌──────────┬──────────────┐
│ len: u32 │ payload: [u8]│
└──────────┴──────────────┘
```

### Connection Manager

Manages the full mesh of node connections:

- Maintains `HashMap<NodeId, Connection>`
- `on_node_discovered(addr)` — connect if not connected
- `on_connection_lost(node_id)` — notify monitors, attempt reconnect
- `route(node_id, frame)` — send via existing connection
- Reconnect strategy: exponential backoff, max 30s

When a connection drops:
1. Generate `NodeDown` event
2. All monitors watching processes on that node get `ProcessDown` messages
3. All links to processes on that node trigger exit signals
4. Begin reconnection with exponential backoff
5. On reconnect, re-establish monitors/links (processes re-register)

### Encryption

- QUIC: always encrypted (TLS 1.3 built-in, no opt-out)
- TCP: optional — `TcpTransport::new()` for plain, `TcpTransport::with_tls(config)` for encrypted

Node identity verification via mutual TLS or pre-shared keys.

---

## 4. SWIM Discovery & Cluster Membership

### Protocol Overview

SWIM (Scalable Weakly-consistent Infection-style Membership) provides decentralized, gossip-based node discovery with built-in failure detection. No external dependencies.

```rust
enum NodeState {
    Alive,
    Suspect,    // might be down, not confirmed yet
    Dead,       // confirmed unreachable, will be removed
}

struct Member {
    node_id: u64,
    addr: SocketAddr,
    state: NodeState,
    incarnation: u64,  // logical clock, incremented to refute suspicion
}
```

### Failure Detection

Runs on a configurable tick interval (default: 1 second):

```
Every tick:
  1. Pick a random member K
  2. Send ping(K)
  3. If K responds with ack -> K is alive
  4. If no ack within timeout:
     a. Pick j random members (default j=3)
     b. Ask them to ping-req(K) on our behalf
     c. If any indirect ack comes back -> K is alive
     d. If no ack -> mark K as Suspect
  5. If K remains Suspect for suspect_timeout -> mark Dead
```

Properties:
- O(1) messages per tick per node (constant overhead regardless of cluster size)
- False positive resistance via indirect probing
- Tunable sensitivity via tick interval, timeout, and j parameter

### Suspicion Mechanism

A suspected node can refute by incrementing its incarnation number and broadcasting an `Alive` message:

```
Node A suspects Node B -> broadcasts Suspect(B, incarnation=5)
Node B receives it     -> broadcasts Alive(B, incarnation=6)
All nodes update B to Alive
```

If B is truly dead, no refutation comes and B transitions to Dead after the suspicion timeout.

### Dissemination (Piggybacking)

State changes (join, suspect, dead, alive) piggyback on ping/ack messages. Each ping/ack carries a bounded number of recent membership updates. Updates propagate in O(log n) rounds across n nodes.

### Join Protocol

```
1. New node sends Join(my_addr) to a seed node
2. Seed responds with full membership list
3. Seed broadcasts Alive(new_node) to the cluster
4. New node begins participating in the ping cycle
```

### Node Departure

- Graceful: node broadcasts `Leave` before shutting down — immediately marked Dead
- Ungraceful: detected via failure detection — Suspect — Dead

### Configuration

```rust
struct SwimConfig {
    bind_addr: SocketAddr,
    seeds: Vec<SocketAddr>,
    tick_interval: Duration,       // default: 1s
    ping_timeout: Duration,        // default: 500ms
    indirect_probes: usize,        // default: 3
    suspect_timeout: Duration,     // default: 5s
    dead_removal_delay: Duration,  // default: 30s
}
```

### Integration with Transport Layer

SWIM runs on its own UDP socket, independent of QUIC/TCP transport for process messages. When SWIM detects a new Alive node, the ConnectionManager establishes a QUIC/TCP connection. When SWIM marks a node Dead, ConnectionManager tears down the connection and fires NodeDown events.

```
SWIM (UDP)          ConnectionManager (QUIC/TCP)
  |                        |
  |-- Alive(node_3) ----->| connect(node_3)
  |-- Dead(node_3)  ----->| disconnect(node_3) + NodeDown events
  |-- tick/ping/ack        | send/recv process messages
```

---

## 5. Global Registry (CRDTs)

### Purpose

Allow processes to register under a name and be looked up by name across the cluster. `send("payment_processor", msg)` works regardless of which node the process lives on.

### Data Structure: OR-Set (Observed-Remove Set)

Each entry is a `(name, pid, unique_tag)` triple:

```rust
struct RegistryEntry {
    name: String,
    pid: ProcessId,
    tag: Uuid,           // unique per registration, makes removes unambiguous
    registered_at: u64,  // logical timestamp
}

struct Registry {
    entries: HashMap<String, Vec<RegistryEntry>>,  // name -> registrations
    tombstones: HashSet<Uuid>,                      // removed tags
}
```

Properties:
- Concurrent adds on different nodes don't conflict
- Removes are precise (only remove the specific registration)
- Convergence is guaranteed — all nodes eventually agree

### Name Conflicts

Resolution strategy:
- Last-writer-wins by default (highest logical timestamp wins)
- Losing process receives a `{registry_conflict, name}` message
- Application code can opt into multi-registration — multiple processes under one name, messages routed round-robin (process group)

### Operations

```rust
register(name: &str) -> Result<(), RegistryError>
unregister(name: &str)
whereis(name: &str) -> Option<ProcessId>
send_named(name: &str, msg: Message) -> Result<(), SendError>
registered() -> Vec<(String, ProcessId)>
```

### Dissemination

Registry updates piggyback on SWIM gossip messages. On registration/unregistration:

1. Local registry updated immediately
2. Delta (add or remove) queued for dissemination
3. Piggybacked on outgoing SWIM pings/acks
4. Receiving nodes merge the delta into their local registry

Full state sync on node join — the joining node receives the complete registry from its seed node.

### Process Death & Cleanup

When a process dies, its registry entries are automatically removed. The owning node generates remove deltas and disseminates them. If a node goes down, all other nodes locally remove entries belonging to that node's processes.

### Consistency Guarantees

- Reads are always local — no network round-trip
- Stale reads are possible (name resolves to dead process) — sender gets `SendError::ProcessDead` and can retry
- Propagation: O(log n) gossip rounds (typically under 5 seconds for 100-node cluster)
- No split-brain issues — OR-Set merges are commutative, associative, and idempotent

---

## 6. Wire Protocol

### Frame Format

All node-to-node communication uses a unified frame format:

```
Frame Layout:
+----------+----------+-----------+--------------+-------------+
| version  | msg_type | request_id| header_len   | payload_len |
|  1 byte  |  1 byte  |  8 bytes  |  4 bytes     |  4 bytes    |
+----------+----------+-----------+--------------+-------------+
| header (MessagePack)                                         |
+--------------------------------------------------------------+
| payload (MessagePack)                                        |
+--------------------------------------------------------------+
```

### Message Types

```
0x01  Send            dest_pid, from_pid -> payload is the message body
0x02  Monitor         watcher_pid, target_pid
0x03  Demonitor       watcher_pid, target_pid
0x04  Link            pid_a, pid_b
0x05  Unlink          pid_a, pid_b
0x06  Exit            from_pid, to_pid, reason
0x07  ProcessDown     ref, pid, reason
0x08  NameLookup      name -> response: pid or not_found
0x09  NameRegister    name, pid
0x0A  NameUnregister  name, pid
0x0B  Heartbeat       (empty)
0x0C  HeartbeatAck    (empty)
0x0D  NodeInfo        node_id, capabilities, version
```

The `request_id` field enables request/response pairing for messages that expect a reply (like `NameLookup`). Fire-and-forget messages (like `Send`) use `request_id = 0`.

### Versioning

The version byte allows protocol evolution. Nodes negotiate version on connection handshake — both send `NodeInfo`, agree on the lower version.

---

## 7. Polyglot FFI (C-ABI)

### Shared Library

The Rust runtime compiles to `librebar.so` / `librebar.dylib` / `rebar.dll` exposing a C API. Language SDKs (Go, Python, TypeScript) link against this.

### Core C API

```c
// Lifecycle
rebar_runtime_t* rebar_runtime_new(rebar_config_t* config);
void rebar_runtime_destroy(rebar_runtime_t* rt);

// Process management
rebar_pid_t rebar_spawn(
    rebar_runtime_t* rt,
    rebar_process_fn callback,    // function pointer: (ctx, msg) -> void
    void* user_data               // opaque pointer to language-side state
);
void rebar_send(rebar_runtime_t* rt, rebar_pid_t dest, rebar_msg_t* msg);
rebar_msg_t* rebar_receive(rebar_runtime_t* rt, int64_t timeout_ms);
rebar_pid_t rebar_self(rebar_runtime_t* rt);

// Messages (MessagePack bytes)
rebar_msg_t* rebar_msg_new(const uint8_t* data, size_t len);
const uint8_t* rebar_msg_data(rebar_msg_t* msg);
size_t rebar_msg_len(rebar_msg_t* msg);
void rebar_msg_free(rebar_msg_t* msg);

// Supervision
rebar_pid_t rebar_supervisor_start(
    rebar_runtime_t* rt,
    rebar_supervisor_spec_t* spec
);

// Registry
int rebar_register(rebar_runtime_t* rt, const char* name);
rebar_pid_t rebar_whereis(rebar_runtime_t* rt, const char* name);
int rebar_send_named(rebar_runtime_t* rt, const char* name, rebar_msg_t* msg);

// Monitors & Links
void rebar_monitor(rebar_runtime_t* rt, rebar_pid_t target);
void rebar_link(rebar_runtime_t* rt, rebar_pid_t target);
```

### FFI Process Execution Model

1. Language SDK calls `rebar_spawn` with a callback function pointer and opaque `user_data`
2. Rust runtime creates a tokio task that loops: receive message -> invoke callback via FFI
3. The callback runs language-side code (Go goroutine, Python function, Node.js callback)
4. Inside the callback, the language can call `rebar_send`, `rebar_receive`, etc. back into Rust
5. When the process exits (callback returns or panics), normal exit signal propagation occurs

### Threading Model Per Language

- **Go:** Callback runs on a Go goroutine via cgo. Go manages its own scheduler. The FFI boundary is a cgo call.
- **Python:** Callback runs on a Python thread. GIL applies within the callback, but other Rebar processes are unaffected since they are separate tokio tasks.
- **TypeScript/Node:** Callback invoked via napi-rs (N-API). Runs on the Node.js event loop thread. Async operations use Node's event loop as normal.

### Memory Ownership Rules

- `rebar_msg_t` created by `rebar_msg_new` — owned by caller, freed with `rebar_msg_free`
- `rebar_msg_t` received from `rebar_receive` — owned by caller, freed with `rebar_msg_free`
- Strings passed to `rebar_register` / `rebar_whereis` — borrowed, Rust copies internally
- `user_data` pointer — owned by language side, Rust never frees it
