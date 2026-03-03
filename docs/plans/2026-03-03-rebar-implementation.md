# Rebar Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a BEAM-inspired distributed actor runtime in Rust with OTP-style supervision, SWIM gossip clustering, CRDT-based global registry, pluggable transports (QUIC + TCP), and polyglot C-ABI FFI.

**Architecture:** Cargo workspace with 4 crates: `rebar-core` (local actors, zero networking), `rebar-cluster` (SWIM, transport, registry, connection manager), `rebar` (facade re-exporting both), and `rebar-ffi` (C-ABI shared library). Each layer builds on the previous — processes first, then supervision, then networking, then distribution.

**Tech Stack:** Rust (2024 edition), tokio (async runtime), dashmap (concurrent process table), rmp-serde/rmpv (MessagePack), quinn (QUIC), rustls (TLS), uuid (registry tags), rand (SWIM), cargo-nextest (test runner)

**Design Doc:** `docs/plans/2026-03-03-rebar-design.md`

**Test Target:** ~245 tests across all components

---

## Phase 1: Project Scaffold

### Task 1: Workspace and Crate Setup

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/rebar-core/Cargo.toml`
- Create: `crates/rebar-core/src/lib.rs`
- Create: `crates/rebar-cluster/Cargo.toml`
- Create: `crates/rebar-cluster/src/lib.rs`
- Create: `crates/rebar/Cargo.toml`
- Create: `crates/rebar/src/lib.rs`
- Create: `crates/rebar-ffi/Cargo.toml`
- Create: `crates/rebar-ffi/src/lib.rs`

**Step 1: Create workspace Cargo.toml**

```toml
[workspace]
resolver = "2"
members = [
    "crates/rebar-core",
    "crates/rebar-cluster",
    "crates/rebar",
    "crates/rebar-ffi",
]

[workspace.package]
edition = "2024"
license = "MIT"
repository = "https://github.com/alexandernicholson/rebar"
```

**Step 2: Create rebar-core crate**

`crates/rebar-core/Cargo.toml`:
```toml
[package]
name = "rebar-core"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Core actor primitives and supervision for the Rebar runtime"

[dependencies]
tokio = { version = "1", features = ["full"] }
dashmap = "6"
rmpv = "1"
serde = { version = "1", features = ["derive"] }
thiserror = "2"
tracing = "0.1"

[dev-dependencies]
tokio-test = "0.4"
```

`crates/rebar-core/src/lib.rs`:
```rust
pub mod process;
```

Create `crates/rebar-core/src/process/mod.rs`:
```rust
mod types;
pub use types::*;
```

Create `crates/rebar-core/src/process/types.rs` as an empty file.

**Step 3: Create rebar-cluster crate**

`crates/rebar-cluster/Cargo.toml`:
```toml
[package]
name = "rebar-cluster"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Distributed clustering, transport, and registry for the Rebar runtime"

[dependencies]
rebar-core = { path = "../rebar-core" }
tokio = { version = "1", features = ["full"] }
rmpv = "1"
rmp-serde = "1"
serde = { version = "1", features = ["derive"] }
async-trait = "0.1"
thiserror = "2"
tracing = "0.1"
bytes = "1"
uuid = { version = "1", features = ["v4"] }
rand = "0.9"
quinn = "0.11"
rustls = "0.23"

[dev-dependencies]
tokio-test = "0.4"
```

`crates/rebar-cluster/src/lib.rs`:
```rust
pub mod protocol;
```

**Step 4: Create rebar facade crate**

`crates/rebar/Cargo.toml`:
```toml
[package]
name = "rebar"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "A BEAM-inspired distributed actor runtime"

[dependencies]
rebar-core = { path = "../rebar-core" }
rebar-cluster = { path = "../rebar-cluster" }
```

`crates/rebar/src/lib.rs`:
```rust
pub use rebar_core::*;
pub use rebar_cluster::*;
```

**Step 5: Create rebar-ffi crate (stub)**

`crates/rebar-ffi/Cargo.toml`:
```toml
[package]
name = "rebar-ffi"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "C-ABI FFI bindings for the Rebar actor runtime"

[lib]
crate-type = ["cdylib", "lib"]

