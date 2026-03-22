use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::process::ProcessId;
use crate::router::MessageRouter;

use super::TimerRef;

/// Send a message to `dest` after `delay`.
///
/// Equivalent to Erlang's `:timer.send_after/3`.
/// Returns a [`TimerRef`] that can be used to cancel the timer.
#[must_use]
pub fn send_after(
    router: Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    delay: Duration,
) -> TimerRef {
    let handle = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = router.route(from, dest, payload);
    });
    TimerRef::new(handle.abort_handle())
}

/// Send a message to `dest` repeatedly at `interval`.
///
/// The first message is sent after one interval has elapsed (not immediately).
/// Equivalent to Erlang's `:timer.send_interval/3`.
/// Returns a [`TimerRef`] that can be used to cancel the interval.
///
/// The interval automatically stops if the destination process is dead.
#[must_use]
pub fn send_interval(
    router: Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    interval: Duration,
) -> TimerRef {
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await; // skip the immediate first tick
        loop {
            tick.tick().await;
            if router.route(from, dest, payload.clone()).is_err() {
                break;
            }
        }
    });
    TimerRef::new(handle.abort_handle())
}

/// Execute a function after `delay`.
///
/// The function runs in a freshly spawned task.
/// Equivalent to Erlang's `:timer.apply_after/2`.
#[must_use]
pub fn apply_after<F, Fut>(delay: Duration, f: F) -> TimerRef
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        f().await;
    });
    TimerRef::new(handle.abort_handle())
}

