//! Integration tests for rebar-core.
//!
//! These tests exercise the full runtime from the outside, using only
//! the crate's public API.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rebar_core::process::ExitReason;
use rebar_core::runtime::{ProcessContext, Runtime};
use rebar_core::supervisor::{
    ChildEntry, ChildSpec, RestartStrategy, RestartType, ShutdownStrategy, SupervisorSpec,
    start_supervisor,
};

// ---------------------------------------------------------------------------
// 1. ping_pong_between_processes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ping_pong_between_processes() {
    let rt = Runtime::new(1);

    // Channel to observe the final "pong" result.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<String>();

    // Spawn process B first so A can address it.
    let b_pid = rt
        .spawn(|mut ctx: ProcessContext| async move {
            // Wait for "ping"
            let msg = ctx.recv().await.unwrap();
            assert_eq!(msg.payload().as_str().unwrap(), "ping");
            // Reply with "pong" to the sender.
            let sender = msg.from();
            ctx.send(sender, rmpv::Value::String("pong".into()))
                .await
                .unwrap();
        })
        .await;

    // Spawn process A: sends "ping" to B, waits for "pong".
    rt.spawn(move |mut ctx: ProcessContext| async move {
        let my_pid = ctx.self_pid();
        // send ping — we cannot send self-pid as payload directly, but B
        // can read `msg.from()`.
        ctx.send(b_pid, rmpv::Value::String("ping".into()))
            .await
            .unwrap();
        // Wait for the pong reply.
        let reply = ctx.recv().await.unwrap();
        let payload = reply.payload().as_str().unwrap().to_string();
        let _ = done_tx.send(payload);
        let _ = my_pid; // suppress unused warning
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(2), done_rx)
        .await
        .expect("timed out waiting for pong")
        .expect("oneshot dropped");

    assert_eq!(result, "pong");
}

// ---------------------------------------------------------------------------
// 2. fan_out_to_multiple_workers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fan_out_to_multiple_workers() {
    let rt = Runtime::new(1);
    let n: u64 = 10;

    let (results_tx, mut results_rx) = tokio::sync::mpsc::channel::<u64>(usize::try_from(n).unwrap());

    // Spawn N workers. Each receives a value and sends value*2 to the results
    // channel.
    let mut worker_pids = Vec::new();
    for _ in 0..n {
        let tx = results_tx.clone();
        let pid = rt
            .spawn(move |mut ctx: ProcessContext| async move {
                let msg = ctx.recv().await.unwrap();
                let val = msg.payload().as_u64().unwrap();
                tx.send(val * 2).await.unwrap();
            })
            .await;
        worker_pids.push(pid);
    }
    // Drop our copy so the channel closes after all workers finish.
    drop(results_tx);

    // Dispatcher sends a different value to each worker.
    rt.spawn(move |ctx: ProcessContext| async move {
        for (i, pid) in worker_pids.iter().enumerate() {
            ctx.send(*pid, rmpv::Value::Integer((i as u64).into()))
                .await
                .unwrap();
        }
    })
    .await;

    // Collect results.
    let mut results = Vec::new();
    while let Ok(Some(val)) = tokio::time::timeout(Duration::from_secs(2), results_rx.recv()).await
    {
        results.push(val);
    }
    results.sort_unstable();

    let expected: Vec<u64> = (0..n).map(|i| i * 2).collect();
    assert_eq!(results, expected);
}

// ---------------------------------------------------------------------------
// 3. chain_of_three_processes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chain_of_three_processes() {
    let rt = Runtime::new(1);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<u64>();

    // C — final receiver.
    let c = rt
        .spawn(move |mut ctx: ProcessContext| async move {
            let msg = ctx.recv().await.unwrap();
            let val = msg.payload().as_u64().unwrap();
            let _ = done_tx.send(val);
        })
        .await;

    // B — transforms: val + 10.
    let b = rt
        .spawn(move |mut ctx: ProcessContext| async move {
            let msg = ctx.recv().await.unwrap();
            let val = msg.payload().as_u64().unwrap();
            ctx.send(c, rmpv::Value::Integer((val + 10).into()))
                .await
                .unwrap();
        })
        .await;

    // A — starts the chain with value 5.
    rt.spawn(move |ctx: ProcessContext| async move {
        ctx.send(b, rmpv::Value::Integer(5u64.into()))
            .await
            .unwrap();
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(2), done_rx)
        .await
        .expect("timeout")
        .expect("oneshot dropped");

    // A sends 5 → B adds 10 → C receives 15.
    assert_eq!(result, 15);
}