[dependencies]
rebar = { path = "../rebar" }
rebar-core = { path = "../rebar-core" }
tokio = { version = "1", features = ["full"] }
```

`crates/rebar-ffi/src/lib.rs`:
```rust
// FFI bindings will be implemented after core runtime is complete.
```

**Step 6: Verify workspace builds**

Run: `cargo build`
Expected: Compiles with no errors

**Step 7: Commit**

```bash
git add Cargo.toml crates/
git commit -m "scaffold: cargo workspace with rebar-core, rebar-cluster, rebar, rebar-ffi"
```

---

## Phase 2: Core Primitives (rebar-core)

### Task 2: ProcessId Type (~8 tests)

**Files:**
- Create: `crates/rebar-core/src/process/types.rs`
- Test: inline tests

**Step 1: Write failing tests for ProcessId**

In `crates/rebar-core/src/process/types.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn process_id_equality() {
        let a = ProcessId::new(1, 42);
        let b = ProcessId::new(1, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn process_id_inequality_different_node() {
        assert_ne!(ProcessId::new(1, 42), ProcessId::new(2, 42));
    }

    #[test]
    fn process_id_inequality_different_local() {
        assert_ne!(ProcessId::new(1, 42), ProcessId::new(1, 43));
    }

    #[test]
    fn process_id_is_copy() {
        let a = ProcessId::new(1, 42);
        let b = a;
        assert_eq!(a, b); // a still valid after copy
    }

    #[test]
    fn process_id_display() {
        assert_eq!(format!("{}", ProcessId::new(1, 42)), "<1.42>");
    }

    #[test]
    fn process_id_hash_dedup() {
        let mut set = HashSet::new();
        set.insert(ProcessId::new(1, 1));
        set.insert(ProcessId::new(1, 1));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn process_id_accessors() {
        let pid = ProcessId::new(5, 99);
        assert_eq!(pid.node_id(), 5);
        assert_eq!(pid.local_id(), 99);
    }

    #[test]
    fn process_id_debug() {
        let pid = ProcessId::new(1, 1);
        let debug = format!("{:?}", pid);
        assert!(debug.contains("ProcessId"));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p rebar-core`
Expected: FAIL — `ProcessId` not defined

**Step 3: Implement ProcessId**

```rust
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId {
    node_id: u64,
    local_id: u64,
}

impl ProcessId {
    pub fn new(node_id: u64, local_id: u64) -> Self {
        Self { node_id, local_id }
    }

    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub fn local_id(&self) -> u64 {
        self.local_id
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}.{}>", self.node_id, self.local_id)
    }
}
```

**Step 4: Run tests, verify pass**

Run: `cargo test -p rebar-core`
Expected: 8 PASS

**Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): add ProcessId type with copy, hash, display"
```

---

### Task 3: Message and ExitReason Types (~10 tests)

**Files:**
- Modify: `crates/rebar-core/src/process/types.rs`

**Step 1: Write failing tests**

Add to tests module:
```rust
    #[test]
    fn message_creation() {
        let from = ProcessId::new(1, 1);
        let payload = rmpv::Value::String("hello".into());
        let msg = Message::new(from, payload.clone());
        assert_eq!(msg.from(), from);
        assert_eq!(*msg.payload(), payload);
        assert!(msg.timestamp() > 0);
    }

    #[test]
    fn message_with_map_payload() {
        let from = ProcessId::new(1, 1);
        let payload = rmpv::Value::Map(vec![
            (rmpv::Value::String("key".into()), rmpv::Value::Integer(42.into())),
        ]);
        let msg = Message::new(from, payload.clone());
        assert_eq!(*msg.payload(), payload);
    }

    #[test]
    fn message_with_nil_payload() {
        let msg = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        assert_eq!(*msg.payload(), rmpv::Value::Nil);
    }

    #[test]
    fn message_with_binary_payload() {
        let data = rmpv::Value::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let msg = Message::new(ProcessId::new(1, 1), data.clone());
        assert_eq!(*msg.payload(), data);
    }

    #[test]
    fn message_with_nested_array() {
        let payload = rmpv::Value::Array(vec![
            rmpv::Value::Integer(1.into()),
            rmpv::Value::Array(vec![rmpv::Value::Integer(2.into())]),
        ]);
        let msg = Message::new(ProcessId::new(1, 1), payload.clone());
        assert_eq!(*msg.payload(), payload);
    }

    #[test]
    fn exit_reason_normal_is_normal() {
        assert!(ExitReason::Normal.is_normal());
    }

    #[test]
    fn exit_reason_abnormal_is_not_normal() {
        assert!(!ExitReason::Abnormal("panicked".into()).is_normal());
    }

    #[test]
    fn exit_reason_kill_is_not_normal() {
        assert!(!ExitReason::Kill.is_normal());
    }

    #[test]
    fn exit_reason_linked_exit() {
        let reason = ExitReason::LinkedExit(
            ProcessId::new(1, 5),
            Box::new(ExitReason::Abnormal("crash".into())),
        );
        assert!(!reason.is_normal());
    }

    #[test]
    fn send_error_display() {
        let err = SendError::ProcessDead(ProcessId::new(1, 5));
        let msg = format!("{}", err);
        assert!(msg.contains("1") && msg.contains("5"));
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p rebar-core`
Expected: FAIL — `Message`, `ExitReason`, `SendError` not defined

**Step 3: Implement Message, ExitReason, SendError**

```rust
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct Message {
    from: ProcessId,
    payload: rmpv::Value,
    timestamp: u64,
}

impl Message {
    pub fn new(from: ProcessId, payload: rmpv::Value) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Self { from, payload, timestamp }
    }

    pub fn from(&self) -> ProcessId { self.from }
    pub fn payload(&self) -> &rmpv::Value { &self.payload }
    pub fn timestamp(&self) -> u64 { self.timestamp }
}

#[derive(Debug, Clone)]
pub enum ExitReason {
    Normal,
    Abnormal(String),
    Kill,
    LinkedExit(ProcessId, Box<ExitReason>),
}

impl ExitReason {
    pub fn is_normal(&self) -> bool {
        matches!(self, ExitReason::Normal)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("process dead: {0}")]
    ProcessDead(ProcessId),
    #[error("mailbox full for: {0}")]
    MailboxFull(ProcessId),
}
```

**Step 4: Run tests, verify pass**

Run: `cargo test -p rebar-core`
Expected: 18 PASS

**Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): add Message, ExitReason, SendError types"
```

---

### Task 4: Mailbox (~12 tests)

**Files:**
- Create: `crates/rebar-core/src/process/mailbox.rs`
- Modify: `crates/rebar-core/src/process/mod.rs`

**Step 1: Write failing tests**

Create `crates/rebar-core/src/process/mailbox.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessId;

    #[tokio::test]
    async fn unbounded_send_receive() {
        let (tx, mut rx) = Mailbox::unbounded();
        let msg = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        tx.send(msg).unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(received.from(), ProcessId::new(1, 1));
    }

    #[tokio::test]
    async fn bounded_capacity_respected() {
        let (tx, _rx) = Mailbox::bounded(1);
        let msg1 = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        let msg2 = Message::new(ProcessId::new(1, 2), rmpv::Value::Nil);
        tx.send(msg1).unwrap();
        assert!(tx.try_send(msg2).is_err());
    }

    #[tokio::test]
    async fn recv_timeout_expires() {
        let (_tx, mut rx) = Mailbox::unbounded();
        let result = rx.recv_timeout(std::time::Duration::from_millis(10)).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn recv_timeout_receives_in_time() {
        let (tx, mut rx) = Mailbox::unbounded();
        let msg = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        tx.send(msg).unwrap();
        let result = rx.recv_timeout(std::time::Duration::from_millis(100)).await;
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn multiple_messages_fifo() {
        let (tx, mut rx) = Mailbox::unbounded();
        for i in 0..5u64 {
            tx.send(Message::new(ProcessId::new(1, i), rmpv::Value::Integer(i.into()))).unwrap();
        }
        for i in 0..5u64 {
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.from().local_id(), i);
        }
    }

    #[tokio::test]
    async fn dropped_sender_closes_receiver() {
        let (tx, mut rx) = Mailbox::unbounded();
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn bounded_send_after_drain() {
        let (tx, mut rx) = Mailbox::bounded(1);
        let msg1 = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        tx.send(msg1).unwrap();
        rx.recv().await.unwrap(); // drain
        let msg2 = Message::new(ProcessId::new(1, 2), rmpv::Value::Nil);
        assert!(tx.send(msg2).is_ok());
    }

    #[tokio::test]
    async fn send_to_closed_receiver_returns_error() {
        let (tx, rx) = Mailbox::unbounded();
        drop(rx);
        let msg = Message::new(ProcessId::new(1, 1), rmpv::Value::Nil);
        assert!(tx.send(msg).is_err());
    }

    #[tokio::test]
    async fn recv_timeout_zero_duration() {
        let (_tx, mut rx) = Mailbox::unbounded();
        let result = rx.recv_timeout(std::time::Duration::ZERO).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn large_message_throughput() {
        let (tx, mut rx) = Mailbox::unbounded();
        let count = 10_000;
        for i in 0..count {
            tx.send(Message::new(ProcessId::new(1, 1), rmpv::Value::Integer(i.into()))).unwrap();
        }
        for _ in 0..count {
            rx.recv().await.unwrap();
        }
    }

    #[tokio::test]
    async fn concurrent_senders() {
        let (tx, mut rx) = Mailbox::unbounded();
        let mut handles = Vec::new();
        for i in 0..10u64 {
            let tx = tx.clone();
            handles.push(tokio::spawn(async move {
                tx.send(Message::new(ProcessId::new(1, i), rmpv::Value::Integer(i.into()))).unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let mut received = 0;
        while rx.recv_timeout(std::time::Duration::from_millis(100)).await.is_some() {
            received += 1;
        }
        assert_eq!(received, 10);
    }

    #[tokio::test]
    async fn bounded_try_send_full_returns_mailbox_full() {
        let (tx, _rx) = Mailbox::bounded(1);
        tx.send(Message::new(ProcessId::new(1, 1), rmpv::Value::Nil)).unwrap();
        let result = tx.try_send(Message::new(ProcessId::new(1, 2), rmpv::Value::Nil));
        match result {
            Err(SendError::MailboxFull(_)) => {},
            other => panic!("expected MailboxFull, got {:?}", other),
        }
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p rebar-core`
Expected: FAIL — `Mailbox` not defined

**Step 3: Implement Mailbox**

Add above tests:
```rust
use tokio::sync::mpsc;
use crate::process::{Message, SendError, ProcessId};

pub struct MailboxTx {
    unbounded: Option<mpsc::UnboundedSender<Message>>,
    bounded: Option<mpsc::Sender<Message>>,
}

impl Clone for MailboxTx {
    fn clone(&self) -> Self {
        Self {
            unbounded: self.unbounded.clone(),
            bounded: self.bounded.clone(),
        }
    }
}

pub struct MailboxRx {
    unbounded: Option<mpsc::UnboundedReceiver<Message>>,
    bounded: Option<mpsc::Receiver<Message>>,
}

pub struct Mailbox;

impl Mailbox {
    pub fn unbounded() -> (MailboxTx, MailboxRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            MailboxTx { unbounded: Some(tx), bounded: None },
            MailboxRx { unbounded: Some(rx), bounded: None },
        )
    }

    pub fn bounded(capacity: usize) -> (MailboxTx, MailboxRx) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            MailboxTx { unbounded: None, bounded: Some(tx) },
            MailboxRx { unbounded: None, bounded: Some(rx) },
        )
    }
}

impl MailboxTx {
    pub fn send(&self, msg: Message) -> Result<(), SendError> {
        if let Some(tx) = &self.unbounded {
            tx.send(msg).map_err(|_| SendError::ProcessDead(ProcessId::new(0, 0)))
        } else if let Some(tx) = &self.bounded {
            tx.try_send(msg).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => SendError::MailboxFull(ProcessId::new(0, 0)),
                mpsc::error::TrySendError::Closed(_) => SendError::ProcessDead(ProcessId::new(0, 0)),
            })
        } else {
            Err(SendError::ProcessDead(ProcessId::new(0, 0)))
        }
    }

    pub fn try_send(&self, msg: Message) -> Result<(), SendError> {
        self.send(msg)
    }
}

impl MailboxRx {
    pub async fn recv(&mut self) -> Option<Message> {
        if let Some(rx) = &mut self.unbounded {
            rx.recv().await
        } else if let Some(rx) = &mut self.bounded {
            rx.recv().await
        } else {
            None
        }
    }

    pub async fn recv_timeout(&mut self, duration: std::time::Duration) -> Option<Message> {
        tokio::time::timeout(duration, self.recv()).await.ok().flatten()
    }
}
```

Update `crates/rebar-core/src/process/mod.rs`:
```rust
mod types;
pub mod mailbox;

pub use types::*;
```

**Step 4: Run tests, verify pass**

Run: `cargo test -p rebar-core`
Expected: 30 PASS

**Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): add Mailbox with bounded/unbounded channels"
```

---

### Task 5: ProcessTable (~12 tests)

**Files:**
- Create: `crates/rebar-core/src/process/table.rs`
- Modify: `crates/rebar-core/src/process/mod.rs`

**Step 1: Write failing tests**

Create `crates/rebar-core/src/process/table.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessId;

    #[test]
    fn allocate_pid_increments() {
        let table = ProcessTable::new(1);
        let pid1 = table.allocate_pid();
        let pid2 = table.allocate_pid();
        assert_eq!(pid1.node_id(), 1);
        assert_eq!(pid1.local_id(), 1);
        assert_eq!(pid2.local_id(), 2);
    }

    #[test]
    fn allocate_pid_node_id_preserved() {
        let table = ProcessTable::new(42);
        let pid = table.allocate_pid();
        assert_eq!(pid.node_id(), 42);
    }

    #[test]
    fn insert_and_lookup() {
        let table = ProcessTable::new(1);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        assert!(table.get(&pid).is_some());
    }

    #[test]
    fn lookup_missing_returns_none() {
        let table = ProcessTable::new(1);
        assert!(table.get(&ProcessId::new(1, 999)).is_none());
    }

    #[test]
    fn remove_process() {
        let table = ProcessTable::new(1);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        table.remove(&pid);
        assert!(table.get(&pid).is_none());
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let table = ProcessTable::new(1);
        assert!(table.remove(&ProcessId::new(1, 999)).is_none());
    }

    #[test]
    fn send_to_process() {
        let table = ProcessTable::new(1);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        let msg = crate::process::Message::new(ProcessId::new(1, 0), rmpv::Value::Nil);
        assert!(table.send(pid, msg).is_ok());
    }

    #[test]
    fn send_to_dead_process_returns_error() {
        let table = ProcessTable::new(1);
        let msg = crate::process::Message::new(ProcessId::new(1, 0), rmpv::Value::Nil);
        assert!(table.send(ProcessId::new(1, 999), msg).is_err());
    }

    #[test]
    fn process_count() {
        let table = ProcessTable::new(1);
        assert_eq!(table.len(), 0);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn concurrent_allocate_pids_unique() {
        use std::sync::Arc;
        use std::collections::HashSet;
        let table = Arc::new(ProcessTable::new(1));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let t = Arc::clone(&table);
            handles.push(std::thread::spawn(move || {
                (0..100).map(|_| t.allocate_pid()).collect::<Vec<_>>()
            }));
        }
        let mut all_pids = HashSet::new();
        for h in handles {
            for pid in h.join().unwrap() {
                assert!(all_pids.insert(pid), "duplicate PID: {}", pid);
            }
        }
        assert_eq!(all_pids.len(), 1000);
    }

    #[test]
    fn concurrent_insert_and_send() {
        use std::sync::Arc;
        let table = Arc::new(ProcessTable::new(1));
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));

        let mut handles = Vec::new();
        for i in 0..10u64 {
            let t = Arc::clone(&table);
            handles.push(std::thread::spawn(move || {
                let msg = crate::process::Message::new(ProcessId::new(1, 0), rmpv::Value::Integer(i.into()));
                t.send(pid, msg)
            }));
        }
        for h in handles {
            assert!(h.join().unwrap().is_ok());
        }
    }

    #[test]
    fn is_empty() {
        let table = ProcessTable::new(1);
        assert!(table.is_empty());
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        assert!(!table.is_empty());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p rebar-core`
Expected: FAIL — `ProcessTable`, `ProcessHandle` not defined

**Step 3: Implement ProcessTable**

```rust
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use crate::process::{ProcessId, Message, SendError};
use crate::process::mailbox::MailboxTx;

pub struct ProcessHandle {
    tx: MailboxTx,
}

impl ProcessHandle {
    pub fn new(tx: MailboxTx) -> Self {
        Self { tx }
    }

    pub fn send(&self, msg: Message) -> Result<(), SendError> {
        self.tx.send(msg)
    }
}

pub struct ProcessTable {
    node_id: u64,
    next_id: AtomicU64,
    processes: DashMap<ProcessId, ProcessHandle>,
}

impl ProcessTable {
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            next_id: AtomicU64::new(1),
            processes: DashMap::new(),
        }
    }

    pub fn allocate_pid(&self) -> ProcessId {
        let local_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        ProcessId::new(self.node_id, local_id)
    }

    pub fn insert(&self, pid: ProcessId, handle: ProcessHandle) {
        self.processes.insert(pid, handle);
    }

    pub fn get(&self, pid: &ProcessId) -> Option<dashmap::mapref::one::Ref<ProcessId, ProcessHandle>> {
        self.processes.get(pid)
    }

    pub fn remove(&self, pid: &ProcessId) -> Option<(ProcessId, ProcessHandle)> {
        self.processes.remove(pid)
    }

    pub fn send(&self, pid: ProcessId, msg: Message) -> Result<(), SendError> {
        match self.processes.get(&pid) {
            Some(handle) => handle.send(msg),
            None => Err(SendError::ProcessDead(pid)),
        }
    }

    pub fn len(&self) -> usize {
        self.processes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}
```

Update mod.rs:
```rust
mod types;
pub mod mailbox;
pub mod table;

pub use types::*;
```

**Step 4: Run tests, verify pass**

Run: `cargo test -p rebar-core`
Expected: 42 PASS

**Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): add ProcessTable with concurrent access"
```

---

### Task 6: Links and Monitors (~12 tests)

**Files:**
- Create: `crates/rebar-core/src/process/monitor.rs`
- Modify: `crates/rebar-core/src/process/mod.rs`

**Step 1: Write failing tests**

Create `crates/rebar-core/src/process/monitor.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessId;

    #[test]
    fn monitor_ref_unique() {
        let r1 = MonitorRef::new();
        let r2 = MonitorRef::new();
        assert_ne!(r1, r2);
    }

    #[test]
    fn monitor_ref_is_copy() {
        let r = MonitorRef::new();
        let r2 = r;
        assert_eq!(r, r2);
    }

    #[test]
    fn monitor_set_add_and_query() {
        let mut set = MonitorSet::new();
        let pid = ProcessId::new(1, 1);
        let mref = set.add_monitor(pid);
        let monitors: Vec<_> = set.monitors_for(pid).collect();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0], mref);
    }

    #[test]
    fn monitor_set_multiple_monitors_same_target() {
        let mut set = MonitorSet::new();
        let pid = ProcessId::new(1, 1);
        let r1 = set.add_monitor(pid);
        let r2 = set.add_monitor(pid);
        assert_ne!(r1, r2);
        assert_eq!(set.monitors_for(pid).count(), 2);
    }

    #[test]
    fn monitor_set_remove() {
        let mut set = MonitorSet::new();
        let pid = ProcessId::new(1, 1);
        let mref = set.add_monitor(pid);
        set.remove_monitor(mref);
        assert_eq!(set.monitors_for(pid).count(), 0);
    }

    #[test]
    fn monitor_set_remove_one_of_many() {
        let mut set = MonitorSet::new();
        let pid = ProcessId::new(1, 1);
        let r1 = set.add_monitor(pid);
        let _r2 = set.add_monitor(pid);
        set.remove_monitor(r1);
        assert_eq!(set.monitors_for(pid).count(), 1);
    }

    #[test]
    fn monitor_set_different_targets() {
        let mut set = MonitorSet::new();
        set.add_monitor(ProcessId::new(1, 1));
        set.add_monitor(ProcessId::new(1, 2));
        assert_eq!(set.monitors_for(ProcessId::new(1, 1)).count(), 1);
        assert_eq!(set.monitors_for(ProcessId::new(1, 2)).count(), 1);
    }

    #[test]
    fn monitor_set_remove_nonexistent_is_noop() {
        let mut set = MonitorSet::new();
        set.remove_monitor(MonitorRef::new()); // should not panic
    }

    #[test]
    fn link_set_add_and_contains() {
        let mut set = LinkSet::new();
        let pid = ProcessId::new(1, 1);
        set.add_link(pid);
        assert!(set.is_linked(pid));
    }

    #[test]
    fn link_set_remove() {
        let mut set = LinkSet::new();
        let pid = ProcessId::new(1, 1);
        set.add_link(pid);
        set.remove_link(pid);
        assert!(!set.is_linked(pid));
    }

    #[test]
    fn link_set_linked_pids_iter() {
        let mut set = LinkSet::new();
        set.add_link(ProcessId::new(1, 1));
        set.add_link(ProcessId::new(1, 2));
        let pids: Vec<_> = set.linked_pids().collect();
        assert_eq!(pids.len(), 2);
    }

    #[test]
    fn link_set_duplicate_add_is_idempotent() {
        let mut set = LinkSet::new();
        let pid = ProcessId::new(1, 1);
        set.add_link(pid);
        set.add_link(pid);
        assert_eq!(set.linked_pids().count(), 1);
    }
}
```

**Step 2: Run tests, verify fail**
**Step 3: Implement (same as original Task 7)**
**Step 4: Run tests, verify 54 PASS**
**Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): add MonitorRef, MonitorSet, LinkSet"
```

---

### Task 7: Runtime with Process Spawning (~15 tests)

**Files:**
- Create: `crates/rebar-core/src/runtime.rs`
- Modify: `crates/rebar-core/src/lib.rs`

**Step 1: Write failing tests**

Create `crates/rebar-core/src/runtime.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_returns_pid() {
        let rt = Runtime::new(1);
        let pid = rt.spawn(|_ctx| async {}).await;
        assert_eq!(pid.node_id(), 1);
        assert_eq!(pid.local_id(), 1);
    }

    #[tokio::test]
    async fn spawn_multiple_unique_pids() {
        let rt = Runtime::new(1);
        let pid1 = rt.spawn(|_ctx| async {}).await;
        let pid2 = rt.spawn(|_ctx| async {}).await;
        assert_ne!(pid1, pid2);
    }

    #[tokio::test]
    async fn send_message_between_processes() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        let receiver = rt.spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            done_tx.send(msg.payload().as_str().unwrap().to_string()).unwrap();
        }).await;

        rt.spawn(move |ctx| async move {
            ctx.send(receiver, rmpv::Value::String("hello".into())).await.unwrap();
        }).await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await.unwrap().unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn self_pid_is_correct() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let pid = rt.spawn(move |ctx| async move {
            done_tx.send(ctx.self_pid()).unwrap();
        }).await;
        let reported = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await.unwrap().unwrap();
        assert_eq!(pid, reported);
    }

    #[tokio::test]
    async fn send_to_dead_process_returns_error() {
        let rt = Runtime::new(1);
        let result = rt.send(ProcessId::new(1, 999), rmpv::Value::Nil).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recv_timeout_in_process() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |mut ctx| async move {
            let result = ctx.recv_timeout(std::time::Duration::from_millis(10)).await;
            done_tx.send(result.is_none()).unwrap();
        }).await;
        let was_none = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await.unwrap().unwrap();
        assert!(was_none);
    }

    #[tokio::test]
    async fn process_can_send_to_self() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |mut ctx| async move {
            let me = ctx.self_pid();
            ctx.send(me, rmpv::Value::String("self-msg".into())).await.unwrap();
            let msg = ctx.recv().await.unwrap();
            done_tx.send(msg.payload().as_str().unwrap().to_string()).unwrap();
        }).await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await.unwrap().unwrap();
        assert_eq!(result, "self-msg");
    }

    #[tokio::test]
    async fn chain_of_three_processes() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();

        let c = rt.spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            let val = msg.payload().as_u64().unwrap();
            done_tx.send(val).unwrap();
        }).await;

        let b = rt.spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            let val = msg.payload().as_u64().unwrap();
            ctx.send(c, rmpv::Value::Integer((val + 1).into())).await.unwrap();
        }).await;

        rt.spawn(move |ctx| async move {
            ctx.send(b, rmpv::Value::Integer(1u64.into())).await.unwrap();
        }).await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await.unwrap().unwrap();
        assert_eq!(result, 2);
    }

    #[tokio::test]
    async fn fan_out_fan_in() {
        let rt = Runtime::new(1);
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let mut workers = Vec::new();
        for _ in 0..5 {
            let tx = tx.clone();
            let pid = rt.spawn(move |mut ctx| async move {
                let msg = ctx.recv().await.unwrap();
                let val = msg.payload().as_u64().unwrap();
                tx.send(val * 2).await.unwrap();
            }).await;
            workers.push(pid);
        }
        drop(tx);

        rt.spawn(move |ctx| async move {
            for (i, pid) in workers.iter().enumerate() {
                ctx.send(*pid, rmpv::Value::Integer((i as u64).into())).await.unwrap();
            }
        }).await;

        let mut results = Vec::new();
        while let Ok(Some(val)) = tokio::time::timeout(
            std::time::Duration::from_secs(2), rx.recv()).await {
            results.push(val);
        }
        results.sort();
        assert_eq!(results, vec![0, 2, 4, 6, 8]);
    }

    #[tokio::test]
    async fn spawn_100_processes() {
        let rt = Runtime::new(1);
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        for i in 0..100u64 {
            let tx = tx.clone();
            rt.spawn(move |_ctx| async move {
                tx.send(i).await.unwrap();
            }).await;
        }
        drop(tx);
        let mut count = 0;
        while let Ok(Some(_)) = tokio::time::timeout(
            std::time::Duration::from_secs(5), rx.recv()).await {
            count += 1;
        }
        assert_eq!(count, 100);
    }

    #[tokio::test]
    async fn process_exit_removes_from_table() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let pid = rt.spawn(move |_ctx| async move {
            done_tx.send(()).unwrap();
            // process exits immediately
        }).await;
        done_rx.await.unwrap();
        // small delay for cleanup
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // process should be cleaned up
        assert!(rt.send(pid, rmpv::Value::Nil).await.is_err());
    }

    #[tokio::test]
    async fn process_panic_does_not_crash_runtime() {
        let rt = Runtime::new(1);
        rt.spawn(|_ctx| async move {
            panic!("intentional panic");
        }).await;
        // runtime should still work
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |_ctx| async move {
            done_tx.send(42u64).unwrap();
        }).await;
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await.unwrap().unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn node_id_accessor() {
        let rt = Runtime::new(42);
        assert_eq!(rt.node_id(), 42);
    }

    #[tokio::test]
    async fn multiple_messages_to_same_process() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let receiver = rt.spawn(move |mut ctx| async move {
            let mut sum = 0u64;
            for _ in 0..5 {
                let msg = ctx.recv().await.unwrap();
                sum += msg.payload().as_u64().unwrap();
            }
            done_tx.send(sum).unwrap();
        }).await;

        for i in 1..=5u64 {
            rt.send(receiver, rmpv::Value::Integer(i.into())).await.unwrap();
        }

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await.unwrap().unwrap();
        assert_eq!(result, 15);
    }

    #[tokio::test]
    async fn process_context_send_returns_error_for_dead_target() {
        let rt = Runtime::new(1);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |ctx| async move {
            let result = ctx.send(ProcessId::new(1, 999), rmpv::Value::Nil).await;
            done_tx.send(result.is_err()).unwrap();
        }).await;
        let was_err = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
            .await.unwrap().unwrap();
        assert!(was_err);
    }
}
```

**Step 2: Run tests, verify fail**
**Step 3: Implement Runtime and ProcessContext (same structure as original Task 6, add `node_id()` accessor and process cleanup on exit)**
**Step 4: Run tests, verify 69 PASS**
**Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): add Runtime with process spawning and local messaging"
```

---

### Task 8: Supervisor Spec Types (~10 tests)

**Files:**
- Create: `crates/rebar-core/src/supervisor/mod.rs`
- Create: `crates/rebar-core/src/supervisor/spec.rs`
- Modify: `crates/rebar-core/src/lib.rs`

**Step 1: Write failing tests**

Create `crates/rebar-core/src/supervisor/spec.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ExitReason;

    #[test]
    fn permanent_restarts_on_normal() {
        assert!(RestartType::Permanent.should_restart(&ExitReason::Normal));
    }

    #[test]
    fn permanent_restarts_on_abnormal() {
        assert!(RestartType::Permanent.should_restart(&ExitReason::Abnormal("err".into())));
    }

    #[test]
    fn permanent_restarts_on_kill() {
        assert!(RestartType::Permanent.should_restart(&ExitReason::Kill));
    }

    #[test]
    fn transient_does_not_restart_on_normal() {
        assert!(!RestartType::Transient.should_restart(&ExitReason::Normal));
    }

    #[test]
    fn transient_restarts_on_abnormal() {
        assert!(RestartType::Transient.should_restart(&ExitReason::Abnormal("err".into())));
    }

    #[test]
    fn temporary_never_restarts() {
        assert!(!RestartType::Temporary.should_restart(&ExitReason::Normal));
        assert!(!RestartType::Temporary.should_restart(&ExitReason::Abnormal("err".into())));
        assert!(!RestartType::Temporary.should_restart(&ExitReason::Kill));
    }

    #[test]
    fn supervisor_spec_builder() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(3)
            .max_seconds(5);
        assert_eq!(spec.max_restarts, 3);
        assert_eq!(spec.max_seconds, 5);
    }

    #[test]
    fn supervisor_spec_with_children() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForAll)
            .child(ChildSpec::new("worker_1"))
            .child(ChildSpec::new("worker_2"));
        assert_eq!(spec.children.len(), 2);
    }

    #[test]
    fn child_spec_defaults() {
        let child = ChildSpec::new("test");
        assert_eq!(child.id, "test");
        matches!(child.restart, RestartType::Permanent);
        matches!(child.shutdown, ShutdownStrategy::Timeout(_));
    }

    #[test]
    fn child_spec_builder() {
        let child = ChildSpec::new("worker")
            .restart(RestartType::Transient)
            .shutdown(ShutdownStrategy::BrutalKill);
        matches!(child.restart, RestartType::Transient);
        matches!(child.shutdown, ShutdownStrategy::BrutalKill);
    }
}
```

**Step 2: Run tests, verify fail**
**Step 3: Implement (same as original Task 10)**
**Step 4: Run tests, verify 79 PASS**
**Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): add supervisor spec types (strategies, child specs)"
```