/// Execute a function repeatedly at `interval`.
///
/// Each invocation waits for the previous one to complete before scheduling
/// the next. Equivalent to Erlang's `:timer.apply_repeatedly/2`.
#[must_use]
pub fn apply_interval<F, Fut>(interval_dur: Duration, f: F) -> TimerRef
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval_dur);
        tick.tick().await; // skip immediate first tick
        loop {
            tick.tick().await;
            f().await;
        }
    });
    TimerRef::new(handle.abort_handle())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::mailbox::Mailbox;
    use crate::process::table::{ProcessHandle, ProcessTable};
    use crate::router::LocalRouter;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn setup_router_and_receiver() -> (
        Arc<dyn MessageRouter>,
        ProcessId,
        ProcessId,
        crate::process::mailbox::MailboxRx,
        Arc<ProcessTable>, // kept alive so channel doesn't close
    ) {
        let table = Arc::new(ProcessTable::new(1));
        let sender_pid = ProcessId::new(1, 0);
        let receiver_pid = table.allocate_pid();
        let (tx, rx) = Mailbox::unbounded();
        table.insert(receiver_pid, ProcessHandle::new(tx));
        let router: Arc<dyn MessageRouter> = Arc::new(LocalRouter::new(Arc::clone(&table)));
        (router, sender_pid, receiver_pid, rx, table)
    }

    // --- send_after tests ---

    #[tokio::test]
    async fn send_after_delivers_message() {
        let (router, from, dest, mut rx, _table) = setup_router_and_receiver();

        let _timer = send_after(
            router,
            from,
            dest,
            rmpv::Value::String("hello".into()),
            Duration::from_millis(10),
        );

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "hello");
    }

    #[tokio::test]
    async fn send_after_does_not_deliver_early() {
        let (router, from, dest, mut rx, _table) = setup_router_and_receiver();

        let _timer = send_after(
            router,
            from,
            dest,
            rmpv::Value::String("delayed".into()),
            Duration::from_millis(200),
        );

        // Should not have arrived yet after 10ms
        let result = tokio::time::timeout(Duration::from_millis(10), rx.recv()).await;
        assert!(result.is_err(), "message should not arrive before delay");

        // But should arrive eventually
        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "delayed");
    }

    #[tokio::test]
    async fn send_after_cancel_prevents_delivery() {
        let (router, from, dest, mut rx, _table) = setup_router_and_receiver();

        let timer = send_after(
            router,
            from,
            dest,
            rmpv::Value::String("cancelled".into()),
            Duration::from_millis(100),
        );

        timer.cancel();
        // Allow the abort to propagate across threads
        tokio::time::sleep(Duration::from_millis(5)).await;

        let result = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(result.is_err(), "cancelled timer should not deliver");
    }

    #[tokio::test]
    async fn send_after_to_dead_process_does_not_panic() {
        let table = Arc::new(ProcessTable::new(1));
        let router: Arc<dyn MessageRouter> = Arc::new(LocalRouter::new(table));
        let from = ProcessId::new(1, 0);
        let dead = ProcessId::new(1, 999);

        // Should not panic even though dest doesn't exist
        let timer = send_after(
            router,
            from,
            dead,
            rmpv::Value::Nil,
            Duration::from_millis(10),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(timer.is_finished());
    }

    // --- send_interval tests ---

    #[tokio::test]
    async fn send_interval_delivers_repeatedly() {
        let (router, from, dest, mut rx, _table) = setup_router_and_receiver();

        let timer = send_interval(
            router,
            from,
            dest,
            rmpv::Value::String("tick".into()),
            Duration::from_millis(20),
        );

        // Collect at least 3 messages
        let mut count = 0;
        for _ in 0..3 {
            let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(msg.payload().as_str().unwrap(), "tick");
            count += 1;
        }
        assert_eq!(count, 3);
        timer.cancel();
    }

    #[tokio::test]
    async fn send_interval_cancel_stops_repeating() {
        let (router, from, dest, mut rx, _table) = setup_router_and_receiver();

        let timer = send_interval(
            router,
            from,
            dest,
            rmpv::Value::String("tick".into()),
            Duration::from_millis(20),
        );

        // Wait for at least one message
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();

        timer.cancel();

        // Drain any in-flight message
        tokio::time::sleep(Duration::from_millis(50)).await;
        while rx.try_recv().is_some() {}

        // No more messages should arrive
        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(result.is_err(), "cancelled interval should stop");
    }

    // --- apply_after tests ---

    #[tokio::test]
    async fn apply_after_runs_function() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = counter.clone();

        let _timer = apply_after(Duration::from_millis(10), move || async move {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn apply_after_cancel_prevents_execution() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = counter.clone();

        let timer = apply_after(Duration::from_millis(100), move || async move {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        timer.cancel();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    // --- apply_interval tests ---

    #[tokio::test]
    async fn apply_interval_runs_repeatedly() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = counter.clone();

        let timer = apply_interval(Duration::from_millis(20), move || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        tokio::time::sleep(Duration::from_millis(150)).await;
        let count = counter.load(Ordering::SeqCst);
        assert!(count >= 3, "expected at least 3 invocations, got {count}");
        timer.cancel();
    }

    #[tokio::test]
    async fn apply_interval_cancel_stops() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = counter.clone();

        let timer = apply_interval(Duration::from_millis(20), move || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        tokio::time::sleep(Duration::from_millis(60)).await;
        timer.cancel();
        let count_at_cancel = counter.load(Ordering::SeqCst);

        tokio::time::sleep(Duration::from_millis(100)).await;
        let count_after = counter.load(Ordering::SeqCst);
        assert!(
            count_after <= count_at_cancel + 1,
            "should stop after cancel"
        );
    }

    // --- multiple timers ---

    #[tokio::test]
    async fn multiple_timers_independent() {
        let (router, from, dest, mut rx, _table) = setup_router_and_receiver();

        let _t1 = send_after(
            router.clone(),
            from,
            dest,
            rmpv::Value::String("first".into()),
            Duration::from_millis(10),
        );

        let t2 = send_after(
            router,
            from,
            dest,
            rmpv::Value::String("second".into()),
            Duration::from_millis(50),
        );

        // Cancel only the second
        t2.cancel();
        tokio::time::sleep(Duration::from_millis(5)).await;

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.payload().as_str().unwrap(), "first");

        // Second should not arrive
        let result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(result.is_err());
    }
}
