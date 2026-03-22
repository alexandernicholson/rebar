use std::fmt;

/// An opaque reference to a running timer. Can be used to cancel the timer.
///
/// Equivalent to Erlang's `TRef` returned by `:timer` functions.
#[derive(Clone)]
pub struct TimerRef {
    abort_handle: tokio::task::AbortHandle,
}

impl TimerRef {
    pub(crate) const fn new(abort_handle: tokio::task::AbortHandle) -> Self {
        Self { abort_handle }
    }

    /// Cancel this timer. If the timer has already fired or been cancelled,
    /// this is a no-op.
    pub fn cancel(&self) {
        self.abort_handle.abort();
    }

    /// Returns true if the timer task has finished (fired or cancelled).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.abort_handle.is_finished()
    }
}

impl fmt::Debug for TimerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimerRef")
            .field("finished", &self.abort_handle.is_finished())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timer_ref_cancel_is_idempotent() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        let timer = TimerRef::new(handle.abort_handle());
        timer.cancel();
        timer.cancel(); // should not panic
        // Allow the runtime to process the abort
        tokio::task::yield_now().await;
        assert!(timer.is_finished());
    }

    #[tokio::test]
    async fn timer_ref_debug_format() {
        let handle = tokio::spawn(async {});
        let timer = TimerRef::new(handle.abort_handle());
        tokio::task::yield_now().await;
        let debug = format!("{timer:?}");
        assert!(debug.contains("TimerRef"));
    }

    #[tokio::test]
    async fn timer_ref_clone_shares_handle() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        let timer1 = TimerRef::new(handle.abort_handle());
        let timer2 = timer1.clone();
        timer1.cancel();
        tokio::task::yield_now().await;
        assert!(timer2.is_finished());
    }
}