---

## Phase 3: Wire Protocol and Transport (rebar-cluster)

### Task 9: Wire Protocol Frame Codec (~15 tests)

**Files:**
- Create: `crates/rebar-cluster/src/protocol/mod.rs`
- Create: `crates/rebar-cluster/src/protocol/frame.rs`
- Modify: `crates/rebar-cluster/src/lib.rs`

**Step 1: Write failing tests**

Create `crates/rebar-cluster/src/protocol/frame.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_send() {
        let frame = Frame {
            version: 1,
            msg_type: MsgType::Send,
            request_id: 0,
            header: rmpv::Value::Map(vec![
                (rmpv::Value::String("dest".into()), rmpv::Value::Integer(42.into())),
            ]),
            payload: rmpv::Value::String("hello".into()),
        };
        let bytes = frame.encode();
        let decoded = Frame::decode(&bytes).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.msg_type, MsgType::Send);
        assert_eq!(decoded.payload, rmpv::Value::String("hello".into()));
    }

    #[test]
    fn encode_decode_heartbeat() {
        let frame = Frame { version: 1, msg_type: MsgType::Heartbeat, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.msg_type, MsgType::Heartbeat);
    }

    #[test]
    fn encode_decode_with_request_id() {
        let frame = Frame { version: 1, msg_type: MsgType::NameLookup, request_id: 12345, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.request_id, 12345);
    }

    #[test]
    fn all_msg_types_roundtrip() {
        let types = [
            MsgType::Send, MsgType::Monitor, MsgType::Demonitor,
            MsgType::Link, MsgType::Unlink, MsgType::Exit,
            MsgType::ProcessDown, MsgType::NameLookup,
            MsgType::NameRegister, MsgType::NameUnregister,
            MsgType::Heartbeat, MsgType::HeartbeatAck, MsgType::NodeInfo,
        ];
        for msg_type in types {
            let frame = Frame { version: 1, msg_type, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
            let decoded = Frame::decode(&frame.encode()).unwrap();
            assert_eq!(decoded.msg_type, msg_type);
        }
    }

    #[test]
    fn decode_invalid_msg_type() {
        let mut bytes = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil }.encode();
        bytes[1] = 0xFF; // invalid
        assert!(Frame::decode(&bytes).is_err());
    }

    #[test]
    fn decode_truncated_header() {
        assert!(Frame::decode(&[0u8; 5]).is_err());
    }

    #[test]
    fn decode_truncated_payload() {
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::String("data".into()) };
        let bytes = frame.encode();
        let truncated = &bytes[..bytes.len() - 2];
        assert!(Frame::decode(truncated).is_err());
    }

    #[test]
    fn decode_empty_bytes() {
        assert!(Frame::decode(&[]).is_err());
    }

    #[test]
    fn large_payload_roundtrip() {
        let big_string = "x".repeat(100_000);
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::String(big_string.clone().into()) };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload.as_str().unwrap().len(), 100_000);
    }

    #[test]
    fn binary_payload_roundtrip() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Binary(data.clone()) };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload, rmpv::Value::Binary(data));
    }

    #[test]
    fn nested_map_payload() {
        let payload = rmpv::Value::Map(vec![
            (rmpv::Value::String("nested".into()), rmpv::Value::Map(vec![
                (rmpv::Value::String("deep".into()), rmpv::Value::Integer(42.into())),
            ])),
        ]);
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.payload.as_map().unwrap().len(), 1);
    }

    #[test]
    fn max_request_id() {
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: u64::MAX, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.request_id, u64::MAX);
    }

    #[test]
    fn version_preserved() {
        let frame = Frame { version: 42, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.version, 42);
    }

    #[test]
    fn encode_deterministic() {
        let frame = Frame { version: 1, msg_type: MsgType::Heartbeat, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        let a = frame.encode();
        let b = frame.encode();
        assert_eq!(a, b);
    }

    #[test]
    fn header_and_payload_both_populated() {
        let header = rmpv::Value::Map(vec![
            (rmpv::Value::String("from".into()), rmpv::Value::Integer(1.into())),
            (rmpv::Value::String("to".into()), rmpv::Value::Integer(2.into())),
        ]);
        let payload = rmpv::Value::String("body".into());
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 7, header, payload };
        let decoded = Frame::decode(&frame.encode()).unwrap();
        assert_eq!(decoded.header.as_map().unwrap().len(), 2);
        assert_eq!(decoded.payload.as_str().unwrap(), "body");
    }
}
```

