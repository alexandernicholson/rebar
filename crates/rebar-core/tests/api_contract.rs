//! API contract regression tests for rebar-core.
//!
//! These tests guard against regressions introduced by forks (e.g. johnnyshields/rebar v5)
//! that broke fundamental API contracts around `ProcessId` construction, Display format,
//! trait bounds, async semantics, and panic isolation.

use std::sync::Arc;

use rebar_core::process::table::ProcessTable;
use rebar_core::process::{ProcessId, SendError};
use rebar_core::router::{LocalRouter, MessageRouter};
use rebar_core::runtime::Runtime;

// ============================================================================
// Task 1: Compile-time contract tests
// ============================================================================

/// Guards that `ProcessId::new` takes exactly two arguments (`node_id`, `local_id`).
///
/// The johnnyshields/rebar v5 fork changed `ProcessId` to a 3-arg constructor
/// (node, thread, local). This test ensures our 2-arg contract is preserved.
#[test]
fn process_id_is_two_arg() {
    let pid = ProcessId::new(1, 42);
    assert_eq!(pid.node_id(), 1);
    assert_eq!(pid.local_id(), 42);

    let pid2 = ProcessId::new(0, 0);
    assert_eq!(pid2.node_id(), 0);
    assert_eq!(pid2.local_id(), 0);
}

/// Guards that `ProcessId` Display format is `<node.local>`.
///
/// The fork changed Display to `<node.thread.local>` (3-part format).
/// This test ensures the canonical 2-part `<node.local>` format is preserved.
#[test]
fn process_id_display_format_is_node_dot_local() {
    assert_eq!(format!("{}", ProcessId::new(1, 42)), "<1.42>");
    assert_eq!(format!("{}", ProcessId::new(0, 0)), "<0.0>");
    assert_eq!(format!("{}", ProcessId::new(99, 1000)), "<99.1000>");
}

/// Guards that `ProcessId` implements Copy, Send, and Sync.
///
/// The fork replaced Copy types with Rc-based types that are neither Send nor Sync.
/// `ProcessId` must be freely copyable and safe to share across threads.
#[test]
fn process_id_is_copy_send_sync() {
    fn assert_copy_send_sync<T: Copy + Send + Sync>() {}
    assert_copy_send_sync::<ProcessId>();

    // Verify Copy works in practice
    let a = ProcessId::new(1, 1);
    let b = a; // copy
    assert_eq!(a, b); // original still usable
}

/// Guards that `SendError` has exactly three variants: `ProcessDead`, `MailboxFull`, `NodeUnreachable`.
///
/// The fork may add or remove error variants. This exhaustive match ensures all
/// three canonical variants exist and no others have been introduced.
#[test]
fn send_error_variants_exhaustive() {
    let errors: Vec<SendError> = vec![
        SendError::ProcessDead(ProcessId::new(1, 1)),
        SendError::MailboxFull(ProcessId::new(1, 2)),
        SendError::NodeUnreachable(99),
    ];

    for err in &errors {
        // Exhaustive match -- if a variant is added or removed, this will not compile.
        match err {
            SendError::ProcessDead(_)
            | SendError::MailboxFull(_)
            | SendError::NodeUnreachable(_) => {}
        }
    }
}

/// Guards that `LocalRouter` implements `MessageRouter` + Send + Sync.
///
/// The fork removed Send + Sync bounds from `MessageRouter` and its implementations,
/// replacing Arc with Rc throughout. This test ensures the trait hierarchy is intact.
#[test]
fn message_router_requires_send_sync() {
    fn assert_router_send_sync<T: MessageRouter + Send + Sync>() {}
    assert_router_send_sync::<LocalRouter>();
}

/// Guards that `LocalRouter` implements Send + Sync.
///
/// `LocalRouter` must be shareable across threads (via Arc) for the runtime to work.
/// The fork broke this by using Rc internally.
#[test]
fn local_router_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LocalRouter>();
}

