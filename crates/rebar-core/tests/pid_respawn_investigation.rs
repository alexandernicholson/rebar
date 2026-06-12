//! Regression tests for the "stale PID after kill + respawn" bug.
//!
//! Originally these tests documented the broken behavior: a killed-and-
//! respawned process got a fresh PID and holders of the old PID were
//! permanently cut off, supervisor children had synthetic unroutable PIDs,
//! and there was no name registry, no DOWN notification, and no pg cleanup.
//!
//! They now assert the fixed behavior:
//! - stale PIDs still fail cleanly (PIDs are never recycled), but
//! - clients recover via the name registry (`register`/`whereis`/`send_named`),
//! - supervised children are real processes with routable PIDs,
//! - registered children are re-registered on every supervisor restart,
//! - monitors deliver `DOWN` messages when a watched process dies,
//! - pg scopes attached to a runtime drop dead PIDs automatically.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rebar_core::pg::PgScope;
use rebar_core::process::monitor::DownMessage;
use rebar_core::process::{ExitReason, ProcessId, SendError};
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::{
    ChildEntry, ChildSpec, RestartStrategy, RestartType, SupervisorSpec, start_supervisor,
};

/// Spawn a long-lived "service" process that registers itself under `name`,
/// echoes payloads to `out_tx` (tagged with its incarnation number), and
/// exits when `kill_rx` fires.
async fn spawn_named_service(
    rt: &Runtime,
    name: &'static str,
    incarnation: u32,
    out_tx: tokio::sync::mpsc::UnboundedSender<(u32, String)>,
    kill_rx: tokio::sync::oneshot::Receiver<()>,
) -> ProcessId {
    rt.spawn(move |mut ctx| async move {
        ctx.register(name).expect("name registration succeeds");
        tokio::pin!(kill_rx);
        loop {
            tokio::select! {
                msg = ctx.recv() => {
                    let Some(msg) = msg else { break };
                    let text = msg.payload().as_str().unwrap_or("").to_string();
                    let _ = out_tx.send((incarnation, text));
                }
                _ = &mut kill_rx => break, // simulated kill
            }
        }
    })
    .await
}

/// Wait until `name` resolves to `pid` (registration happens inside the
/// spawned process, asynchronously to the spawner).
async fn wait_until_registered(rt: &Runtime, name: &str, pid: ProcessId) {
    for _ in 0..1000 {
        if rt.whereis(name) == Some(pid) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{name} never registered to {pid:?}");
}

/// Wait until `pid` is no longer present in the process table.
async fn wait_until_dead(rt: &Runtime, pid: ProcessId) {
    for _ in 0..1000 {
        if rt.table().get(&pid).is_none() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("process {pid:?} never left the table");
}

// ---------------------------------------------------------------------------
// Scenario 1: manual kill + respawn — the originally reported bug.
// The stale PID still fails (PIDs are immutable identities), but the client
// recovers by resolving the registered name to the new incarnation.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn stale_pid_fails_but_name_lookup_recovers() {
    let rt = Runtime::new(1);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();

    // Incarnation 1
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();
    let pid_v1 = spawn_named_service(&rt, "echo", 1, out_tx.clone(), kill_rx).await;
    wait_until_registered(&rt, "echo", pid_v1).await;

    rt.send_named("echo", rmpv::Value::String("hello".into()))
        .await
        .expect("send to live v1 by name succeeds");
    let (inc, text) = out_rx.recv().await.unwrap();
    assert_eq!((inc, text.as_str()), (1, "hello"));
    assert_eq!(rt.whereis("echo"), Some(pid_v1));

    // Kill v1; its name is unregistered automatically on exit.
    kill_tx.send(()).unwrap();
    wait_until_dead(&rt, pid_v1).await;

    // Respawn as v2 under the same name.
    let (_kill_tx2, kill_rx2) = tokio::sync::oneshot::channel();
    let pid_v2 = spawn_named_service(&rt, "echo", 2, out_tx.clone(), kill_rx2).await;
    assert_ne!(pid_v1, pid_v2, "respawn allocates a fresh PID");

    // The stale PID fails cleanly — that part is by design.
    let err = rt.send(pid_v1, rmpv::Value::Nil).await.unwrap_err();
    assert!(matches!(err, SendError::ProcessDead(p) if p == pid_v1));

    // FIX: the client recovers by name instead of a cached PID.
    // (Poll: v2 registers itself asynchronously after spawn returns.)
    wait_until_registered(&rt, "echo", pid_v2).await;
    rt.send_named("echo", rmpv::Value::String("via name".into()))
        .await
        .expect("name resolves to the new incarnation");

    let (inc, text) = out_rx.recv().await.unwrap();
    assert_eq!((inc, text.as_str()), (2, "via name"));
}

// ---------------------------------------------------------------------------
// Scenario 2: PIDs are never reused. A stale PID fails cleanly forever
// (ProcessDead) rather than being misdelivered to an unrelated new process.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn stale_pid_is_never_recycled_to_another_process() {
    let rt = Runtime::new(1);

    let dead_pid = rt.spawn(|_ctx| async {}).await;
    wait_until_dead(&rt, dead_pid).await;

    for _ in 0..50 {
        let pid = rt
            .spawn(|mut ctx| async move {
                while ctx.recv().await.is_some() {}
            })
            .await;
        assert_ne!(pid, dead_pid, "PID must not be reused");
    }

    let err = rt.send(dead_pid, rmpv::Value::Nil).await.unwrap_err();
    assert!(matches!(err, SendError::ProcessDead(p) if p == dead_pid));
}