**Step 2: Run tests, verify fail**
**Step 3: Implement (same as original Task 8)**
**Step 4: Run tests, verify 94 PASS**
**Step 5: Commit**

```bash
git add -A && git commit -m "feat(cluster): add wire protocol frame codec"
```

---

### Task 10: Transport Trait and TCP Implementation (~15 tests)

**Files:**
- Create: `crates/rebar-cluster/src/transport/mod.rs`
- Create: `crates/rebar-cluster/src/transport/traits.rs`
- Create: `crates/rebar-cluster/src/transport/tcp.rs`
- Modify: `crates/rebar-cluster/src/lib.rs`

**Step 1: Write failing tests**

Create `crates/rebar-cluster/src/transport/tcp.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Frame, MsgType};

    #[tokio::test]
    async fn connect_and_send_frame() {
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });
        let mut client = transport.connect(addr).await.unwrap();
        let frame = Frame { version: 1, msg_type: MsgType::Heartbeat, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
        client.send(&frame).await.unwrap();
        client.close().await.unwrap();
        let received = server.await.unwrap();
        assert_eq!(received.msg_type, MsgType::Heartbeat);
    }

    #[tokio::test]
    async fn bidirectional_echo() {
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let frame = conn.recv().await.unwrap();
            conn.send(&frame).await.unwrap();
        });
        let mut client = transport.connect(addr).await.unwrap();
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 42, header: rmpv::Value::Nil, payload: rmpv::Value::String("ping".into()) };
        client.send(&frame).await.unwrap();
        let response = client.recv().await.unwrap();
        assert_eq!(response.request_id, 42);
        assert_eq!(response.payload, rmpv::Value::String("ping".into()));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_frames_sequential() {
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut frames = Vec::new();
            for _ in 0..5 {
                frames.push(conn.recv().await.unwrap());
            }
            frames
        });
        let mut client = transport.connect(addr).await.unwrap();
        for i in 0..5u64 {
            let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: i, header: rmpv::Value::Nil, payload: rmpv::Value::Integer(i.into()) };
            client.send(&frame).await.unwrap();
        }
        let frames = server.await.unwrap();
        assert_eq!(frames.len(), 5);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.request_id, i as u64);
        }
    }

    #[tokio::test]
    async fn large_frame() {
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        let big = "x".repeat(1_000_000);
        let big_clone = big.clone();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            conn.recv().await.unwrap()
        });
        let mut client = transport.connect(addr).await.unwrap();
        let frame = Frame { version: 1, msg_type: MsgType::Send, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::String(big.into()) };
        client.send(&frame).await.unwrap();
        let received = server.await.unwrap();
        assert_eq!(received.payload.as_str().unwrap().len(), 1_000_000);
    }

    #[tokio::test]
    async fn recv_after_close_returns_error() {
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let result = conn.recv().await;
            assert!(result.is_err());
        });
        let mut client = transport.connect(addr).await.unwrap();
        client.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_clients() {
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn1 = listener.accept().await.unwrap();
            let mut conn2 = listener.accept().await.unwrap();
            let f1 = conn1.recv().await.unwrap();
            let f2 = conn2.recv().await.unwrap();
            (f1.request_id, f2.request_id)
        });
        let mut c1 = transport.connect(addr).await.unwrap();
        let mut c2 = transport.connect(addr).await.unwrap();
        c1.send(&Frame { version: 1, msg_type: MsgType::Send, request_id: 100, header: rmpv::Value::Nil, payload: rmpv::Value::Nil }).await.unwrap();
        c2.send(&Frame { version: 1, msg_type: MsgType::Send, request_id: 200, header: rmpv::Value::Nil, payload: rmpv::Value::Nil }).await.unwrap();
        let (r1, r2) = server.await.unwrap();
        assert!(r1 == 100 || r1 == 200);
        assert!(r2 == 100 || r2 == 200);
        assert_ne!(r1, r2);
    }

    #[tokio::test]
    async fn high_throughput() {
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        let count = 1000u64;
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut received = 0;
            for _ in 0..count {
                conn.recv().await.unwrap();
                received += 1;
            }
            received
        });
        let mut client = transport.connect(addr).await.unwrap();
        for i in 0..count {
            let frame = Frame { version: 1, msg_type: MsgType::Heartbeat, request_id: i, header: rmpv::Value::Nil, payload: rmpv::Value::Nil };
            client.send(&frame).await.unwrap();
        }
        let received = server.await.unwrap();
        assert_eq!(received, count);
    }

    #[tokio::test]
    async fn connect_to_invalid_address_returns_error() {
        let transport = TcpTransport::new();
        let result = transport.connect("127.0.0.1:1".parse().unwrap()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn listener_local_addr() {
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        assert_eq!(addr.ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        assert_ne!(addr.port(), 0);
    }

    #[tokio::test]
    async fn all_msg_types_over_tcp() {
        let types = [
            MsgType::Send, MsgType::Monitor, MsgType::Demonitor,
            MsgType::Link, MsgType::Unlink, MsgType::Exit,
            MsgType::ProcessDown, MsgType::NameLookup,
            MsgType::NameRegister, MsgType::NameUnregister,
            MsgType::Heartbeat, MsgType::HeartbeatAck, MsgType::NodeInfo,
        ];
        let transport = TcpTransport::new();
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr();
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let mut received = Vec::new();
            for _ in 0..types.len() {
                received.push(conn.recv().await.unwrap().msg_type);
            }
            received
        });
        let mut client = transport.connect(addr).await.unwrap();
        for msg_type in &types {
            client.send(&Frame { version: 1, msg_type: *msg_type, request_id: 0, header: rmpv::Value::Nil, payload: rmpv::Value::Nil }).await.unwrap();
        }
        let received = server.await.unwrap();
        assert_eq!(received, types.to_vec());
    }
}
```