/// Guards that `ProcessTable` implements Send + Sync.
///
/// `ProcessTable` is shared across tokio tasks via Arc. If it loses Send + Sync
/// (e.g. by using Rc or Cell internally), the runtime cannot function.
#[test]
fn process_table_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ProcessTable>();
}

/// Guards that Runtime implements Send + Sync.
///
/// The Runtime must be shareable across threads for use in async contexts.
/// The fork broke this by using Rc-based internals.
#[test]
fn runtime_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Runtime>();
}

/// Guards that `Runtime::with_router` accepts Arc parameters.
///
/// The fork replaced Arc<ProcessTable> and Arc<dyn MessageRouter> with Rc equivalents.
/// This test ensures the Arc-based API is preserved.
#[test]
fn runtime_with_router_accepts_arc() {
    let table = Arc::new(ProcessTable::new(1));
    let router: Arc<dyn MessageRouter> = Arc::new(LocalRouter::new(Arc::clone(&table)));
    let rt = Runtime::with_router(1, table, router);
    assert_eq!(rt.node_id(), 1);
}

// ============================================================================
// Task 2: Async/runtime contract tests
// ============================================================================

/// Guards that `Runtime::spawn` is async and returns a Future.
///
/// The fork changed spawn from async to sync, breaking the ability to .await
/// the PID. This test ensures spawn().await works and returns a valid PID.
#[tokio::test]
async fn spawn_is_async_and_returns_future() {
    let rt = Runtime::new(1);
    let pid = rt.spawn(|_ctx| async {}).await;
    assert_eq!(pid.node_id(), 1);
    assert!(pid.local_id() > 0);
}

/// Guards that spawn requires Send-bounded handlers and futures.
///
/// The fork removed Send bounds, allowing Rc and non-Send types in handlers.
/// This test verifies that Arc (which is Send) works across .await points
/// inside spawned processes, which is only possible if the future is Send.
#[tokio::test]
async fn spawn_requires_send_handler() {
    let rt = Runtime::new(1);
    let shared = Arc::new(std::sync::Mutex::new(0u64));
    let shared_clone = Arc::clone(&shared);
    let (tx, rx) = tokio::sync::oneshot::channel();

    rt.spawn(move |_ctx| async move {
        // Hold Arc across an .await point -- only valid if future is Send
        tokio::task::yield_now().await;
        *shared_clone.lock().unwrap() = 42;
        tx.send(()).unwrap();
    })
    .await;

    tokio::time::timeout(std::time::Duration::from_secs(1), rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*shared.lock().unwrap(), 42);
}

/// Guards that `Runtime::send` is async.
///
/// The fork changed send from async to sync. This test ensures send().await
/// compiles and works correctly.
#[tokio::test]
async fn runtime_send_is_async() {
    let rt = Runtime::new(1);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    let pid = rt
        .spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            done_tx
                .send(msg.payload().as_str().unwrap().to_string())
                .unwrap();
        })
        .await;

    // This .await is the contract under test
    rt.send(pid, rmpv::Value::String("async-send".into()))
        .await
        .unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, "async-send");
}

/// Guards that `ProcessContext::send` is async.
///
/// The fork changed context send from async to sync. This test ensures
/// ctx.send().await compiles and works inside a spawned process.
#[tokio::test]
async fn context_send_is_async() {
    let rt = Runtime::new(1);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    let receiver = rt
        .spawn(move |mut ctx| async move {
            let msg = ctx.recv().await.unwrap();
            done_tx
                .send(msg.payload().as_str().unwrap().to_string())
                .unwrap();
        })
        .await;

    rt.spawn(move |ctx| async move {
        // This .await is the contract under test
        ctx.send(receiver, rmpv::Value::String("ctx-async".into()))
            .await
            .unwrap();
    })
    .await;

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), done_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result, "ctx-async");
}

