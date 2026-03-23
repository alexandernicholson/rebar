use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;
use rebar_core::supervisor::{
    start_dynamic_supervisor, ChildEntry, ChildSpec, DynamicSupervisorSpec, RestartType,
};

use rebar_core::process::ProcessId;

#[tokio::test]
async fn dynamic_supervisor_manages_many_children() {
    let rt = Arc::new(Runtime::new(1));
    let handle = start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

    // Start 10 children
    for i in 0..10 {
        let entry = ChildEntry::new(
            ChildSpec::new(format!("worker-{i}")),
            || async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                ExitReason::Normal
            },
        );
        handle.start_child(entry).await.unwrap();
    }

    let counts = handle.count_children().await.unwrap();
    assert_eq!(counts.active, 10);
    assert_eq!(counts.specs, 10);

    // Collect PIDs to terminate 5
    let children = handle.which_children().await.unwrap();
    let pids_to_terminate: Vec<ProcessId> = children
        .iter()
        .filter_map(|c| c.pid)
        .take(5)
        .collect();

    for pid in pids_to_terminate {
        handle.terminate_child(pid).await.unwrap();
    }

    // count_children is a synchronous query on the supervisor
    let counts = handle.count_children().await.unwrap();
    assert_eq!(counts.active, 5);

    handle.shutdown();
}

#[tokio::test]
async fn terminate_nonexistent_child_returns_error() {
    let rt = Arc::new(Runtime::new(1));
    let handle = start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

    let bogus_pid = ProcessId::new(0, 99999);
    let result = handle.terminate_child(bogus_pid).await;
    assert!(result.is_err());

    handle.shutdown();
}

#[tokio::test]
async fn remove_running_child_returns_error() {
    let rt = Arc::new(Runtime::new(1));
    let handle = start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

    let entry = ChildEntry::new(ChildSpec::new("worker"), || async {
        std::future::pending::<()>().await;
        #[allow(unreachable_code)]
        ExitReason::Normal
    });
    let pid = handle.start_child(entry).await.unwrap();

    // Try to remove without terminating first — should fail
    let result = handle.remove_child(pid).await;
    assert!(result.is_err());

    handle.shutdown();
}

#[tokio::test]
async fn transient_child_restarted_on_abnormal_exit() {
    let rt = Arc::new(Runtime::new(1));
    let spec = DynamicSupervisorSpec::new()
        .max_restarts(10)
        .max_seconds(5);
    let handle = start_dynamic_supervisor(rt, spec).await;

    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();

    let entry = ChildEntry {
        spec: ChildSpec::new("crasher").restart(RestartType::Transient),
        factory: Arc::new(move || {
            let c = counter_clone.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                ExitReason::Abnormal("crash".into())
            })
        }),
    };

    handle.start_child(entry).await.unwrap();

    // Poll until at least 2 starts (original + restart)
    for _ in 0..200 {
        if counter.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }

    let start_count = counter.load(Ordering::SeqCst);
    assert!(
        start_count >= 2,
        "expected at least 2 starts (original + restart), got {start_count}",
    );

    handle.shutdown();
}

#[tokio::test]
async fn transient_child_not_restarted_on_normal_exit() {
    let rt = Arc::new(Runtime::new(1));
    let handle = start_dynamic_supervisor(rt, DynamicSupervisorSpec::new()).await;

    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();

    let entry = ChildEntry {
        spec: ChildSpec::new("normal-exit").restart(RestartType::Transient),
        factory: Arc::new(move || {
            let c = counter_clone.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                ExitReason::Normal
            })
        }),
    };

    handle.start_child(entry).await.unwrap();

    // Yield several times to give the supervisor a chance to (incorrectly) restart
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }

    let start_count = counter.load(Ordering::SeqCst);
    assert_eq!(
        start_count, 1,
        "transient child should NOT restart on normal exit, got {start_count} starts",
    );

    handle.shutdown();
}
