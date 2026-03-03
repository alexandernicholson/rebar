# Documentation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create comprehensive documentation for Rebar covering architecture, developer guides, API reference, internals, extension guides, and benchmarks — all with Mermaid diagrams.

**Architecture:** 12 Markdown files in docs/ plus a README.md rewrite. Each file is self-contained. Diagrams use Mermaid with `<br>` for line breaks in labels (never `\n`). All code examples are real, tested API usage.

**Tech Stack:** Markdown, Mermaid diagrams, GitHub-rendered docs

**Important conventions:**
- Use `<br>` not `\n` for line breaks inside Mermaid node labels
- All Rust code examples must use real types from the actual API
- Working directory: `/home/alexandernicholson/.pxycrab/workspace/rebar`
- Commit after each task

---

### Task 1: README.md Rewrite

**Files:**
- Modify: `README.md`

**Step 1: Write README.md**

Write the full README with these sections:
1. Hero title + tagline
2. Feature highlights (bullet list)
3. Architecture Overview diagram (Mermaid — 4 crates + their relationships)
4. Quick Start code example (create Runtime, spawn process, send message, supervisor)
5. Benchmark Results table (from bench/results/report.md — throughput at c=100 + latency p50/p99)
6. Project Status section (what's implemented vs planned)
7. Documentation links (into docs/)
8. Contributing section
9. License

The architecture diagram should show:
- `rebar` (facade) depending on `rebar-core` and `rebar-cluster`
- `rebar-ffi` depending on `rebar-core`
- Key modules inside each crate

Quick start example:
```rust
use rebar_core::runtime::Runtime;
use rebar_core::process::ProcessId;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let runtime = Arc::new(Runtime::new(1));

    // Spawn a process
    let pid = runtime.spawn(|mut ctx| async move {
        println!("Hello from process {:?}", ctx.self_pid());
        while let Some(msg) = ctx.recv().await {
            println!("Got: {:?}", msg.payload());
        }
    }).await;

    // Send a message
    runtime.send(pid, rmpv::Value::from("hello")).await.unwrap();
}
```

Supervisor example:
```rust
use rebar_core::supervisor::*;
use rebar_core::runtime::Runtime;
use std::sync::Arc;

let runtime = Arc::new(Runtime::new(1));
let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
    .max_restarts(5)
    .max_seconds(60);
let handle = start_supervisor(runtime.clone(), spec, vec![]).await;
```

**Step 2: Commit**
```bash
git add README.md
git commit -m "docs: rewrite README with architecture diagram, quick start, benchmarks"
```

---

### Task 2: Architecture Document

**Files:**
- Create: `docs/architecture.md`

**Step 1: Write docs/architecture.md**

This is the centerpiece. Sections:

**1. System Overview** — Paragraph explaining BEAM inspiration, crate layering.

**2. Crate Dependency Graph** — Mermaid graph showing:
```
rebar (facade) --> rebar-core
rebar (facade) --> rebar-cluster
rebar-ffi --> rebar-core
rebar-cluster --> rebar-core (for ProcessId, Message types)
```

**3. Process Model** — Diagram of process lifecycle:
```
Runtime::spawn() → allocate PID → create Mailbox → start tokio task → process runs handler → exit/panic → remove from ProcessTable
```
Explain: ProcessId = (node_id, local_id), mailbox-based messaging, panic isolation via catch_unwind.

**4. Message Flow** — Two diagrams:
- Local: Sender → ProcessTable lookup → MailboxTx::send → MailboxRx::recv
- Remote: Sender → Frame encode → Transport::send → TCP → Transport::recv → Frame decode → local delivery

**5. Supervisor Trees** — Visual diagram of all 3 restart strategies:
- OneForOne: only failed child restarts
- OneForAll: all children restart
- RestForOne: failed + subsequent children restart
Include restart limiting (VecDeque<Instant> sliding window).

**6. SWIM Protocol** — State machine diagram:
```
Alive → (no ACK) → Suspect → (timeout) → Dead → (removal delay) → Removed
Suspect → (ACK received) → Alive
```
Explain: protocol period, indirect probes, incarnation numbers.

**7. Wire Protocol** — Byte-level frame layout diagram:
```
| version(1) | msg_type(1) | request_id(8) | header_len(4) | payload_len(4) | header(N) | payload(M) |
```
List all 13 MsgType variants.

**8. Global Registry** — OR-Set CRDT diagram:
- Register: create RegistryEntry with unique UUID tag
- Lookup: LWW by timestamp, tiebreak by node_id
- Merge: add entries, apply tombstones
- Convergence: all nodes eventually agree

**9. Connection Management** — Diagram showing:
- Node discovery → connect → connected
- Connection lost → exponential backoff → reconnect attempts
- Backoff formula: min(base * 2^attempt, max)

**10. FFI Layer** — Diagram showing:
- Language (Go/Python/TS) → C-ABI → RebarRuntime → Rust Runtime
- Memory ownership boundaries

**Step 2: Commit**
```bash
git add docs/architecture.md
git commit -m "docs: add comprehensive architecture document with diagrams"
```

---

### Task 3: Getting Started Guide

**Files:**
- Create: `docs/getting-started.md`

**Step 1: Write docs/getting-started.md**

Progressive tutorial sections:

**1. Installation** — Add to Cargo.toml:
```toml
[dependencies]
rebar-core = { git = "https://github.com/alexandernicholson/rebar" }
tokio = { version = "1", features = ["full"] }
rmpv = "1"
```

**2. Your First Process** — Create Runtime, spawn, observe output.

**3. Sending Messages** — Send rmpv::Value between processes, recv pattern.

**4. Request-Reply Pattern** — Using tokio::sync::oneshot for RPC-style.

**5. Supervisor Trees** — Create SupervisorSpec, ChildEntry, start_supervisor. Show crash → automatic restart.

**6. Process-per-Key Pattern** — Like the benchmark store service: HashMap<String, mpsc::Sender> mapping keys to long-lived processes.

**7. Monitoring & Linking** — MonitorRef, MonitorSet, LinkSet usage.

Each section has a complete, runnable code example.

**Step 2: Commit**
```bash
git add docs/getting-started.md
git commit -m "docs: add getting started guide with progressive examples"
```

---

### Task 4: rebar-core API Reference

**Files:**
- Create: `docs/api/rebar-core.md`

**Step 1: Write docs/api/rebar-core.md**

Document every public type in rebar-core, grouped by module:

**process module:**
- ProcessId — fields, methods, Display impl
- Message — fields, constructor, accessors
- ExitReason — variants, is_normal()
- SendError — variants
- MailboxTx — send(), try_send()
- MailboxRx — recv(), recv_timeout()
- Mailbox — unbounded(), bounded()
- ProcessHandle — new(), send()
- ProcessTable — new(), allocate_pid(), insert(), get(), remove(), send(), len(), is_empty()
- MonitorRef — new()
- MonitorSet — new(), add_monitor(), remove_monitor(), monitors_for()
- LinkSet — new(), add_link(), remove_link(), is_linked(), linked_pids()

**runtime module:**
- Runtime — new(), node_id(), spawn(), send()
- ProcessContext — self_pid(), recv(), recv_timeout(), send()

**supervisor module:**
- RestartStrategy — OneForOne, OneForAll, RestForOne
- RestartType — Permanent, Transient, Temporary + should_restart()
- ShutdownStrategy — Timeout, BrutalKill
- SupervisorSpec — new(), max_restarts(), max_seconds(), child()
- ChildSpec — new(), restart(), shutdown()
- ChildFactory type alias
- ChildEntry — new()
- SupervisorHandle — pid(), add_child(), shutdown()
- start_supervisor() — signature and usage

Each type gets: definition, description, example code, related types.

**Step 2: Commit**
```bash
mkdir -p docs/api
git add docs/api/rebar-core.md
git commit -m "docs: add rebar-core API reference"
```

---

### Task 5: rebar-cluster API Reference

**Files:**
- Create: `docs/api/rebar-cluster.md`

**Step 1: Write docs/api/rebar-cluster.md**

Document every public type grouped by module:

**protocol module:**
- Frame — fields, encode(), decode()
- MsgType — all 13 variants with descriptions
- FrameError — variants

**transport module:**
- TransportConnection trait — send(), recv(), close()
- TransportListener trait — accept(), local_addr()
- TransportError — variants
- TcpConnection — implementation details
- TcpListener — implementation details

**swim module:**
- Member — fields, new(), suspect(), alive(), dead()
- NodeState — Alive, Suspect, Dead
- MembershipList — full API
- SwimConfig — defaults, builder pattern
- SwimConfigBuilder — all methods
- FailureDetector — full API with tick(), record_ack/nack, timeouts
- GossipUpdate — variants (Alive, Suspect, Dead, Leave)
- GossipQueue — new(), add(), drain()

**registry module:**
- Registry — full API (register, lookup, unregister, merge_delta, generate_deltas, remove_by_pid/node)
- RegistryEntry — fields
- RegistryDelta — Add, Remove variants

**connection module:**
- ConnectionManager — full API
- ConnectionEvent — NodeDown, ReconnectTriggered
- ReconnectPolicy — fields, backoff_delay()
- TransportConnector trait
- ConnectionError — variants

**Step 2: Commit**
```bash
git add docs/api/rebar-cluster.md
git commit -m "docs: add rebar-cluster API reference"
```

---

### Task 6: rebar-ffi API Reference

**Files:**
- Create: `docs/api/rebar-ffi.md`

**Step 1: Write docs/api/rebar-ffi.md**

C-ABI reference organized for polyglot developers:

**Types:**
- RebarPid (repr(C)) — node_id: u64, local_id: u64
- RebarMsg — opaque, created/freed via API
- RebarRuntime — opaque, created/freed via API

**Error Codes:**
- REBAR_OK (0)
- REBAR_ERR_NULL_PTR (-1)
- REBAR_ERR_SEND_FAILED (-2)
- REBAR_ERR_NOT_FOUND (-3)
- REBAR_ERR_INVALID_NAME (-4)

**Functions** (each with signature, description, example C code):
- rebar_msg_create, rebar_msg_data, rebar_msg_len, rebar_msg_free
- rebar_runtime_new, rebar_runtime_free
- rebar_spawn, rebar_send, rebar_send_named
- rebar_register, rebar_whereis

**Memory Ownership Rules:**
- Who allocates, who frees
- String borrowing semantics
- Thread safety guarantees

**Language Integration Examples:**
- C header file
- Go via cgo
- Python via ctypes

**Step 2: Commit**
```bash
git add docs/api/rebar-ffi.md
git commit -m "docs: add rebar-ffi C-ABI reference with language examples"
```

---

### Task 7: Wire Protocol Internals

**Files:**
- Create: `docs/internals/wire-protocol.md`

**Step 1: Write docs/internals/wire-protocol.md**

Deep dive:
- Frame layout diagram (byte-level with offsets)
- Version field semantics
- MsgType registry (all 13 types with request/response patterns)
- MessagePack encoding for header and payload
- Frame::encode() and Frame::decode() walkthrough
- Error handling (FrameError variants)
- Example: encoding a Send message, decoding on the other end

**Step 2: Commit**
```bash
mkdir -p docs/internals
git add docs/internals/wire-protocol.md
git commit -m "docs: add wire protocol internals documentation"
```

---

### Task 8: SWIM Protocol Internals

**Files:**
- Create: `docs/internals/swim-protocol.md`

**Step 1: Write docs/internals/swim-protocol.md**

Deep dive:
- SWIM overview (Scalable Weakly-consistent Infection-style Membership)
- Protocol period tick loop
- Direct probe → ACK/NACK
- Indirect probe (k random members)
- State transitions: Alive → Suspect → Dead → Removed
- Incarnation numbers for refuting false suspicion
- Gossip piggybacking (GossipUpdate variants)
- Configuration knobs (SwimConfig defaults and tuning guide)
- Failure detector algorithm (HashMap<u64, Instant> timers)
- Convergence guarantees

**Step 2: Commit**
```bash
git add docs/internals/swim-protocol.md
git commit -m "docs: add SWIM protocol internals documentation"
```

---

### Task 9: CRDT Registry Internals

**Files:**
- Create: `docs/internals/crdt-registry.md`

**Step 1: Write docs/internals/crdt-registry.md**

Deep dive:
- OR-Set CRDT theory (Observed-Remove Set)
- Why OR-Set for distributed process registry
- Registration: RegistryEntry with UUID tag + timestamp
- Lookup: LWW conflict resolution (highest timestamp, tiebreak by node_id)
- Unregistration: tombstone via UUID tag
- Delta merging: merge_delta() idempotency guarantees
- Full state sync: generate_deltas()
- Node/PID cleanup: remove_by_node(), remove_by_pid()
- Convergence proof (informal): all nodes apply same deltas → same state
- Example: two nodes register same name concurrently → LWW resolves

**Step 2: Commit**
```bash
git add docs/internals/crdt-registry.md
git commit -m "docs: add CRDT registry internals documentation"
```

---

### Task 10: Supervisor Engine Internals

**Files:**
- Create: `docs/internals/supervisor-engine.md`

**Step 1: Write docs/internals/supervisor-engine.md**

Deep dive:
- OTP supervisor model overview
- Supervisor as a process (spawned via Runtime::spawn)
- Message loop: SupervisorMsg enum (ChildExited, AddChild, Shutdown)
- ChildState lifecycle: factory → spawn → running → exit → evaluate restart
- Restart strategies with diagrams:
  - OneForOne: restart(failed_index)
  - OneForAll: restart(0..all)
  - RestForOne: restart(failed_index..all)
- Restart limiting: VecDeque<Instant> sliding window algorithm
  - Record restart timestamp
  - Evict timestamps older than max_seconds
  - If count > max_restarts → supervisor shuts down
- Shutdown strategies:
  - Timeout: send shutdown signal, wait Duration, then force kill
  - BrutalKill: immediate cancellation
- Dynamic children: add_child() at runtime
- Nested supervisors: supervisor as a child of another supervisor
- PID allocation: static AtomicU64 counter starting at 1,000,000

**Step 2: Commit**
```bash
git add docs/internals/supervisor-engine.md
git commit -m "docs: add supervisor engine internals documentation"
```

---

### Task 11: Extension Guide

**Files:**
- Create: `docs/extending.md`

**Step 1: Write docs/extending.md**

For developers building on Rebar:

**1. Custom Transport Implementation**
- Implement TransportConnection trait (send, recv, close)
- Implement TransportListener trait (accept, local_addr)
- Example: QUIC transport skeleton
- Testing with MockTransportConnector pattern (from connection manager tests)

**2. Custom Supervisor Strategies**
- Understanding the ChildFactory pattern
- Creating custom ChildEntry factories
- Dynamic child addition via SupervisorHandle::add_child()
- Monitoring supervisor health

**3. FFI Bindings for New Languages**
- Understanding the C-ABI layer
- Writing a language wrapper (step by step)
- Memory ownership rules
- Example: Ruby FFI wrapper skeleton

**4. Custom Message Serialization**
- Current: rmpv::Value (MessagePack dynamic)
- Adding typed messages via serde
- Frame header/payload customization

**5. Custom Registry Backends**
- Understanding the OR-Set CRDT interface
- Implementing custom conflict resolution
- State sync strategies

**Step 2: Commit**
```bash
git add docs/extending.md
git commit -m "docs: add extension guide for custom transports, supervisors, FFI"
```

---

### Task 12: Benchmarks Document

**Files:**
- Create: `docs/benchmarks.md`

**Step 1: Write docs/benchmarks.md**

**1. Methodology**
- HTTP microservices mesh architecture diagram
- 3 services: Gateway (8080), Compute (8081), Store (8082)
- Docker Compose with resource limits (2 CPU, 512MB per container)
- oha load generator

**2. Test Application**
- What each service does
- Compute: fibonacci(n) via Rebar process spawn
- Store: process-per-key pattern
- Gateway: HTTP proxy

**3. Competitor Stacks**
- Rebar (axum + rebar-core)
- Actix (actix-web)
- Go (net/http stdlib)
- Elixir (Plug + Cowboy)

**4. Scenarios**
- Throughput ramp (c=1..1000)
- Latency profile (c=100, 30s)
- Process spawn stress (10k keys)
- Cross-node messaging (c=100, 30s)

**5. Results** — Full tables from bench/results/report.md

**6. Analysis**
- Rebar vs Actix: near-identical (both Rust + tokio), Rebar wins on latency
- Rebar vs Go: ~10-20x throughput advantage at high concurrency
- Rebar vs Elixir: ~3x throughput, lower latency
- What the benchmark does and doesn't measure

**7. Reproducing** — Step-by-step instructions to run benchmarks

**Step 2: Commit**
```bash
git add docs/benchmarks.md
git commit -m "docs: add benchmark methodology, results, and analysis"
```

---

### Task 13: Final Review and Cross-Links

**Files:**
- Modify: All docs files for cross-references

**Step 1: Review all docs**
- Verify all internal links work (relative paths between docs)
- Ensure README links to all docs
- Check Mermaid diagrams use `<br>` not `\n`
- Verify code examples match actual API

**Step 2: Commit**
```bash
git add -A docs/ README.md
git commit -m "docs: add cross-references and final review pass"
```