**Step 2: Run tests, verify fail**
**Step 3: Implement transport trait and TCP (same structure as original Task 9, add `PartialEq` to `MsgType`)**
**Step 4: Run tests, verify 104 PASS (note: `all_msg_types_over_tcp` counts as 1 test)**
**Step 5: Commit**

```bash
git add -A && git commit -m "feat(cluster): add Transport trait and TCP implementation"
```

---

## Phase 4: SWIM Discovery (rebar-cluster)

### Task 11: SWIM Membership (~15 tests)

**Files:**
- Create: `crates/rebar-cluster/src/swim/mod.rs`
- Create: `crates/rebar-cluster/src/swim/member.rs`
- Modify: `crates/rebar-cluster/src/lib.rs`

**Step 1: Write failing tests**

Create `crates/rebar-cluster/src/swim/member.rs` with tests covering:
```rust
// Tests:
// 1. new_member_is_alive
// 2. suspect_member
// 3. refute_suspicion_with_higher_incarnation
// 4. ignore_stale_alive
// 5. declare_dead
// 6. dead_cannot_be_revived
// 7. suspect_with_lower_incarnation_ignored
// 8. membership_list_add_and_get
// 9. membership_list_remove_dead
// 10. membership_list_random_alive_excludes_self
// 11. membership_list_random_alive_excludes_dead
// 12. membership_list_random_alive_excludes_suspect
// 13. membership_list_alive_count
// 14. membership_list_all_members_iter
// 15. membership_list_empty_random_returns_none
```

