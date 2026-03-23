mod engine;
mod types;

pub use engine::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessId;
    use crate::runtime::Runtime;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Spawn a worker process that receives tasks (rmpv::Value),
    /// multiplies the integer payload by 2, and sends the result back
    /// to the `reply_to` PID encoded in the message.
    async fn spawn_doubler_worker(rt: &Runtime) -> ProcessId {
        rt.spawn(|mut ctx| async move {
            while let Some(msg) = ctx.recv().await {
                // Expect Map with "task", "reply_to_node", "reply_to_local"
                if let rmpv::Value::Map(entries) = msg.payload().clone() {
                    let mut task_val: i64 = 0;
                    let mut reply_node: u64 = 0;
                    let mut reply_local: u64 = 0;
                    for (k, v) in &entries {
                        match k.as_str().unwrap_or("") {
                            "task" => task_val = v.as_i64().unwrap_or(0),
                            "reply_to_node" => reply_node = v.as_u64().unwrap_or(0),
                            "reply_to_local" => reply_local = v.as_u64().unwrap_or(0),
                            _ => {}
                        }
                    }
                    let reply_pid = ProcessId::new(reply_node, reply_local);
                    let result = rmpv::Value::from(task_val * 2);
                    let _ = ctx.send(reply_pid, result).await;
                }
            }
        })
        .await
    }

    // ---------------------------------------------------------------
    // Registration
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn register_worker_increases_count() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;
        let w = spawn_doubler_worker(&rt).await;

        let id = coord.register_worker(w, rmpv::Value::Nil).await.unwrap();
        assert!(id.0 > 0);
        assert_eq!(coord.worker_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn register_multiple_workers() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;

        for _ in 0..5 {
            let w = spawn_doubler_worker(&rt).await;
            coord.register_worker(w, rmpv::Value::Nil).await.unwrap();
        }
        assert_eq!(coord.worker_count().await.unwrap(), 5);
    }

    #[tokio::test]
    async fn unregister_worker_decreases_count() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;
        let w = spawn_doubler_worker(&rt).await;

        let id = coord.register_worker(w, rmpv::Value::Nil).await.unwrap();
        coord.unregister_worker(id).await.unwrap();
        assert_eq!(coord.worker_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn unregister_unknown_worker_errors() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;
        let result = coord.unregister_worker(WorkerId(999)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_workers_returns_registered() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;

        let w1 = spawn_doubler_worker(&rt).await;
        let w2 = spawn_doubler_worker(&rt).await;
        coord
            .register_worker(w1, rmpv::Value::from("alpha"))
            .await
            .unwrap();
        coord
            .register_worker(w2, rmpv::Value::from("beta"))
            .await
            .unwrap();

        let workers = coord.list_workers().await.unwrap();
        assert_eq!(workers.len(), 2);
    }

    // ---------------------------------------------------------------
    // Task submission
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn submit_task_returns_result() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;
        let w = spawn_doubler_worker(&rt).await;
        coord.register_worker(w, rmpv::Value::Nil).await.unwrap();

        let result = coord
            .submit(rmpv::Value::from(21), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(result.as_i64().unwrap(), 42);
    }

    #[tokio::test]
    async fn submit_to_empty_pool_errors() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;

        let result = coord
            .submit(rmpv::Value::from(1), Duration::from_secs(1))
            .await;
        assert!(matches!(result, Err(CoordinatorError::NoWorkers)));
    }

    #[tokio::test]
    async fn submit_many_distributes_across_workers() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;

        for _ in 0..3 {
            let w = spawn_doubler_worker(&rt).await;
            coord.register_worker(w, rmpv::Value::Nil).await.unwrap();
        }

        let tasks: Vec<rmpv::Value> = (1..=6).map(|i| rmpv::Value::from(i)).collect();
        let results = coord.submit_many(tasks, Duration::from_secs(2)).await;

        let mut values: Vec<i64> = results
            .into_iter()
            .map(|r| r.unwrap().as_i64().unwrap())
            .collect();
        values.sort_unstable();
        assert_eq!(values, vec![2, 4, 6, 8, 10, 12]);
    }

    // ---------------------------------------------------------------
    // Least-loaded scheduling
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn least_loaded_prefers_idle_worker() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;

        let counter_fast = Arc::new(AtomicU64::new(0));
        let counter_slow = Arc::new(AtomicU64::new(0));

        // Fast worker: replies immediately
        let cf = counter_fast.clone();
        let fast = rt
            .spawn(move |mut ctx| async move {
                while let Some(msg) = ctx.recv().await {
                    cf.fetch_add(1, Ordering::SeqCst);
                    if let rmpv::Value::Map(entries) = msg.payload().clone() {
                        let mut rn: u64 = 0;
                        let mut rl: u64 = 0;
                        for (k, v) in &entries {
                            match k.as_str().unwrap_or("") {
                                "reply_to_node" => rn = v.as_u64().unwrap_or(0),
                                "reply_to_local" => rl = v.as_u64().unwrap_or(0),
                                _ => {}
                            }
                        }
                        let _ = ctx.send(ProcessId::new(rn, rl), rmpv::Value::from(1)).await;
                    }
                }
            })
            .await;

        // Slow worker: takes 200ms per task
        let cs = counter_slow.clone();
        let slow = rt
            .spawn(move |mut ctx| async move {
                while let Some(msg) = ctx.recv().await {
                    cs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if let rmpv::Value::Map(entries) = msg.payload().clone() {
                        let mut rn: u64 = 0;
                        let mut rl: u64 = 0;
                        for (k, v) in &entries {
                            match k.as_str().unwrap_or("") {
                                "reply_to_node" => rn = v.as_u64().unwrap_or(0),
                                "reply_to_local" => rl = v.as_u64().unwrap_or(0),
                                _ => {}
                            }
                        }
                        let _ = ctx.send(ProcessId::new(rn, rl), rmpv::Value::from(1)).await;
                    }
                }
            })
            .await;

        coord.register_worker(fast, rmpv::Value::Nil).await.unwrap();
        coord.register_worker(slow, rmpv::Value::Nil).await.unwrap();

        // Submit 6 tasks sequentially — fast worker should handle more
        // because its in-flight count drops back to 0 quickly
        for i in 0..6 {
            coord
                .submit(rmpv::Value::from(i), Duration::from_secs(2))
                .await
                .unwrap();
        }

        let fast_count = counter_fast.load(Ordering::SeqCst);
        let slow_count = counter_slow.load(Ordering::SeqCst);
        assert_eq!(fast_count + slow_count, 6);
        // The fast worker should have handled more tasks than the slow one
        assert!(
            fast_count > slow_count,
            "fast worker ({fast_count}) should handle more than slow ({slow_count})"
        );
    }

    #[tokio::test]
    async fn in_flight_visible_in_list_workers() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;

        // Worker that takes 500ms to respond
        let slow = rt
            .spawn(|mut ctx| async move {
                while let Some(msg) = ctx.recv().await {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if let rmpv::Value::Map(entries) = msg.payload().clone() {
                        let mut rn: u64 = 0;
                        let mut rl: u64 = 0;
                        for (k, v) in &entries {
                            match k.as_str().unwrap_or("") {
                                "reply_to_node" => rn = v.as_u64().unwrap_or(0),
                                "reply_to_local" => rl = v.as_u64().unwrap_or(0),
                                _ => {}
                            }
                        }
                        let _ = ctx.send(ProcessId::new(rn, rl), rmpv::Value::from(1)).await;
                    }
                }
            })
            .await;

        coord.register_worker(slow, rmpv::Value::Nil).await.unwrap();

        // Submit a task (don't await — let it be in-flight)
        let coord2 = coord.clone();
        tokio::spawn(async move {
            let _ = coord2
                .submit(rmpv::Value::from(1), Duration::from_secs(2))
                .await;
        });

        // Give the submit time to dispatch
        tokio::time::sleep(Duration::from_millis(50)).await;

        let workers = coord.list_workers().await.unwrap();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].in_flight, 1, "should show 1 in-flight task");
    }

    // ---------------------------------------------------------------
    // Dead worker removal
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn dead_worker_removed_on_submit() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;

        // Spawn a worker that exits immediately
        let dead_worker = rt.spawn(|_ctx| async {}).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        coord
            .register_worker(dead_worker, rmpv::Value::Nil)
            .await
            .unwrap();

        // Also register a live worker
        let live_worker = spawn_doubler_worker(&rt).await;
        coord
            .register_worker(live_worker, rmpv::Value::Nil)
            .await
            .unwrap();

        // Submit should succeed (routes to live worker after dead one fails)
        let result = coord
            .submit(rmpv::Value::from(5), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(result.as_i64().unwrap(), 10);

        // Dead worker should have been removed
        assert_eq!(coord.worker_count().await.unwrap(), 1);
    }

    // ---------------------------------------------------------------
    // Shutdown
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn shutdown_stops_coordinator() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;
        coord.shutdown();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = coord
            .submit(rmpv::Value::from(1), Duration::from_millis(100))
            .await;
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // CoordinatorSpec
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn coordinator_pid_is_valid() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;
        assert_eq!(coord.pid().node_id(), 1);
    }

    #[tokio::test]
    async fn existing_runtime_features_unchanged() {
        // Verify basic spawn/send still works
        let rt = Runtime::new(1);
        let (tx, rx) = tokio::sync::oneshot::channel();
        rt.spawn(move |_ctx| async move {
            tx.send(42_u64).unwrap();
        })
        .await;
        let val = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(val, 42);
    }
}