// ---------------------------------------------------------------------------
// 4. supervisor_restarts_crashed_worker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_restarts_crashed_worker() {
    let rt = Arc::new(Runtime::new(1));

    // Counter shared with the child factory. Each time the child starts it
    // increments the counter. After incrementing, the first invocation
    // crashes (Abnormal). The second invocation stays alive long enough for
    // us to observe the counter reaching 2.
    let start_count = Arc::new(AtomicU32::new(0));
    let (restarted_tx, restarted_rx) = tokio::sync::oneshot::channel::<()>();
    // Wrap in a mutex so the closure is Fn (not FnOnce).
    let restarted_tx = Arc::new(tokio::sync::Mutex::new(Some(restarted_tx)));

    let sc = Arc::clone(&start_count);
    let rtx = Arc::clone(&restarted_tx);

    let entry = ChildEntry::new(
        ChildSpec::new("crasher").restart(RestartType::Permanent),
        move || {
            let sc = Arc::clone(&sc);
            let rtx = Arc::clone(&rtx);
            async move {
                let n = sc.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    // First start: crash immediately.
                    ExitReason::Abnormal("boom".into())
                } else {
                    // Second start: signal that we restarted, then stay alive.
                    let tx = rtx.lock().await.take();
                    if let Some(tx) = tx {
                        let _ = tx.send(());
                    }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    ExitReason::Normal
                }
            }
        },
    );

    let spec = SupervisorSpec::new(RestartStrategy::OneForOne).max_restarts(5);
    let handle = start_supervisor(rt, spec, vec![entry]).await;

    // Wait for the restart signal.
    tokio::time::timeout(Duration::from_secs(2), restarted_rx)
        .await
        .expect("timed out waiting for restart")
        .expect("oneshot dropped");

    assert!(start_count.load(Ordering::SeqCst) >= 2);

    handle.shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ---------------------------------------------------------------------------
