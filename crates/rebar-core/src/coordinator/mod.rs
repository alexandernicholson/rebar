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
    // Round-robin scheduling
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn round_robin_distributes_evenly() {
        let rt = Arc::new(Runtime::new(1));
        let coord = start_coordinator(Arc::clone(&rt), CoordinatorSpec::default()).await;

        // Spawn 2 workers that count tasks received
        let counter1 = Arc::new(AtomicU64::new(0));
        let counter2 = Arc::new(AtomicU64::new(0));

        let c1 = counter1.clone();
        let w1 = rt
            .spawn(move |mut ctx| async move {
                while let Some(msg) = ctx.recv().await {
                    c1.fetch_add(1, Ordering::SeqCst);
                    if let rmpv::Value::Map(entries) = msg.payload().clone() {
                        let mut reply_node: u64 = 0;
                        let mut reply_local: u64 = 0;
                        for (k, v) in &entries {
                            match k.as_str().unwrap_or("") {
                                "reply_to_node" => reply_node = v.as_u64().unwrap_or(0),
                                "reply_to_local" => reply_local = v.as_u64().unwrap_or(0),
                                _ => {}
                            }
                        }
                        let reply_pid = ProcessId::new(reply_node, reply_local);
                        let _ = ctx.send(reply_pid, rmpv::Value::from(1)).await;
                    }
                }
            })
            .await;

        let c2 = counter2.clone();
        let w2 = rt
            .spawn(move |mut ctx| async move {
                while let Some(msg) = ctx.recv().await {
                    c2.fetch_add(1, Ordering::SeqCst);
                    if let rmpv::Value::Map(entries) = msg.payload().clone() {
                        let mut reply_node: u64 = 0;
                        let mut reply_local: u64 = 0;
                        for (k, v) in &entries {
                            match k.as_str().unwrap_or("") {
                                "reply_to_node" => reply_node = v.as_u64().unwrap_or(0),
                                "reply_to_local" => reply_local = v.as_u64().unwrap_or(0),
                                _ => {}
                            }
                        }
                        let reply_pid = ProcessId::new(reply_node, reply_local);
                        let _ = ctx.send(reply_pid, rmpv::Value::from(1)).await;
                    }
                }
            })
            .await;

        coord.register_worker(w1, rmpv::Value::Nil).await.unwrap();
        coord.register_worker(w2, rmpv::Value::Nil).await.unwrap();

        // Submit 10 tasks
        for i in 0..10 {
            coord
                .submit(rmpv::Value::from(i), Duration::from_secs(1))
                .await
                .unwrap();
        }

        let c1_val = counter1.load(Ordering::SeqCst);
        let c2_val = counter2.load(Ordering::SeqCst);
        assert_eq!(c1_val + c2_val, 10);
        assert_eq!(c1_val, 5);
        assert_eq!(c2_val, 5);
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