Each test follows the same pattern as original Task 11 but adds edge cases: dead nodes cannot be revived via alive(), suspect with lower incarnation is ignored, random excludes suspect and dead nodes.

**Step 2-5: Implement, run, commit**

```bash
git add -A && git commit -m "feat(cluster): add SWIM membership with state transitions"
```

---

### Task 12: SWIM Gossip Dissemination (~10 tests)

**Files:**
- Create: `crates/rebar-cluster/src/swim/gossip.rs`
- Modify: `crates/rebar-cluster/src/swim/mod.rs`

**Step 1: Write failing tests**

Tests covering:
```rust
// 1. add_update_to_queue
// 2. drain_returns_bounded_count
// 3. drain_more_than_available_returns_all
// 4. drain_empty_queue
// 5. gossip_alive_serialization_roundtrip
// 6. gossip_suspect_serialization_roundtrip
// 7. gossip_dead_serialization_roundtrip
// 8. gossip_leave_serialization_roundtrip
// 9. gossip_queue_fifo_order
// 10. gossip_addr_preserved_in_roundtrip
```

**Step 2-5: Implement, run, commit**

```bash
git add -A && git commit -m "feat(cluster): add SWIM gossip queue and serialization"
```

---

### Task 13: SWIM Config and Failure Detector (~12 tests)

