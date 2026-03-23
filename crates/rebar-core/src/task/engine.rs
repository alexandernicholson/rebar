use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::process::ProcessId;
use crate::runtime::Runtime;

use super::{StreamOpts, Task, TaskError};

/// Spawn a task and get a handle to await its result.
///
/// The task runs as a full process in the runtime (has a PID, is in the
/// process table). Equivalent to Elixir's `Task.async/1`.
pub async fn async_task<F, Fut, T>(runtime: &Runtime, f: F) -> Task<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (result_tx, result_rx) = oneshot::channel();

    let pid = runtime
        .spawn(move |_ctx| async move {
            let result = f().await;
            let _ = result_tx.send(result);
        })
        .await;

    Task {
        pid,
        result_rx: Some(result_rx),
        join_handle: None,
    }
}

/// Spawn a task with access to a [`ProcessContext`](crate::runtime::ProcessContext).
///
/// Useful when the task needs to send/receive messages.
pub async fn async_task_ctx<F, Fut, T>(runtime: &Runtime, f: F) -> Task<T>
where
    F: FnOnce(crate::runtime::ProcessContext) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (result_tx, result_rx) = oneshot::channel();

    let pid = runtime
        .spawn(move |ctx| async move {
            let result = f(ctx).await;
            let _ = result_tx.send(result);
        })
        .await;

    Task {
        pid,
        result_rx: Some(result_rx),
        join_handle: None,
    }
}

impl<T: Send + 'static> Task<T> {
    /// Block until the task completes and return its result.
    ///
    /// Consumes the result receiver — can only be called once.
    /// Equivalent to Elixir's `Task.await/2`.
    ///
    /// # Errors
    ///
    /// Returns `TaskError::ProcessDead` if the task panicked or was already consumed.
    /// Returns `TaskError::Timeout` if the task didn't complete within the duration.
    pub async fn await_result(&mut self, timeout: Duration) -> Result<T, TaskError> {
        let rx = self.result_rx.take().ok_or(TaskError::ProcessDead)?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(_)) => Err(TaskError::ProcessDead),
            Err(_) => Err(TaskError::Timeout),
        }
    }

    /// Non-blocking check for a task result.
    ///
    /// Returns `Some(Ok(value))` if done, `Some(Err(_))` on failure,
    /// or `None` if the task is still running.
    /// Unlike `await_result`, this can be called multiple times.
    /// Equivalent to Elixir's `Task.yield/2`.
    pub async fn yield_result(&mut self, timeout: Duration) -> Option<Result<T, TaskError>> {
        let rx = self.result_rx.as_mut()?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(val)) => {
                self.result_rx = None;
                Some(Ok(val))
            }
            Ok(Err(_)) => {
                self.result_rx = None;
                Some(Err(TaskError::ProcessDead))
            }
            Err(_) => None, // still running
        }
    }

    /// Shut down the task.
    ///
    /// Returns the result if the task completed, or `None` if it was killed.
    /// Equivalent to Elixir's `Task.shutdown/2`.
    #[allow(clippy::unused_async)]
    pub async fn shutdown(mut self) -> Option<T> {
        if let Some(mut rx) = self.result_rx.take() {
            // Try to get result immediately
            match rx.try_recv() {
                Ok(val) => return Some(val),
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => return None,
            }
        }
        // Kill the task process by aborting its join handle
        if let Some(handle) = self.join_handle.take() {
            handle.abort();
        }
        None
    }
}

/// Fire-and-forget task (no result tracking).
///
/// Equivalent to Elixir's `Task.start/1`.
pub async fn start_task<F, Fut>(runtime: &Runtime, f: F) -> ProcessId
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    runtime
        .spawn(move |_ctx| async move {
            f().await;
        })
        .await
}

