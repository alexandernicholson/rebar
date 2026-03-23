pub mod engine;
pub mod types;

pub use engine::*;
pub use types::*;

#[cfg(test)]
#[allow(
    clippy::items_after_statements,
    clippy::significant_drop_tightening,
    clippy::needless_pass_by_value
)]
mod tests {
    use super::*;
    use crate::process::ExitReason;
    use crate::runtime::Runtime;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // ========================================================================
    // Test state machine: simple counter
    // ========================================================================

    #[derive(Debug, Clone, PartialEq)]
    enum CounterState {
        Running,
    }

    struct CounterData {
        count: u32,
    }

    struct CounterStatem;

    #[async_trait::async_trait]
    impl GenStatem for CounterStatem {
        type State = CounterState;
        type Data = CounterData;
        type Call = String;
        type Cast = String;
        type Reply = u32;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((CounterState::Running, CounterData { count: 0 }))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Call(reply_tx) => {
                    let count = data.count;
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, count)],
                    }
                }
                EventType::Cast => {
                    data.count += 1;
                    TransitionResult::KeepState {
                        data: CounterData { count: data.count },
                        actions: vec![],
                    }
                }
                _ => TransitionResult::KeepStateAndData {
                    actions: vec![],
                },
            }
        }
    }

    // ========================================================================
    // Shared two-state enum for transition tests
    // ========================================================================

    #[derive(Debug, Clone, PartialEq)]
    enum TwoState {
        A,
        B,
    }

    // ========================================================================
    // 1. statem_starts_in_initial_state
    // ========================================================================
    #[tokio::test]
    async fn statem_starts_in_initial_state() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), CounterStatem).await;
        let count = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    // ========================================================================
    // 2. statem_handles_call
    // ========================================================================
    #[tokio::test]
    async fn statem_handles_call() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), CounterStatem).await;
        let reply = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(reply, 0);
    }

    // ========================================================================
    // 3. statem_handles_cast
    // ========================================================================
    #[tokio::test]
    async fn statem_handles_cast() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), CounterStatem).await;
        statem_ref.cast("inc".to_string()).unwrap();
        for _ in 0..1000 {
            let count = statem_ref
                .call("get".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if count == 1 { break; }
            tokio::task::yield_now().await;
        }
        let count = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    // ========================================================================
    // 4. statem_stops_cleanly
    // ========================================================================
    struct StoppingStatem {
        terminated: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for StoppingStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = String;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("alive", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Call(reply_tx) => TransitionResult::StopAndReply {
                    reason: ExitReason::Normal,
                    data: (),
                    replies: vec![(reply_tx, "stopping".to_string())],
                },
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }

        async fn terminate(
            &self,
            _reason: ExitReason,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) {
            self.terminated.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn statem_stops_cleanly() {
        let terminated = Arc::new(AtomicBool::new(false));
        let statem_ref = spawn_gen_statem(
            Arc::new(Runtime::new(1)),
            StoppingStatem {
                terminated: Arc::clone(&terminated),
            },
        )
        .await;

        let reply = statem_ref
            .call("stop".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(reply, "stopping");
        for _ in 0..1000 {
            if terminated.load(Ordering::SeqCst) { break; }
            tokio::task::yield_now().await;
        }
        assert!(terminated.load(Ordering::SeqCst));
    }

    // ========================================================================
    // 5. statem_ref_has_pid
    // ========================================================================
    #[tokio::test]
    async fn statem_ref_has_pid() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), CounterStatem).await;
        assert_eq!(statem_ref.pid().node_id(), 1);
    }

    // ========================================================================
    // 6. next_state_changes_state
    // ========================================================================
    struct TransitionStatem;

    #[async_trait::async_trait]
    impl GenStatem for TransitionStatem {
        type State = TwoState;
        type Data = Vec<String>;
        type Call = String;
        type Cast = String;
        type Reply = String;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((TwoState::A, vec![]))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            state: &Self::State,
            data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Call(reply_tx) => {
                    let state_name = format!("{state:?}");
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, state_name)],
                    }
                }
                EventType::Cast => {
                    data.push(format!("transition from {state:?}"));
                    let new_state = match state {
                        TwoState::A => TwoState::B,
                        TwoState::B => TwoState::A,
                    };
                    TransitionResult::NextState {
                        state: new_state,
                        data: data.clone(),
                        actions: vec![],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn next_state_changes_state() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), TransitionStatem).await;

        let state = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(state, "A");

        statem_ref.cast("go".to_string()).unwrap();
        for _ in 0..1000 {
            let state = statem_ref
                .call("get".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if state == "B" { break; }
            tokio::task::yield_now().await;
        }

        let state = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(state, "B");
    }

    // ========================================================================
    // 7. keep_state_preserves_state
    // ========================================================================
    #[tokio::test]
    async fn keep_state_preserves_state() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), CounterStatem).await;

        statem_ref.cast("inc".to_string()).unwrap();
        for _ in 0..1000 {
            let count = statem_ref
                .call("get".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if count == 1 { break; }
            tokio::task::yield_now().await;
        }

        let count = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    // ========================================================================
    // 8. keep_state_and_data_no_changes
    // ========================================================================
    #[tokio::test]
    async fn keep_state_and_data_no_changes() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), CounterStatem).await;

        let count1 = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        let count2 = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count1, count2);
        assert_eq!(count1, 0);
    }

    // ========================================================================
    // 9. state_change_triggers_enter
    // ========================================================================
    struct EnterStatem {
        enter_count: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl GenStatem for EnterStatem {
        type State = TwoState;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = u32;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, true)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((TwoState::A, ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Enter { .. } => {
                    self.enter_count.fetch_add(1, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let count = self.enter_count.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, count)],
                    }
                }
                EventType::Cast => {
                    let new_state = match state {
                        TwoState::A => TwoState::B,
                        TwoState::B => TwoState::A,
                    };
                    TransitionResult::NextState {
                        state: new_state,
                        data: (),
                        actions: vec![],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn state_change_triggers_enter() {
        let enter_count = Arc::new(AtomicU32::new(0));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            EnterStatem {
                enter_count: Arc::clone(&enter_count),
            },
        )
        .await;

        for _ in 0..1000 {
            let count = statem_ref
                .call("get".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if count == 1 { break; }
            tokio::task::yield_now().await;
        }
        let count = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 1);

        statem_ref.cast("go".to_string()).unwrap();
        for _ in 0..1000 {
            let count = statem_ref
                .call("get".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if count == 2 { break; }
            tokio::task::yield_now().await;
        }

        let count = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    // ========================================================================
    // 10. no_enter_when_disabled
    // ========================================================================
    struct NoEnterStatem {
        enter_count: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl GenStatem for NoEnterStatem {
        type State = TwoState;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = u32;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((TwoState::A, ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Enter { .. } => {
                    self.enter_count.fetch_add(1, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let count = self.enter_count.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, count)],
                    }
                }
                EventType::Cast => {
                    let new_state = match state {
                        TwoState::A => TwoState::B,
                        TwoState::B => TwoState::A,
                    };
                    TransitionResult::NextState {
                        state: new_state,
                        data: (),
                        actions: vec![],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn no_enter_when_disabled() {
        let enter_count = Arc::new(AtomicU32::new(0));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            NoEnterStatem {
                enter_count: Arc::clone(&enter_count),
            },
        )
        .await;

        statem_ref.cast("go".to_string()).unwrap();
        // No enter_count events should fire (enter disabled). Wait for cast to be processed.
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }

        let count = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    // ========================================================================
    // 11. enter_receives_old_state
    // ========================================================================
    struct EnterOldStateStatem {
        old_state_names: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl GenStatem for EnterOldStateStatem {
        type State = TwoState;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = Vec<String>;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, true)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((TwoState::A, ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Enter { old_state_name } => {
                    self.old_state_names.lock().await.push(old_state_name);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let names = self.old_state_names.lock().await.clone();
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, names)],
                    }
                }
                EventType::Cast => {
                    let new_state = match state {
                        TwoState::A => TwoState::B,
                        TwoState::B => TwoState::A,
                    };
                    TransitionResult::NextState {
                        state: new_state,
                        data: (),
                        actions: vec![],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn enter_receives_old_state() {
        let old_state_names = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            EnterOldStateStatem {
                old_state_names: Arc::clone(&old_state_names),
            },
        )
        .await;

        statem_ref.cast("go".to_string()).unwrap();
        for _ in 0..1000 {
            let names = statem_ref
                .call("get".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if names.len() == 2 { break; }
            tokio::task::yield_now().await;
        }

        let names = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "A");
        assert_eq!(names[1], "A");
    }

    // ========================================================================
    // 12. state_timeout_fires
    // ========================================================================
    struct StateTimeoutStatem {
        timeout_fired: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for StateTimeoutStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = bool;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("waiting", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => TransitionResult::KeepStateAndData {
                    actions: vec![Action::StateTimeout(
                        Duration::from_millis(50),
                        rmpv::Value::String("timeout_data".into()),
                    )],
                },
                EventType::StateTimeout => {
                    self.timeout_fired.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let fired = self.timeout_fired.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, fired)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn state_timeout_fires() {
        let timeout_fired = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            StateTimeoutStatem {
                timeout_fired: Arc::clone(&timeout_fired),
            },
        )
        .await;

        statem_ref.cast("set_timeout".to_string()).unwrap();

        // Timeout is 50ms; poll until it fires
        for _ in 0..200 {
            let fired = statem_ref
                .call("check".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if fired { return; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("state timeout did not fire");
    }

    // ========================================================================
    // 13. state_timeout_cancelled_on_state_change
    // ========================================================================
    struct StateTimeoutCancelStatem {
        timeout_fired: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for StateTimeoutCancelStatem {
        type State = TwoState;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = bool;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((TwoState::A, ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => {
                    if *state == TwoState::A {
                        TransitionResult::NextState {
                            state: TwoState::B,
                            data: (),
                            actions: vec![Action::StateTimeout(
                                Duration::from_millis(50),
                                rmpv::Value::Nil,
                            )],
                        }
                    } else {
                        TransitionResult::KeepStateAndData { actions: vec![] }
                    }
                }
                EventType::StateTimeout => {
                    self.timeout_fired.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let fired = self.timeout_fired.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, fired)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn state_timeout_cancelled_on_state_change() {
        let timeout_fired = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            StateTimeoutCancelStatem {
                timeout_fired: Arc::clone(&timeout_fired),
            },
        )
        .await;

        statem_ref.cast("go".to_string()).unwrap();
        // Must wait longer than the 50ms timeout to confirm it did NOT fire
        tokio::time::sleep(Duration::from_millis(100)).await;

        let fired = statem_ref
            .call("check".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(
            !fired,
            "state timeout should have been cancelled on state change"
        );
    }

    // ========================================================================
    // 14. event_timeout_fires
    // ========================================================================
    struct EventTimeoutStatem {
        timeout_fired: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for EventTimeoutStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = bool;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("idle", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => TransitionResult::KeepStateAndData {
                    actions: vec![Action::EventTimeout(
                        Duration::from_millis(50),
                        rmpv::Value::String("event_timeout_data".into()),
                    )],
                },
                EventType::EventTimeout => {
                    self.timeout_fired.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let fired = self.timeout_fired.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, fired)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn event_timeout_fires() {
        let timeout_fired = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            EventTimeoutStatem {
                timeout_fired: Arc::clone(&timeout_fired),
            },
        )
        .await;

        statem_ref.cast("set_timeout".to_string()).unwrap();

        // Event timeout is 50ms. We must NOT send any events (including calls)
        // during the wait, because any event cancels the event timeout.
        // Wait for timeout to fire, then check via call.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let fired = statem_ref
            .call("check".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(fired);
    }

    // ========================================================================
    // 15. event_timeout_cancelled_by_any_event
    // ========================================================================
    struct EventTimeoutCancelStatem {
        timeout_fired: Arc<AtomicBool>,
        first_cast: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for EventTimeoutCancelStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = bool;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("idle", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => {
                    if self.first_cast.swap(true, Ordering::SeqCst) {
                        // Second+ cast: event timeout already cancelled by engine
                        TransitionResult::KeepStateAndData { actions: vec![] }
                    } else {
                        // First cast: set event timeout
                        TransitionResult::KeepStateAndData {
                            actions: vec![Action::EventTimeout(
                                Duration::from_millis(50),
                                rmpv::Value::Nil,
                            )],
                        }
                    }
                }
                EventType::EventTimeout => {
                    self.timeout_fired.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let fired = self.timeout_fired.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, fired)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn event_timeout_cancelled_by_any_event() {
        let timeout_fired = Arc::new(AtomicBool::new(false));
        let first_cast = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            EventTimeoutCancelStatem {
                timeout_fired: Arc::clone(&timeout_fired),
                first_cast: Arc::clone(&first_cast),
            },
        )
        .await;

        statem_ref.cast("set".to_string()).unwrap();
        statem_ref.cast("cancel".to_string()).unwrap();
        // Must wait longer than the 50ms timeout to confirm it did NOT fire
        tokio::time::sleep(Duration::from_millis(100)).await;

        let fired = statem_ref
            .call("check".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(
            !fired,
            "event timeout should have been cancelled by the second cast"
        );
    }

    // ========================================================================
    // 16. generic_timeout_fires
    // ========================================================================
    struct GenericTimeoutStatem {
        timeout_fired: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for GenericTimeoutStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = bool;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("idle", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => TransitionResult::KeepStateAndData {
                    actions: vec![Action::GenericTimeout(
                        "my_timer".to_string(),
                        Duration::from_millis(50),
                        rmpv::Value::String("generic_data".into()),
                    )],
                },
                EventType::Timeout(ref name) if name == "my_timer" => {
                    self.timeout_fired.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let fired = self.timeout_fired.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, fired)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn generic_timeout_fires() {
        let timeout_fired = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            GenericTimeoutStatem {
                timeout_fired: Arc::clone(&timeout_fired),
            },
        )
        .await;

        statem_ref.cast("set".to_string()).unwrap();

        // Timeout is 50ms; poll until it fires
        for _ in 0..200 {
            let fired = statem_ref
                .call("check".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if fired { return; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("generic timeout did not fire");
    }

    // ========================================================================
    // 17. generic_timeout_independent
    // ========================================================================
    struct MultiTimeoutStatem {
        timer_a_fired: Arc<AtomicBool>,
        timer_b_fired: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for MultiTimeoutStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = (bool, bool);

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("idle", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => TransitionResult::KeepStateAndData {
                    actions: vec![
                        Action::GenericTimeout(
                            "timer_a".to_string(),
                            Duration::from_millis(30),
                            rmpv::Value::Nil,
                        ),
                        Action::GenericTimeout(
                            "timer_b".to_string(),
                            Duration::from_millis(80),
                            rmpv::Value::Nil,
                        ),
                    ],
                },
                EventType::Timeout(ref name) if name == "timer_a" => {
                    self.timer_a_fired.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Timeout(ref name) if name == "timer_b" => {
                    self.timer_b_fired.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let a = self.timer_a_fired.load(Ordering::SeqCst);
                    let b = self.timer_b_fired.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, (a, b))],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn generic_timeout_independent() {
        let timer_a = Arc::new(AtomicBool::new(false));
        let timer_b = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            MultiTimeoutStatem {
                timer_a_fired: Arc::clone(&timer_a),
                timer_b_fired: Arc::clone(&timer_b),
            },
        )
        .await;

        statem_ref.cast("set".to_string()).unwrap();

        // timer_a is 30ms; poll until it fires
        for _ in 0..200 {
            let (a, _b) = statem_ref
                .call("check".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if a { break; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let (a, b) = statem_ref
            .call("check".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(a, "timer_a should have fired");
        assert!(!b, "timer_b should not have fired yet");

        // timer_b is 80ms; poll until it fires
        for _ in 0..200 {
            let (_a, b) = statem_ref
                .call("check".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if b { break; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let (a, b) = statem_ref
            .call("check".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(a);
        assert!(b, "timer_b should have fired");
    }

    // ========================================================================
    // 18. cancel_timeout_action
    // ========================================================================
    struct CancelTimeoutStatem {
        timeout_fired: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for CancelTimeoutStatem {
        type State = &'static str;
        type Data = bool;
        type Call = String;
        type Cast = String;
        type Reply = bool;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("idle", false))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => {
                    if *data {
                        // Second cast: cancel the timeout
                        TransitionResult::KeepStateAndData {
                            actions: vec![Action::CancelTimeout(TimeoutKind::Generic(
                                "cancel_me".to_string(),
                            ))],
                        }
                    } else {
                        // First cast: set the timeout
                        *data = true;
                        TransitionResult::KeepState {
                            data: true,
                            actions: vec![Action::GenericTimeout(
                                "cancel_me".to_string(),
                                Duration::from_millis(50),
                                rmpv::Value::Nil,
                            )],
                        }
                    }
                }
                EventType::Timeout(ref name) if name == "cancel_me" => {
                    self.timeout_fired.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let fired = self.timeout_fired.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, fired)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn cancel_timeout_action() {
        let timeout_fired = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            CancelTimeoutStatem {
                timeout_fired: Arc::clone(&timeout_fired),
            },
        )
        .await;

        statem_ref.cast("set".to_string()).unwrap();
        statem_ref.cast("cancel".to_string()).unwrap();
        // Must wait longer than the 50ms timeout to confirm it did NOT fire
        tokio::time::sleep(Duration::from_millis(100)).await;

        let fired = statem_ref
            .call("check".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(!fired, "timeout should have been cancelled");
    }

    // ========================================================================
    // 19. postponed_events_replayed_on_state_change
    // ========================================================================
    struct PostponeStatem {
        processed_in_b: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl GenStatem for PostponeStatem {
        type State = TwoState;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = u32;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((TwoState::A, ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast if *state == TwoState::A => {
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Postpone],
                    }
                }
                EventType::Cast if *state == TwoState::B => {
                    self.processed_in_b.fetch_add(1, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let count = self.processed_in_b.load(Ordering::SeqCst);
                    TransitionResult::NextState {
                        state: TwoState::B,
                        data: (),
                        actions: vec![Action::Reply(reply_tx, count)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn postponed_events_replayed_on_state_change() {
        let processed = Arc::new(AtomicU32::new(0));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            PostponeStatem {
                processed_in_b: Arc::clone(&processed),
            },
        )
        .await;

        statem_ref.cast("msg1".to_string()).unwrap();
        statem_ref.cast("msg2".to_string()).unwrap();

        // Call is a sync barrier; casts are queued before it
        let _count = statem_ref
            .call("transition".to_string(), Duration::from_secs(1))
            .await
            .unwrap();

        for _ in 0..1000 {
            if processed.load(Ordering::SeqCst) == 2 { break; }
            tokio::task::yield_now().await;
        }
        assert_eq!(processed.load(Ordering::SeqCst), 2);
    }

    // ========================================================================
    // 20. postponed_events_not_replayed_on_keep_state
    // ========================================================================
    #[tokio::test]
    async fn postponed_events_not_replayed_on_keep_state() {
        let processed = Arc::new(AtomicU32::new(0));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            PostponeStatem {
                processed_in_b: Arc::clone(&processed),
            },
        )
        .await;

        statem_ref.cast("msg".to_string()).unwrap();
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }

        assert_eq!(processed.load(Ordering::SeqCst), 0);
    }

    // ========================================================================
    // 21. multiple_postponed_events_ordered
    // ========================================================================
    struct OrderedPostponeStatem {
        order: Arc<tokio::sync::Mutex<Vec<u32>>>,
    }

    #[async_trait::async_trait]
    impl GenStatem for OrderedPostponeStatem {
        type State = TwoState;
        type Data = ();
        type Call = u32;
        type Cast = u32;
        type Reply = Vec<u32>;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((TwoState::A, ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast if *state == TwoState::A => {
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Postpone],
                    }
                }
                EventType::Cast if *state == TwoState::B => {
                    let mut guard = self.order.lock().await;
                    let len = guard.len();
                    guard.push(u32::try_from(len).unwrap_or(0) + 1);
                    drop(guard);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let order = self.order.lock().await.clone();
                    TransitionResult::NextState {
                        state: TwoState::B,
                        data: (),
                        actions: vec![Action::Reply(reply_tx, order)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn multiple_postponed_events_ordered() {
        let order = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            OrderedPostponeStatem {
                order: Arc::clone(&order),
            },
        )
        .await;

        statem_ref.cast(1).unwrap();
        statem_ref.cast(2).unwrap();
        statem_ref.cast(3).unwrap();

        let _result = statem_ref.call(0, Duration::from_secs(1)).await.unwrap();

        for _ in 0..1000 {
            if order.lock().await.len() == 3 { break; }
            tokio::task::yield_now().await;
        }

        let final_order = order.lock().await.clone();
        assert_eq!(final_order.len(), 3, "all 3 postponed events should replay");
        assert_eq!(final_order, vec![1, 2, 3]);
    }

    // ========================================================================
    // 22. next_event_inserts_internal
    // ========================================================================
    struct NextEventStatem {
        internal_received: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl GenStatem for NextEventStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = bool;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("idle", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => TransitionResult::KeepStateAndData {
                    actions: vec![Action::NextEvent(rmpv::Value::String(
                        "internal_msg".into(),
                    ))],
                },
                EventType::Internal => {
                    self.internal_received.store(true, Ordering::SeqCst);
                    TransitionResult::KeepStateAndData { actions: vec![] }
                }
                EventType::Call(reply_tx) => {
                    let received = self.internal_received.load(Ordering::SeqCst);
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, received)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn next_event_inserts_internal() {
        let internal_received = Arc::new(AtomicBool::new(false));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            NextEventStatem {
                internal_received: Arc::clone(&internal_received),
            },
        )
        .await;

        statem_ref.cast("trigger".to_string()).unwrap();
        for _ in 0..1000 {
            let received = statem_ref
                .call("check".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if received { return; }
            tokio::task::yield_now().await;
        }

        let received = statem_ref
            .call("check".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(received, "internal event should have been processed");
    }

    // ========================================================================
    // 23. multiple_replies_in_stop_and_reply
    // ========================================================================
    struct MultiReplyStatem;

    #[async_trait::async_trait]
    impl GenStatem for MultiReplyStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = String;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("alive", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Call(reply_tx) => TransitionResult::StopAndReply {
                    reason: ExitReason::Normal,
                    data: (),
                    replies: vec![(reply_tx, "goodbye".to_string())],
                },
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn multiple_replies_in_stop_and_reply() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), MultiReplyStatem).await;

        let reply = statem_ref
            .call("stop".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(reply, "goodbye");
    }

    // ========================================================================
    // 24. hibernate_hint_accepted
    // ========================================================================
    struct HibernateStatem;

    #[async_trait::async_trait]
    impl GenStatem for HibernateStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = String;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("active", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => TransitionResult::KeepStateAndData {
                    actions: vec![Action::Hibernate],
                },
                EventType::Call(reply_tx) => TransitionResult::KeepStateAndData {
                    actions: vec![Action::Reply(reply_tx, "still_alive".to_string())],
                },
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn hibernate_hint_accepted() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), HibernateStatem).await;

        statem_ref.cast("hibernate".to_string()).unwrap();

        let reply = statem_ref
            .call("check".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(reply, "still_alive");
    }

    // ========================================================================
    // 25. gen_statem_ref_is_send_sync
    // ========================================================================
    #[tokio::test]
    async fn gen_statem_ref_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GenStatemRef<CounterStatem>>();
    }

    // ========================================================================
    // 26. connection_state_machine_example
    // ========================================================================
    #[derive(Debug, Clone, PartialEq)]
    enum ConnState {
        Connecting,
        Connected,
        Disconnected,
    }

    struct ConnData {
        attempts: u32,
        messages_sent: u32,
    }

    struct ConnectionStatem;

    #[async_trait::async_trait]
    impl GenStatem for ConnectionStatem {
        type State = ConnState;
        type Data = ConnData;
        type Call = String;
        type Cast = String;
        type Reply = String;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, true)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok((
                ConnState::Connecting,
                ConnData {
                    attempts: 0,
                    messages_sent: 0,
                },
            ))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            state: &Self::State,
            data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Enter { .. } => {
                    if *state == ConnState::Connecting {
                        data.attempts += 1;
                        TransitionResult::KeepState {
                            data: ConnData {
                                attempts: data.attempts,
                                messages_sent: data.messages_sent,
                            },
                            actions: vec![Action::StateTimeout(
                                Duration::from_millis(50),
                                rmpv::Value::Nil,
                            )],
                        }
                    } else {
                        TransitionResult::KeepStateAndData { actions: vec![] }
                    }
                }
                EventType::Cast => match state {
                    ConnState::Connecting => TransitionResult::NextState {
                        state: ConnState::Connected,
                        data: ConnData {
                            attempts: data.attempts,
                            messages_sent: data.messages_sent,
                        },
                        actions: vec![],
                    },
                    ConnState::Connected => {
                        data.messages_sent += 1;
                        TransitionResult::KeepState {
                            data: ConnData {
                                attempts: data.attempts,
                                messages_sent: data.messages_sent,
                            },
                            actions: vec![],
                        }
                    }
                    ConnState::Disconnected => {
                        TransitionResult::KeepStateAndData { actions: vec![] }
                    }
                },
                EventType::StateTimeout => {
                    if *state == ConnState::Connecting && data.attempts >= 3 {
                        TransitionResult::NextState {
                            state: ConnState::Disconnected,
                            data: ConnData {
                                attempts: data.attempts,
                                messages_sent: data.messages_sent,
                            },
                            actions: vec![],
                        }
                    } else {
                        TransitionResult::KeepStateAndData { actions: vec![] }
                    }
                }
                EventType::Call(reply_tx) => {
                    let status = format!(
                        "state={state:?} attempts={} sent={}",
                        data.attempts, data.messages_sent
                    );
                    TransitionResult::KeepStateAndData {
                        actions: vec![Action::Reply(reply_tx, status)],
                    }
                }
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }
    }

    #[tokio::test]
    async fn connection_state_machine_example() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), ConnectionStatem).await;

        for _ in 0..1000 {
            let status = statem_ref
                .call("status".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if status.contains("attempts=1") { break; }
            tokio::task::yield_now().await;
        }
        let status = statem_ref
            .call("status".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(status.contains("Connecting"), "status: {status}");
        assert!(status.contains("attempts=1"), "status: {status}");

        statem_ref.cast("connected".to_string()).unwrap();
        for _ in 0..1000 {
            let status = statem_ref
                .call("status".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if status.contains("Connected") { break; }
            tokio::task::yield_now().await;
        }

        let status = statem_ref
            .call("status".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(status.contains("Connected"), "status: {status}");

        statem_ref.cast("send_data".to_string()).unwrap();
        statem_ref.cast("send_data".to_string()).unwrap();
        for _ in 0..1000 {
            let status = statem_ref
                .call("status".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if status.contains("sent=2") { break; }
            tokio::task::yield_now().await;
        }

        let status = statem_ref
            .call("status".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(status.contains("sent=2"), "status: {status}");
    }

    // ========================================================================
    // 27. statem_multiple_casts
    // ========================================================================
    #[tokio::test]
    async fn statem_multiple_casts() {
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(Arc::clone(&rt), CounterStatem).await;

        for _ in 0..10 {
            statem_ref.cast("inc".to_string()).unwrap();
        }
        for _ in 0..1000 {
            let count = statem_ref
                .call("get".to_string(), Duration::from_secs(1))
                .await
                .unwrap();
            if count == 10 { break; }
            tokio::task::yield_now().await;
        }

        let count = statem_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 10);
    }

    // ========================================================================
    // 28. statem_init_failure_does_not_crash_runtime
    // ========================================================================
    struct FailingInitStatem;

    #[async_trait::async_trait]
    impl GenStatem for FailingInitStatem {
        type State = &'static str;
        type Data = ();
        type Call = ();
        type Cast = ();
        type Reply = ();

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Err("init failed".to_string())
        }

        async fn handle_event(
            &self,
            _event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            TransitionResult::KeepStateAndData { actions: vec![] }
        }
    }

    #[tokio::test]
    async fn statem_init_failure_does_not_crash_runtime() {
        let rt = Arc::new(Runtime::new(1));
        let _statem_ref = spawn_gen_statem(Arc::clone(&rt), FailingInitStatem).await;

        let counter_ref = spawn_gen_statem(Arc::clone(&rt), CounterStatem).await;
        let count = counter_ref
            .call("get".to_string(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    // ========================================================================
    // 29. statem_stop_with_reason
    // ========================================================================
    struct StopReasonStatem {
        terminated_reason: Arc<tokio::sync::Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl GenStatem for StopReasonStatem {
        type State = &'static str;
        type Data = ();
        type Call = String;
        type Cast = String;
        type Reply = String;

        fn callback_mode(&self) -> (CallbackMode, bool) {
            (CallbackMode::HandleEventFunction, false)
        }

        async fn init(&self) -> Result<(Self::State, Self::Data), String> {
            Ok(("alive", ()))
        }

        async fn handle_event(
            &self,
            event_type: EventType<Self::Reply>,
            _event: rmpv::Value,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
            match event_type {
                EventType::Cast => TransitionResult::Stop {
                    reason: ExitReason::Abnormal("test_stop".to_string()),
                    data: (),
                },
                _ => TransitionResult::KeepStateAndData { actions: vec![] },
            }
        }

        async fn terminate(
            &self,
            reason: ExitReason,
            _state: &Self::State,
            _data: &mut Self::Data,
        ) {
            let reason_str = format!("{reason:?}");
            *self.terminated_reason.lock().await = Some(reason_str);
        }
    }

    #[tokio::test]
    async fn statem_stop_with_reason() {
        let terminated_reason = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let rt = Arc::new(Runtime::new(1));
        let statem_ref = spawn_gen_statem(
            Arc::clone(&rt),
            StopReasonStatem {
                terminated_reason: Arc::clone(&terminated_reason),
            },
        )
        .await;

        statem_ref.cast("stop".to_string()).unwrap();

        for _ in 0..1000 {
            if terminated_reason.lock().await.is_some() { break; }
            tokio::task::yield_now().await;
        }

        let reason = terminated_reason.lock().await.clone();
        assert!(reason.is_some());
        assert!(reason.as_ref().unwrap().contains("test_stop"));
    }

    // ========================================================================
    // 30. callback_mode_returns_correct_values
    // ========================================================================
    #[test]
    fn callback_mode_returns_correct_values() {
        let statem = CounterStatem;
        let (mode, enter) = statem.callback_mode();
        assert_eq!(mode, CallbackMode::HandleEventFunction);
        assert!(!enter);
    }

    // ========================================================================
    // 31. event_type_debug_formatting
    // ========================================================================
    #[test]
    fn event_type_debug_formatting() {
        let et: EventType<String> = EventType::Cast;
        let debug = format!("{et:?}");
        assert!(debug.contains("Cast"));

        let et: EventType<String> = EventType::StateTimeout;
        let debug = format!("{et:?}");
        assert!(debug.contains("StateTimeout"));

        let et: EventType<String> = EventType::Enter {
            old_state_name: "OldState".to_string(),
        };
        let debug = format!("{et:?}");
        assert!(debug.contains("OldState"));
    }

    // ========================================================================
    // 32. timeout_kind_equality
    // ========================================================================
    #[test]
    fn timeout_kind_equality() {
        assert_eq!(TimeoutKind::State, TimeoutKind::State);
        assert_eq!(TimeoutKind::Event, TimeoutKind::Event);
        assert_eq!(
            TimeoutKind::Generic("a".to_string()),
            TimeoutKind::Generic("a".to_string())
        );
        assert_ne!(TimeoutKind::State, TimeoutKind::Event);
        assert_ne!(
            TimeoutKind::Generic("a".to_string()),
            TimeoutKind::Generic("b".to_string())
        );
    }

    // ========================================================================
    // 33. action_debug_formatting
    // ========================================================================
    #[test]
    fn action_debug_formatting() {
        let action: Action<&str, String> = Action::Postpone;
        let debug = format!("{action:?}");
        assert!(debug.contains("Postpone"));

        let action: Action<&str, String> = Action::Hibernate;
        let debug = format!("{action:?}");
        assert!(debug.contains("Hibernate"));
    }

    // ========================================================================
    // 34. transition_result_debug_formatting
    // ========================================================================
    #[test]
    fn transition_result_debug_formatting() {
        let result: TransitionResult<&str, (), String> = TransitionResult::NextState {
            state: "new",
            data: (),
            actions: vec![],
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("NextState"));

        let result: TransitionResult<&str, (), String> = TransitionResult::Stop {
            reason: ExitReason::Normal,
            data: (),
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("Stop"));
    }
}