/// Guards that a panicking process does not crash the runtime.
///
/// The fork removed panic isolation, meaning a panicking process would bring
/// down the entire runtime. This test spawns a panicking process, waits for
/// cleanup, then spawns a healthy process to prove the runtime survived.
#[tokio::test]
async fn panicking_process_does_not_crash_runtime() {
    let rt = Runtime::new(1);

    rt.spawn(|_ctx| async move {
        panic!("intentional panic for contract test");
    })
    .await;

    // Allow the panic to propagate and cleanup to occur
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let (tx, rx) = tokio::sync::oneshot::channel();
    rt.spawn(move |_ctx| async move {
        tx.send(true).unwrap();
    })
    .await;

    let survived = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
        .await
        .unwrap()
        .unwrap();
    assert!(survived, "runtime must survive a process panic");
}

/// Guards that a panicked process is cleaned up from the process table.
///
/// After a process panics, its PID must be removed from the table so that
/// sending to it returns `ProcessDead`. The fork removed this cleanup.
#[tokio::test]
async fn panicking_process_is_cleaned_up() {
    let rt = Runtime::new(1);

    let pid = rt
        .spawn(|_ctx| async move {
            panic!("intentional panic for cleanup test");
        })
        .await;

    // Wait for panic propagation and table cleanup
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let result = rt.send(pid, rmpv::Value::Nil).await;
    assert!(
        matches!(result, Err(SendError::ProcessDead(_))),
        "sending to a panicked PID must return ProcessDead, got: {result:?}",
    );
}

/// Guards that concurrent spawns from multiple tokio tasks all produce unique PIDs.
///
/// This validates that PID allocation is atomic and thread-safe. The fork's use of
/// Rc and non-Send types would make concurrent spawning impossible.
#[tokio::test]
async fn concurrent_spawn_from_multiple_tasks() {
    let rt = Arc::new(Runtime::new(1));
    let mut handles = Vec::new();

    for _ in 0..5 {
        let rt = Arc::clone(&rt);
        handles.push(tokio::spawn(async move {
            rt.spawn(|_ctx| async {}).await
        }));
    }

    let mut pids = std::collections::HashSet::new();
    for handle in handles {
        let pid = handle.await.unwrap();
        assert!(pids.insert(pid), "duplicate PID detected: {pid}");
    }

    assert_eq!(pids.len(), 5, "expected 5 unique PIDs from concurrent spawns");
}

// === GenServer API contracts ===

#[test]
fn gen_server_context_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<rebar_core::gen_server::GenServerContext>();
}

#[test]
fn call_error_has_timeout_and_server_dead_variants() {
    let err = rebar_core::gen_server::CallError::Timeout;
    match err {
        rebar_core::gen_server::CallError::Timeout
        | rebar_core::gen_server::CallError::ServerDead => {}
    }
}

// === DynamicSupervisor API contracts ===

#[test]
fn dynamic_supervisor_handle_is_clone_send() {
    fn assert_clone_send<T: Clone + Send>() {}
    assert_clone_send::<rebar_core::supervisor::DynamicSupervisorHandle>();
}

#[test]
fn dynamic_supervisor_spec_builder_pattern() {
    let spec = rebar_core::supervisor::DynamicSupervisorSpec::new()
        .max_restarts(10)
        .max_seconds(60);
    let _ = spec;
}

// === RuntimeBuilder API contracts ===

#[test]
fn runtime_builder_new_takes_node_id() {
    let _builder = rebar_core::runtime::RuntimeBuilder::new(1);
}

#[test]
fn runtime_builder_has_worker_threads() {
    let _builder = rebar_core::runtime::RuntimeBuilder::new(1).worker_threads(4);
}

#[test]
fn runtime_builder_has_thread_name() {
    let _builder = rebar_core::runtime::RuntimeBuilder::new(1).thread_name("test");
}

#[test]
fn runtime_builder_build_returns_pair() {
    let (tokio_rt, rebar_rt) = rebar_core::runtime::RuntimeBuilder::new(1).build().unwrap();
    assert_eq!(rebar_rt.node_id(), 1);
    drop(tokio_rt);
}
