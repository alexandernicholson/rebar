use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::process::ProcessId;
use crate::process::table::ProcessTable;
use crate::router::MessageRouter;

use super::TimerRef;

/// Hard ceiling for any timer duration. `tokio::time::sleep`/`interval`
/// panic on durations beyond ~2^63 ns; clamp well below that so a bogus
/// caller value can never crash the timer task.
const MAX_TIMER: Duration = Duration::from_secs(60 * 60 * 24 * 365 * 30); // ~30 years

/// Clamp a delay/interval into a safe, non-panicking range.
///
/// `tokio::time::interval` panics on a zero period, and both APIs panic on
/// absurdly large durations. We treat zero as 1ns (effectively "as fast as
/// possible" without panicking) and cap the upper bound.
fn clamp_duration(d: Duration) -> Duration {
    if d.is_zero() {
        Duration::from_nanos(1)
    } else if d > MAX_TIMER {
        MAX_TIMER
    } else {
        d
    }
}

/// Send a message to `dest` after `delay`.
///
/// Equivalent to Erlang's `:timer.send_after/3`.
/// Returns a [`TimerRef`] that can be used to cancel the timer.
///
/// A zero delay is treated as "essentially immediate" and an
/// absurdly large delay is capped; neither panics the timer task.
#[must_use]
pub fn send_after(
    router: Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    delay: Duration,
) -> TimerRef {
    let delay = clamp_duration(delay);
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
/// The interval stops automatically once the **destination** process is
/// dead (the next `route` fails). For a self-interval (`from == dest`, the
/// common case via [`ProcessContext::send_interval`]) this correctly stops
/// when the owning process dies.
///
/// Caveat for third-party intervals (`from != dest`): this variant is tied to
/// the *destination's* lifetime, not the owner's, so it can outlive the owner.
/// Use [`send_interval_owned`] to bind the timer to the owner's death.
///
/// A zero interval is clamped to a minimal non-zero period and an absurdly
/// large interval is capped, so neither panics the timer task.
#[must_use]
pub fn send_interval(
    router: Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    interval: Duration,
) -> TimerRef {
    let interval = clamp_duration(interval);
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

/// Like [`send_interval`], but the timer's lifetime is tied to the owner.
///
/// When the owner (`from`) dies, the table's exit hook aborts the interval,
/// even if the destination is a still-live third party. This closes the
/// "third-party interval survives owner death" leak by registering an
/// [`AbortHandle`](tokio::task::AbortHandle) cancelled from the owner's death
/// cleanup.
///
/// A zero interval is clamped to a minimal non-zero period and an absurdly
/// large interval is capped.
#[must_use]
pub fn send_interval_owned(
    table: &Arc<ProcessTable>,
    router: Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
    payload: rmpv::Value,
    interval: Duration,
) -> TimerRef {
    let interval = clamp_duration(interval);
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
    let abort = handle.abort_handle();
    // Abort when the owner dies. `cleanup_process` runs every exit hook, so a
    // dead owner cancels its interval rather than leaking a task that keeps
    // firing stale messages at the third party forever.
    let owner = from;
    table.add_exit_hook(move |pid| {
        if pid == owner {
            abort.abort();
        }
    });
    TimerRef::new(handle.abort_handle())
}

/// Execute a function after `delay`.
///
/// The function runs in a freshly spawned task.
/// Equivalent to Erlang's `:timer.apply_after/2`.
///
/// A zero delay is treated as essentially immediate and an absurdly large
/// delay is capped; neither panics the timer task.
#[must_use]
pub fn apply_after<F, Fut>(delay: Duration, f: F) -> TimerRef
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let delay = clamp_duration(delay);
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
///
/// A panic in one invocation is isolated: the offending tick is dropped and
/// the interval keeps ticking, rather than silently dying on the first panic.
/// A zero interval is clamped to a minimal non-zero period and an absurdly
/// large interval is capped.
#[must_use]
pub fn apply_interval<F, Fut>(interval_dur: Duration, f: F) -> TimerRef
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let interval_dur = clamp_duration(interval_dur);
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval_dur);
        tick.tick().await; // skip immediate first tick
        loop {
            tick.tick().await;
            // Run each invocation in its own task so a panic is contained:
            // a `JoinError` (panicked tick) is observed and ignored, and the
            // interval continues firing instead of dying silently.
            let fut = f();
            let inv = tokio::spawn(fut);
            let _ = inv.await;
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

    #[tokio::test]
    async fn send_interval_owned_stops_when_owner_dies() {
        // Owner (third party) and a separate live destination.
        let table = Arc::new(ProcessTable::new(1));
        let owner_pid = table.allocate_pid();
        let (owner_tx, _owner_rx) = Mailbox::unbounded();
        table.insert(owner_pid, ProcessHandle::new(owner_tx));

        let dest_pid = table.allocate_pid();
        let (dest_tx, mut dest_rx) = Mailbox::unbounded();
        table.insert(dest_pid, ProcessHandle::new(dest_tx));

        let router: Arc<dyn MessageRouter> = Arc::new(LocalRouter::new(Arc::clone(&table)));

        let timer = send_interval_owned(
            &table,
            router,
            owner_pid,
            dest_pid,
            rmpv::Value::String("tick".into()),
            Duration::from_millis(20),
        );

        // Confirm at least one tick is delivered.
        tokio::time::timeout(Duration::from_secs(1), dest_rx.recv())
            .await
            .unwrap()
            .unwrap();

        // Owner dies — the exit hook must abort the interval even though the
        // destination is still alive.
        table.cleanup_process(owner_pid);

        // Allow the abort to propagate, then drain.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        while dest_rx.try_recv().is_some() {}

        let result = tokio::time::timeout(Duration::from_millis(150), dest_rx.recv()).await;
        assert!(result.is_err(), "interval should stop after owner death");
        assert!(timer.is_finished());
    }

    #[tokio::test]
    async fn zero_and_huge_durations_do_not_panic() {
        let (router, from, dest, mut rx, _table) = setup_router_and_receiver();

        // Zero delay: clamped, still delivers without panicking.
        let _t = send_after(
            router.clone(),
            from,
            dest,
            rmpv::Value::Nil,
            Duration::ZERO,
        );
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();

        // Huge interval: clamped to MAX_TIMER, spawning must not panic.
        let t = send_interval(
            router,
            from,
            dest,
            rmpv::Value::Nil,
            Duration::from_secs(u64::MAX),
        );
        // It simply won't fire soon; just confirm it didn't panic on spawn.
        assert!(!t.is_finished());
        t.cancel();
    }

    #[tokio::test]
    async fn apply_interval_survives_callback_panic() {
        let counter = Arc::new(AtomicU64::new(0));
        let c = counter.clone();

        let timer = apply_interval(Duration::from_millis(20), move || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                // Panic on the first invocation only.
                assert!(n != 0, "intentional panic on first tick");
            }
        });

        // Despite the first tick panicking, later ticks must keep running.
        for _ in 0..200 {
            if counter.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            counter.load(Ordering::SeqCst) >= 3,
            "interval should keep firing after a callback panic"
        );
        timer.cancel();
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