**Files:**
- Create: `crates/rebar-cluster/src/swim/config.rs`
- Create: `crates/rebar-cluster/src/swim/detector.rs`
- Modify: `crates/rebar-cluster/src/swim/mod.rs`

**Step 1: Write failing tests**

Tests covering:
```rust
// Config:
// 1. default_config_values
// 2. config_builder_overrides
//
// Failure detector:
// 3. ping_alive_node_stays_alive
// 4. missed_ping_marks_suspect
// 5. suspect_timeout_marks_dead
// 6. suspect_node_refutes_with_alive
// 7. indirect_probe_succeeds_keeps_alive
// 8. dead_node_triggers_callback
// 9. multiple_suspects_tracked
// 10. dead_removal_delay_respected
// 11. tick_selects_random_target
// 12. no_targets_available_skips_tick
```

These test the core SWIM failure detection loop using a mock transport (trait object that captures sent messages and can inject responses).

**Step 2-5: Implement, run, commit**

```bash
git add -A && git commit -m "feat(cluster): add SWIM config and failure detector"
```

---

## Phase 5: Global Registry (rebar-cluster)

### Task 14: CRDT OR-Set Registry (~20 tests)

**Files:**
- Create: `crates/rebar-cluster/src/registry/mod.rs`
- Create: `crates/rebar-cluster/src/registry/orset.rs`
- Modify: `crates/rebar-cluster/src/lib.rs`

**Step 1: Write failing tests**

```rust
// Basic operations:
// 1. register_and_lookup
// 2. unregister
// 3. lookup_nonexistent_returns_none
// 4. registered_returns_all
//
// Conflict resolution:
// 5. last_writer_wins
// 6. conflict_with_same_timestamp_deterministic
//
// Cleanup:
// 7. remove_by_pid_cleans_all_names
// 8. remove_by_pid_doesnt_affect_others
// 9. remove_by_node_cleans_all_from_node
// 10. remove_by_node_preserves_other_nodes
//
// Delta merging:
// 11. merge_delta_add
// 12. merge_delta_remove
// 13. merge_delta_add_then_remove
// 14. merge_delta_remove_then_add_with_new_tag
// 15. merge_idempotent_add
// 16. merge_tombstoned_tag_not_re_added
//
// Convergence:
// 17. two_registries_converge_after_delta_exchange
// 18. concurrent_adds_different_names_merge_cleanly
// 19. concurrent_adds_same_name_lww_after_merge
//
// Edge cases:
// 20. register_empty_name
```

**Step 2-5: Implement, run, commit**

```bash
git add -A && git commit -m "feat(cluster): add CRDT OR-Set global registry"
```

---

## Phase 6: Connection Manager (rebar-cluster)

### Task 15: Connection Manager (~15 tests)