/// Process a collection concurrently with bounded parallelism.
///
/// Returns results as a `Vec`, in input order if `opts.ordered` is true.
/// Equivalent to Elixir's `Task.async_stream/3` (collected).
///
/// # Panics
///
/// Panics if the internal concurrency semaphore is unexpectedly closed.
pub async fn async_map<I, F, Fut, T>(
    runtime: Arc<Runtime>,
    items: I,
    f: F,
    opts: StreamOpts,
) -> Vec<Result<T, TaskError>>
where
    I: IntoIterator,
    I::Item: Send + 'static,
    F: Fn(I::Item) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let f = Arc::new(f);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(opts.max_concurrency));

    let mut tasks: Vec<(usize, Task<T>)> = Vec::new();

    for (idx, item) in items.into_iter().enumerate() {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore should not be closed");
        let f = Arc::clone(&f);
        let timeout = opts.timeout;

        let (result_tx, result_rx) = oneshot::channel();

        let pid = runtime
            .spawn(move |_ctx| async move {
                let result = tokio::time::timeout(timeout, f(item)).await;
                if let Ok(val) = result {
                    let _ = result_tx.send(val);
                } else {
                    drop(result_tx); // signals timeout via channel close
                }
                drop(permit);
            })
            .await;

        tasks.push((
            idx,
            Task {
                pid,
                result_rx: Some(result_rx),
                join_handle: None,
            },
        ));
    }

    // Collect results by awaiting each oneshot
    let mut results: Vec<(usize, Result<T, TaskError>)> = Vec::with_capacity(tasks.len());
    for (idx, mut task) in tasks {
        let result = match task.result_rx.take() {
            Some(rx) => rx.await.map_err(|_| TaskError::ProcessDead),
            None => Err(TaskError::ProcessDead),
        };
        results.push((idx, result));
    }

    if opts.ordered {
        results.sort_by_key(|(idx, _)| *idx);
    }

    results.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[tokio::test]
    async fn async_task_returns_result() {
        let rt = Runtime::new(1);
        let mut task = async_task(&rt, || async { 42_u64 }).await;
        let result = task.await_result(Duration::from_secs(1)).await.unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn async_task_timeout_returns_error() {
        let rt = Runtime::new(1);
        let mut task = async_task(&rt, || async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            42_u64
        })
        .await;
        let result = task.await_result(Duration::from_millis(10)).await;
        assert!(matches!(result, Err(TaskError::Timeout)));
    }

    #[tokio::test]
    async fn async_task_pid_is_valid() {
        let rt = Runtime::new(1);
        let task = async_task(&rt, || async { 1 }).await;
        assert_eq!(task.pid().node_id(), 1);
        assert!(task.pid().local_id() > 0);
    }

    #[tokio::test]
    async fn async_task_with_context_can_send_messages() {
        let rt = Runtime::new(1);
        let (tx, rx) = oneshot::channel();

        let receiver = rt
            .spawn(move |mut ctx| async move {
                let msg = ctx.recv().await.unwrap();
                tx.send(msg.payload().as_str().unwrap().to_string())
                    .unwrap();
            })
            .await;

        let mut task = async_task_ctx(&rt, move |ctx| async move {
            ctx.send(receiver, rmpv::Value::String("from_task".into()))
                .await
                .unwrap();
            true
        })
        .await;

        let task_result = task.await_result(Duration::from_secs(1)).await.unwrap();
        assert!(task_result);

        let msg = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg, "from_task");
    }

    #[tokio::test]
    async fn yield_returns_none_when_not_ready() {
        let rt = Runtime::new(1);
        let mut task = async_task(&rt, || async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            42_u64
        })
        .await;

        let result = task.yield_result(Duration::from_millis(10)).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn yield_returns_some_when_complete() {
        let rt = Runtime::new(1);
        let mut task = async_task(&rt, || async { 42_u64 }).await;

        // Yield until task completes
        for _ in 0..1000 {
            if let Some(result) = task.yield_result(Duration::from_millis(10)).await {
                assert!(matches!(result, Ok(42)));
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("task did not complete in time");
    }

    #[tokio::test]
    async fn shutdown_returns_result_if_complete() {
        let rt = Runtime::new(1);
        let task = async_task(&rt, || async { 42_u64 }).await;

        // Yield until task completes
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }

        let result = task.shutdown().await;
        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn start_task_fire_and_forget() {
        let counter = Arc::new(AtomicU64::new(0));
        let c = counter.clone();
        let rt = Runtime::new(1);

        let pid = start_task(&rt, move || async move {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .await;

        assert!(pid.local_id() > 0);
        for _ in 0..1000 {
            if counter.load(Ordering::SeqCst) == 1 { break; }
            tokio::task::yield_now().await;
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn async_task_cleanup_after_completion() {
        let rt = Runtime::new(1);
        let mut task = async_task(&rt, || async { 1 }).await;
        let pid = task.pid();

        task.await_result(Duration::from_secs(1)).await.unwrap();
        // Process should be cleaned up
        for _ in 0..1000 {
            if rt.table().get(&pid).is_none() { break; }
            tokio::task::yield_now().await;
        }
        assert!(rt.table().get(&pid).is_none());
    }

    #[tokio::test]
    async fn async_task_does_not_break_existing_spawn() {
        let rt = Runtime::new(1);
        let (tx, rx) = oneshot::channel();

        let pid = rt
            .spawn(move |_ctx| async move {
                tx.send(42_u64).unwrap();
            })
            .await;

        assert!(pid.local_id() > 0);
        let val = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(val, 42);
    }

    // --- async_map tests ---

    #[tokio::test]
    async fn async_map_processes_all_items() {
        let rt = Arc::new(Runtime::new(1));
        let results = async_map(
            rt,
            vec![1_u64, 2, 3, 4, 5],
            |x| async move { x * 2 },
            StreamOpts::default(),
        )
        .await;

        let values: Vec<u64> = results.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(values, vec![2, 4, 6, 8, 10]);
    }

    #[tokio::test]
    async fn async_map_respects_max_concurrency() {
        let rt = Arc::new(Runtime::new(1));
        let active = Arc::new(AtomicU64::new(0));
        let max_seen = Arc::new(AtomicU64::new(0));

        let a = active.clone();
        let m = max_seen.clone();
        let results = async_map(
            rt,
            0..10_u64,
            move |_x| {
                let a = a.clone();
                let m = m.clone();
                async move {
                    let current = a.fetch_add(1, Ordering::SeqCst) + 1;
                    // Track max concurrent
                    m.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    a.fetch_sub(1, Ordering::SeqCst);
                    current
                }
            },
            StreamOpts {
                max_concurrency: 3,
                ..Default::default()
            },
        )
        .await;

        assert_eq!(results.len(), 10);
        assert!(
            max_seen.load(Ordering::SeqCst) <= 3,
            "max concurrency exceeded"
        );
    }

    #[tokio::test]
    async fn async_map_ordered_results() {
        let rt = Arc::new(Runtime::new(1));
        let results = async_map(
            rt,
            vec![50_u64, 10, 30, 20, 40],
            |x| async move {
                tokio::time::sleep(Duration::from_millis(x)).await;
                x
            },
            StreamOpts {
                max_concurrency: 5,
                ordered: true,
                timeout: Duration::from_secs(5),
            },
        )
        .await;

        let values: Vec<u64> = results.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(values, vec![50, 10, 30, 20, 40]); // input order preserved
    }

    #[tokio::test]
    async fn async_map_empty_input() {
        let rt = Arc::new(Runtime::new(1));
        let results = async_map(
            rt,
            Vec::<u64>::new(),
            |x| async move { x },
            StreamOpts::default(),
        )
        .await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn multiple_concurrent_tasks() {
        let rt = Runtime::new(1);
        let mut tasks = Vec::new();

        for i in 0..5_u64 {
            tasks.push(async_task(&rt, move || async move { i * 10 }).await);
        }

        let mut results = Vec::new();
        for task in &mut tasks {
            results.push(task.await_result(Duration::from_secs(1)).await.unwrap());
        }

        results.sort_unstable();
        assert_eq!(results, vec![0, 10, 20, 30, 40]);
    }
}