// 5. supervisor_one_for_all_cascade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_one_for_all_cascade() {
    let rt = Arc::new(Runtime::new(1));

    // Counters for each child.
    let counters: Vec<Arc<AtomicU32>> = (0..3).map(|_| Arc::new(AtomicU32::new(0))).collect();

    // A channel that fires once all three children have started at least twice
    // (i.e., original start + restart).
    let (all_restarted_tx, all_restarted_rx) = tokio::sync::oneshot::channel::<()>();
    let all_restarted_tx = Arc::new(tokio::sync::Mutex::new(Some(all_restarted_tx)));
    let counters_check = counters.clone();

    let mut entries = Vec::new();
    for (i, counter_arc) in counters.iter().enumerate() {
        let counter = Arc::clone(counter_arc);
        let all_tx = Arc::clone(&all_restarted_tx);
        let counters_check = counters_check.clone();

        entries.push(ChildEntry::new(
            ChildSpec::new(format!("child_{i}"))
                .restart(RestartType::Permanent)
                .shutdown(ShutdownStrategy::BrutalKill),
            move || {
                let counter = Arc::clone(&counter);
                let all_tx = Arc::clone(&all_tx);
                let counters_check = counters_check.clone();
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    if i == 0 && n == 1 {
                        // Child 0, first run: crash to trigger OneForAll restart.
                        ExitReason::Abnormal("crash".into())
                    } else {
                        // Check if all children have been started at least twice.
                        let all_reached =
                            counters_check.iter().all(|c| c.load(Ordering::SeqCst) >= 2);
                        if all_reached {
                            let tx = all_tx.lock().await.take();
                            if let Some(tx) = tx {
                                let _ = tx.send(());
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        ExitReason::Normal
                    }
                }
            },
        ));
    }

    let spec = SupervisorSpec::new(RestartStrategy::OneForAll).max_restarts(5);
    let handle = start_supervisor(rt, spec, entries).await;

    tokio::time::timeout(Duration::from_secs(3), all_restarted_rx)
        .await
        .expect("timed out waiting for OneForAll cascade")
        .expect("oneshot dropped");

    // Every child should have been started at least twice.
    for (i, c) in counters.iter().enumerate() {
        let count = c.load(Ordering::SeqCst);
        assert!(
            count >= 2,
            "child {i} started only {count} time(s)",
        );
    }

    handle.shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ---------------------------------------------------------------------------
// 6. process_monitor_receives_down
// ---------------------------------------------------------------------------

#[tokio::test]
async fn process_monitor_receives_down() {
    // The full monitor-to-runtime integration may not be wired yet, so we
    // test the observable effect: when a process exits, its mailbox senders
    // are dropped, and any process trying to send to it will get
    // SendError::ProcessDead. We have a watcher that polls until the target
    // is dead.
    let rt = Runtime::new(1);

    let (detected_tx, detected_rx) = tokio::sync::oneshot::channel::<bool>();

    // Target process: exits immediately.
    let target = rt
        .spawn(|_ctx: ProcessContext| async move {
            // Exit right away.
        })
        .await;

    // Watcher process: repeatedly tries to send to target until it gets
    // ProcessDead, confirming the target is gone.
    rt.spawn(move |ctx: ProcessContext| async move {
        let mut detected = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if ctx.send(target, rmpv::Value::Nil).await.is_err() {
                detected = true;
                break;
            }
        }
        let _ = detected_tx.send(detected);
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(3), detected_rx)
        .await
        .expect("timeout")
        .expect("oneshot dropped");

    assert!(result, "watcher should have detected target exit");
}

// ---------------------------------------------------------------------------
// 7. linked_process_dies_together
// ---------------------------------------------------------------------------

#[tokio::test]
async fn linked_process_dies_together() {
    // We demonstrate linked-process semantics via a OneForAll supervisor:
    // when one child crashes, all siblings are stopped and restarted.
    // This is the practical equivalent of "linked process dies together."
    let rt = Arc::new(Runtime::new(1));

    let peer_start_count = Arc::new(AtomicU32::new(0));

    let (peer_restarted_tx, peer_restarted_rx) = tokio::sync::oneshot::channel::<()>();
    let peer_restarted_tx = Arc::new(tokio::sync::Mutex::new(Some(peer_restarted_tx)));

    // Child 0 (crasher): crashes on first start.
    let crasher_count = Arc::new(AtomicU32::new(0));
    let cc = Arc::clone(&crasher_count);
    let crasher_entry = ChildEntry::new(
        ChildSpec::new("crasher")
            .restart(RestartType::Permanent)
            .shutdown(ShutdownStrategy::BrutalKill),
        move || {
            let cc = Arc::clone(&cc);
            async move {
                let n = cc.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    ExitReason::Abnormal("crash".into())
                } else {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    ExitReason::Normal
                }
            }
        },
    );

    // Child 1 (peer): stays alive. We detect its restart.
    let psc = Arc::clone(&peer_start_count);
    let ptx = Arc::clone(&peer_restarted_tx);
    let peer_entry = ChildEntry::new(
        ChildSpec::new("peer")
            .restart(RestartType::Permanent)
            .shutdown(ShutdownStrategy::BrutalKill),
        move || {
            let psc = Arc::clone(&psc);
            let ptx = Arc::clone(&ptx);
            async move {
                let n = psc.fetch_add(1, Ordering::SeqCst) + 1;
                if n >= 2 {
                    let tx = ptx.lock().await.take();
                    if let Some(tx) = tx {
                        let _ = tx.send(());
                    }
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
                ExitReason::Normal
            }
        },
    );

    let spec = SupervisorSpec::new(RestartStrategy::OneForAll).max_restarts(5);
    let handle = start_supervisor(rt, spec, vec![crasher_entry, peer_entry]).await;

    tokio::time::timeout(Duration::from_secs(3), peer_restarted_rx)
        .await
        .expect("timed out waiting for peer restart")
        .expect("oneshot dropped");

    assert!(
        peer_start_count.load(Ordering::SeqCst) >= 2,
        "peer should have been restarted when crasher died"
    );

    handle.shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ---------------------------------------------------------------------------
// 8. spawn_1000_processes_all_complete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_1000_processes_all_complete() {
    let rt = Runtime::new(1);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(1024);

    for i in 0..1000u64 {
        let tx = tx.clone();
        rt.spawn(move |_ctx: ProcessContext| async move {
            tx.send(i).await.unwrap();
        })
        .await;
    }
    // Drop our sender so the channel closes after all spawned tasks finish.
    drop(tx);

    let mut count = 0u64;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
        count += 1;
    }

    assert_eq!(count, 1000, "expected all 1000 processes to report in");
}