// ---------------------------------------------------------------------------
// Scenario 3: supervised children are real runtime processes. The PID from
// add_child() belongs to this node, lives in the process table, and routes
// messages to the child's mailbox.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn supervisor_child_pid_is_routable_while_alive() {
    let rt = Arc::new(Runtime::new(1));

    let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
        .max_restarts(5)
        .max_seconds(10);
    let handle = start_supervisor(Arc::clone(&rt), spec, vec![]).await;

    let (echo_tx, mut echo_rx) = tokio::sync::mpsc::unbounded_channel();
    let entry = ChildEntry::with_context(ChildSpec::new("worker"), move |mut ctx| {
        let echo_tx = echo_tx.clone();
        async move {
            while let Some(msg) = ctx.recv().await {
                let _ = echo_tx.send(msg.payload().as_str().unwrap_or("").to_string());
            }
            ExitReason::Normal
        }
    });

    let child_pid = handle.add_child(entry).await.expect("add_child succeeds");

    // Real PID: this node's ID, present in the process table.
    assert_eq!(child_pid.node_id(), rt.node_id());
    assert!(rt.table().get(&child_pid).is_some());

    // And it routes.
    rt.send(child_pid, rmpv::Value::String("ping".into()))
        .await
        .expect("send to supervised child succeeds");
    let echoed = tokio::time::timeout(Duration::from_secs(2), echo_rx.recv())
        .await
        .expect("child echoed")
        .unwrap();
    assert_eq!(echoed, "ping");

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Scenario 4: supervisor restart. The original PID dies with the first
// incarnation, but a `.registered()` child re-registers its spec id on every
// restart, so clients discover and reach the new incarnation by name.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn restarted_child_reachable_via_registered_name() {
    let rt = Arc::new(Runtime::new(1));

    let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
        .max_restarts(5)
        .max_seconds(10);
    let handle = start_supervisor(Arc::clone(&rt), spec, vec![]).await;

    let starts = Arc::new(AtomicU32::new(0));
    let (up_tx, mut up_rx) = tokio::sync::mpsc::unbounded_channel();
    let (echo_tx, mut echo_rx) = tokio::sync::mpsc::unbounded_channel();

    let sc = Arc::clone(&starts);
    let entry = ChildEntry::with_context(
        ChildSpec::new("svc")
            .restart(RestartType::Permanent)
            .registered(),
        move |mut ctx| {
            let n = sc.fetch_add(1, Ordering::SeqCst);
            let up_tx = up_tx.clone();
            let echo_tx = echo_tx.clone();
            async move {
                let _ = up_tx.send(n);
                if n == 0 {
                    // First incarnation: crash immediately.
                    ExitReason::Abnormal("boom".into())
                } else {
                    while let Some(msg) = ctx.recv().await {
                        let _ =
                            echo_tx.send((n, msg.payload().as_str().unwrap_or("").to_string()));
                    }
                    ExitReason::Normal
                }
            }
        },
    );

    let original_pid = handle.add_child(entry).await.expect("add_child succeeds");

    // Wait until incarnation 2 is up (it registers before its task runs).
    loop {
        let n = tokio::time::timeout(Duration::from_secs(2), up_rx.recv())
            .await
            .expect("child (re)started")
            .unwrap();
        if n >= 1 {
            break;
        }
    }

    // The cached PID is dead — but the name now resolves to the new one.
    let err = rt.send(original_pid, rmpv::Value::Nil).await.unwrap_err();
    assert!(matches!(err, SendError::ProcessDead(p) if p == original_pid));

    let current = rt.whereis("svc").expect("name re-registered on restart");
    assert_ne!(current, original_pid);

    rt.send_named("svc", rmpv::Value::String("still here?".into()))
        .await
        .expect("name-based send reaches restarted child");
    let (inc, text) = tokio::time::timeout(Duration::from_secs(2), echo_rx.recv())
        .await
        .expect("restarted child echoed")
        .unwrap();
    assert!(inc >= 1);
    assert_eq!(text, "still here?");

    handle.shutdown();
}