**Files:**
- Create: `crates/rebar-cluster/src/connection/mod.rs`
- Create: `crates/rebar-cluster/src/connection/manager.rs`
- Modify: `crates/rebar-cluster/src/lib.rs`

**Step 1: Write failing tests**

```rust
// Connection lifecycle:
// 1. connect_to_new_node
// 2. route_frame_to_connected_node
// 3. route_to_unknown_node_returns_error
// 4. disconnect_node
// 5. reconnect_after_disconnect
//
// Event handling:
// 6. on_node_discovered_connects
// 7. on_node_discovered_idempotent
// 8. on_connection_lost_fires_node_down
// 9. on_connection_lost_triggers_reconnect
//
// Reconnection:
// 10. exponential_backoff_timing
// 11. max_backoff_capped
// 12. reconnect_succeeds_restores_routing
//
// Multi-node:
// 13. full_mesh_three_nodes
// 14. concurrent_route_to_multiple_nodes
// 15. connection_count
```

The ConnectionManager wraps `TcpTransport` (or any `Transport` impl) and uses SWIM events to drive connect/disconnect. Tests use a `MockTransport` trait impl.

**Step 2-5: Implement, run, commit**

```bash
git add -A && git commit -m "feat(cluster): add ConnectionManager with reconnection"
```

---

## Phase 7: Supervisor Engine (rebar-core)

### Task 16: Supervisor Engine (~25 tests)

**Files:**
- Create: `crates/rebar-core/src/supervisor/engine.rs`
- Modify: `crates/rebar-core/src/supervisor/mod.rs`

**Step 1: Write failing tests**

```rust
// Basic lifecycle:
// 1. supervisor_starts_children_in_order
// 2. supervisor_stops_children_in_reverse_order
// 3. supervisor_is_a_process_with_pid
//
// OneForOne:
// 4. one_for_one_restarts_only_failed_child
// 5. one_for_one_other_children_unaffected
// 6. one_for_one_permanent_child_always_restarts
// 7. one_for_one_transient_child_normal_exit_no_restart
// 8. one_for_one_transient_child_abnormal_restarts
// 9. one_for_one_temporary_child_never_restarts
//
// OneForAll:
// 10. one_for_all_restarts_all_on_single_failure
// 11. one_for_all_stops_in_reverse_starts_in_order
//
// RestForOne:
// 12. rest_for_one_restarts_failed_and_subsequent
// 13. rest_for_one_earlier_children_unaffected
//
// Restart limits:
// 14. max_restarts_within_window_escalates
// 15. restarts_outside_window_reset_counter
// 16. max_restarts_zero_means_never_restart
//
// Shutdown:
// 17. shutdown_timeout_respected
// 18. brutal_kill_immediate
// 19. graceful_shutdown_sends_exit_signal
//
// Exit signals:
// 20. child_exit_sends_signal_to_linked
// 21. trap_exit_converts_signal_to_message
// 22. no_trap_exit_propagates_death
//
// Nested supervisors:
// 23. nested_supervisor_escalation
// 24. nested_supervisor_independent_restart
//
// Dynamic children:
// 25. add_child_dynamically
```

These tests use the Runtime to spawn supervisors and verify restart behavior by monitoring child processes through channels.

**Step 2-5: Implement, run, commit**

```bash
git add -A && git commit -m "feat(core): add supervisor engine with restart strategies"
```

---

## Phase 8: C-ABI FFI (rebar-ffi)

### Task 17: FFI Bindings (~15 tests)

**Files:**
- Modify: `crates/rebar-ffi/src/lib.rs`
- Modify: `crates/rebar-ffi/Cargo.toml`

**Step 1: Write failing tests**

```rust
// Message lifecycle:
// 1. msg_create_and_read
// 2. msg_empty_data
// 3. msg_large_data
// 4. msg_free_null_is_noop
// 5. msg_data_ptr_stable
//
// Runtime lifecycle:
// 6. runtime_create_destroy
// 7. runtime_create_with_different_node_ids
//
// PID types:
// 8. pid_components
// 9. pid_zero_values
//
// Spawn and send:
// 10. spawn_returns_valid_pid
// 11. send_to_spawned_process
// 12. send_to_invalid_pid_returns_error
//
// Registry:
// 13. register_and_whereis
// 14. send_named
// 15. whereis_not_found
```

**Step 2-5: Implement, run, commit**

```bash
git add -A && git commit -m "feat(ffi): add C-ABI bindings for runtime, messages, registry"
```

---

## Phase 9: Integration Tests

### Task 18: Local Integration Tests (~8 tests)

**Files:**
- Create: `crates/rebar-core/tests/integration.rs`

```rust
// 1. ping_pong_between_processes
// 2. fan_out_to_multiple_workers
// 3. chain_of_three_processes
// 4. supervisor_restarts_crashed_worker
// 5. supervisor_one_for_all_cascade
// 6. process_monitor_receives_down
// 7. linked_process_dies_together
// 8. spawn_1000_processes_all_complete
```

**Commit:**
```bash
git add -A && git commit -m "test: add local integration tests"
```

---

### Task 19: Cluster Integration Tests (~7 tests)

**Files:**
- Create: `crates/rebar-cluster/tests/cluster_integration.rs`

```rust
// 1. two_nodes_discover_via_swim
// 2. send_message_across_nodes
// 3. remote_process_monitor_fires_on_exit
// 4. registry_name_resolves_across_nodes
// 5. node_down_fires_when_node_disconnects
// 6. reconnection_after_transient_failure
// 7. three_node_mesh_all_connected
```

These tests spin up multiple `Runtime` instances on localhost with different ports.

**Commit:**
```bash
git add -A && git commit -m "test: add cluster integration tests"
```

---

## Test Summary

| Task | Component | Crate | Tests |
|------|-----------|-------|-------|
| 1 | Scaffold | all | build check |
| 2 | ProcessId | rebar-core | 8 |
| 3 | Message, ExitReason, SendError | rebar-core | 10 |
| 4 | Mailbox | rebar-core | 12 |
| 5 | ProcessTable | rebar-core | 12 |
| 6 | Links & Monitors | rebar-core | 12 |
| 7 | Runtime & Spawning | rebar-core | 15 |
| 8 | Supervisor Specs | rebar-core | 10 |
| 9 | Wire Protocol Frames | rebar-cluster | 15 |
| 10 | Transport + TCP | rebar-cluster | 10 |
| 11 | SWIM Membership | rebar-cluster | 15 |
| 12 | SWIM Gossip | rebar-cluster | 10 |
| 13 | SWIM Failure Detector | rebar-cluster | 12 |
| 14 | CRDT Registry | rebar-cluster | 20 |
| 15 | Connection Manager | rebar-cluster | 15 |
| 16 | Supervisor Engine | rebar-core | 25 |
| 17 | FFI Bindings | rebar-ffi | 15 |
| 18 | Local Integration | rebar-core | 8 |
| 19 | Cluster Integration | rebar-cluster | 7 |
| **Total** | | | **~231** |

---

## Execution

Plan complete and saved to `docs/plans/2026-03-03-rebar-implementation.md`.

Two execution options:

**1. Subagent-Driven (this session)** — Dispatch fresh subagent per task, review between tasks, fast iteration

**2. Parallel Session (separate)** — Open new session with executing-plans skill, batch execution with checkpoints

Which approach?