// ---------------------------------------------------------------------------
// Scenario 5: monitors. A watcher gets a DOWN message when the watched
// process exits, and an immediate "noproc" DOWN when monitoring a PID that
// is already dead — so clients can react to deaths instead of discovering
// them via send failures.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn monitor_delivers_down_when_target_exits() {
    let rt = Runtime::new(1);

    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    let target = rt
        .spawn(move |_ctx| async move {
            let _ = kill_rx.await;
        })
        .await;

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (down_tx, down_rx) = tokio::sync::oneshot::channel();
    rt.spawn(move |mut ctx| async move {
        let mref = ctx.monitor(target);
        ready_tx.send(()).unwrap();
        let msg = ctx.recv().await.unwrap();
        let down = DownMessage::from_value(msg.payload()).expect("payload is a DOWN message");
        down_tx.send((mref, down, msg.from())).unwrap();
    })
    .await;

    // Only kill the target after the monitor is in place.
    ready_rx.await.unwrap();
    kill_tx.send(()).unwrap();

    let (mref, down, from) = tokio::time::timeout(Duration::from_secs(2), down_rx)
        .await
        .expect("DOWN delivered")
        .unwrap();
    assert_eq!(down.monitor_ref, mref);
    assert_eq!(down.pid, target);
    assert_eq!(down.reason, "exit");
    assert_eq!(from, target, "DOWN message is sent from the dead PID");
}

#[tokio::test]
async fn monitor_of_dead_pid_delivers_noproc_immediately() {
    let rt = Runtime::new(1);
    let never_alive = ProcessId::new(1, 999_999);

    let (down_tx, down_rx) = tokio::sync::oneshot::channel();
    rt.spawn(move |mut ctx| async move {
        let mref = ctx.monitor(never_alive);
        let msg = ctx.recv().await.unwrap();
        let down = DownMessage::from_value(msg.payload()).expect("payload is a DOWN message");
        down_tx.send((mref, down)).unwrap();
    })
    .await;

    let (mref, down) = tokio::time::timeout(Duration::from_secs(2), down_rx)
        .await
        .expect("noproc DOWN delivered")
        .unwrap();
    assert_eq!(down.monitor_ref, mref);
    assert_eq!(down.pid, never_alive);
    assert_eq!(down.reason, "noproc");
}

// ---------------------------------------------------------------------------
// Scenario 6: pg groups. A scope attached to the runtime drops dead PIDs
// automatically, so group lookups never hand out stale PIDs after a member
// dies and is respawned.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn attached_pg_scope_drops_dead_pids() {
    let rt = Runtime::new(1);
    let scope = PgScope::new();
    rt.attach_pg_scope(&scope);

    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    let pid_v1 = rt
        .spawn(move |_ctx| async move {
            let _ = kill_rx.await;
        })
        .await;
    scope.join("my-service", pid_v1);

    kill_tx.send(()).unwrap();
    wait_until_dead(&rt, pid_v1).await;

    // Respawn v2 and join the group.
    let pid_v2 = rt
        .spawn(|mut ctx| async move {
            while ctx.recv().await.is_some() {}
        })
        .await;
    scope.join("my-service", pid_v2);

    // The exit hook runs during cleanup; poll briefly for it.
    let mut members = scope.get_members("my-service");
    for _ in 0..1000 {
        if !members.contains(&pid_v1) {
            break;
        }
        tokio::task::yield_now().await;
        members = scope.get_members("my-service");
    }
    assert!(
        !members.contains(&pid_v1),
        "dead PID removed from pg group: {members:?}"
    );
    assert!(members.contains(&pid_v2), "live member retained");
}
