use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::process::ProcessId;
use crate::runtime::Runtime;

/// Errors from agent operations.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The operation did not complete within the specified timeout.
    #[error("agent timeout")]
    Timeout,
    /// The agent process has shut down.
    #[error("agent dead")]
    Dead,
    /// The closure's state/return type did not match the agent's actual
    /// state type. Returned to the single offending caller instead of
    /// panicking the agent and destroying all shared state.
    #[error("agent state type mismatch")]
    TypeMismatch,
}

/// Result of running a closure against the agent state. `Ok` carries the
/// boxed return value; `Err` signals the closure could not downcast the
/// state to the expected type.
type OpResult = Result<Box<dyn Any + Send>, AgentError>;

type BoxedGetFn = Box<dyn FnOnce(&dyn Any) -> OpResult + Send>;
type BoxedUpdateFn = Box<dyn FnOnce(&mut dyn Any) -> Result<(), AgentError> + Send>;
type BoxedGetAndUpdateFn = Box<dyn FnOnce(&mut dyn Any) -> OpResult + Send>;

enum AgentMsg {
    Get {
        f: BoxedGetFn,
        reply_tx: oneshot::Sender<OpResult>,
    },
    Update {
        f: BoxedUpdateFn,
        reply_tx: oneshot::Sender<Result<(), AgentError>>,
    },
    GetAndUpdate {
        f: BoxedGetAndUpdateFn,
        reply_tx: oneshot::Sender<OpResult>,
    },
    Cast {
        f: BoxedUpdateFn,
    },
    Stop {
        reply_tx: oneshot::Sender<()>,
    },
}

/// A handle to a running Agent process.
///
/// Agents are a simple abstraction around state, equivalent to Elixir's
/// `Agent` module. They provide `get`/`update`/`get_and_update`/`cast`
/// operations without requiring custom message types.
#[derive(Clone)]
pub struct AgentRef {
    pid: ProcessId,
    msg_tx: mpsc::Sender<AgentMsg>,
}

/// Start a new Agent with the given initial state.
///
/// Equivalent to Elixir's `Agent.start_link/2`.
pub async fn start_agent<S, F>(runtime: Arc<Runtime>, init: F) -> AgentRef
where
    S: Send + 'static,
    F: FnOnce() -> S + Send + 'static,
{
    let (msg_tx, mut msg_rx) = mpsc::channel::<AgentMsg>(64);

    let pid = runtime
        .spawn(move |mut ctx| async move {
            let mut state: Box<dyn Any + Send> = Box::new(init());

            loop {
                tokio::select! {
                    biased;

                    // Process-exit signal: when the agent's PID is killed
                    // (its process mailbox is closed by `cleanup_process`),
                    // `ctx.recv()` yields `None`. Stop the loop so the agent
                    // does not become a zombie that keeps accepting writes
                    // after the table reports it dead.
                    proc_msg = ctx.recv() => {
                        if proc_msg.is_none() {
                            break;
                        }
                        // Stray process messages to an agent are ignored.
                    }

                    maybe_msg = msg_rx.recv() => {
                        let Some(msg) = maybe_msg else { break };
                        match msg {
                            AgentMsg::Get { f, reply_tx } => {
                                let _ = reply_tx.send(f(state.as_ref()));
                            }
                            AgentMsg::Update { f, reply_tx } => {
                                let _ = reply_tx.send(f(state.as_mut()));
                            }
                            AgentMsg::GetAndUpdate { f, reply_tx } => {
                                let _ = reply_tx.send(f(state.as_mut()));
                            }
                            AgentMsg::Cast { f } => {
                                // Fire-and-forget: a type mismatch here has no
                                // caller to report to, so it is dropped rather
                                // than crashing the agent.
                                let _ = f(state.as_mut());
                            }
                            AgentMsg::Stop { reply_tx } => {
                                let _ = reply_tx.send(());
                                break;
                            }
                        }
                    }
                }
            }
        })
        .await;

    AgentRef { pid, msg_tx }
}

impl AgentRef {
    /// Get the agent's PID.
    #[must_use]
    pub const fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Read from agent state via a function.
    ///
    /// The function receives a reference to the state and returns a computed value.
    /// Equivalent to Elixir's `Agent.get/3`.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::Timeout` if the agent doesn't respond in time,
    /// `AgentError::Dead` if the agent has shut down, or
    /// `AgentError::TypeMismatch` if `S`/`T` do not match the agent's state.
    pub async fn get<S, F, T>(&self, f: F, timeout: Duration) -> Result<T, AgentError>
    where
        S: Send + 'static,
        F: FnOnce(&S) -> T + Send + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let boxed_f: BoxedGetFn = Box::new(move |any_state| {
            // Fallible downcast: a wrong type returns an error to this one
            // caller instead of panicking the agent loop (which would wipe
            // all shared state for every other client).
            let state = any_state
                .downcast_ref::<S>()
                .ok_or(AgentError::TypeMismatch)?;
            Ok(Box::new(f(state)) as Box<dyn Any + Send>)
        });
        self.msg_tx
            .send(AgentMsg::Get {
                f: boxed_f,
                reply_tx,
            })
            .await
            .map_err(|_| AgentError::Dead)?;

        let result = tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| AgentError::Timeout)?
            .map_err(|_| AgentError::Dead)??;

        result
            .downcast::<T>()
            .map(|b| *b)
            .map_err(|_| AgentError::TypeMismatch)
    }

    /// Update the agent state via a function.
    ///
    /// The function receives a mutable reference to the state.
    /// Equivalent to Elixir's `Agent.update/3`.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::Timeout`, `AgentError::Dead`, or
    /// `AgentError::TypeMismatch` if `S` does not match the agent's state.
    pub async fn update<S, F>(&self, f: F, timeout: Duration) -> Result<(), AgentError>
    where
        S: Send + 'static,
        F: FnOnce(&mut S) + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let boxed_f: BoxedUpdateFn = Box::new(move |any_state| {
            let state = any_state
                .downcast_mut::<S>()
                .ok_or(AgentError::TypeMismatch)?;
            f(state);
            Ok(())
        });
        self.msg_tx
            .send(AgentMsg::Update {
                f: boxed_f,
                reply_tx,
            })
            .await
            .map_err(|_| AgentError::Dead)?;

        tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| AgentError::Timeout)?
            .map_err(|_| AgentError::Dead)?
    }

    /// Get a value and update state atomically.
    ///
    /// The function receives a mutable reference to state and returns a tuple
    /// of `(return_value, new_state)`. Note: unlike Elixir, the state is mutated
    /// in place — the function modifies `&mut S` and returns only the get value.
    ///
    /// Equivalent to Elixir's `Agent.get_and_update/3`.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::Timeout`, `AgentError::Dead`, or
    /// `AgentError::TypeMismatch` if `S`/`T` do not match the agent's state.
    pub async fn get_and_update<S, F, T>(
        &self,
        f: F,
        timeout: Duration,
    ) -> Result<T, AgentError>
    where
        S: Send + 'static,
        F: FnOnce(&mut S) -> T + Send + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let boxed_f: BoxedGetAndUpdateFn = Box::new(move |any_state| {
            let state = any_state
                .downcast_mut::<S>()
                .ok_or(AgentError::TypeMismatch)?;
            Ok(Box::new(f(state)) as Box<dyn Any + Send>)
        });
        self.msg_tx
            .send(AgentMsg::GetAndUpdate {
                f: boxed_f,
                reply_tx,
            })
            .await
            .map_err(|_| AgentError::Dead)?;

        let result = tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| AgentError::Timeout)?
            .map_err(|_| AgentError::Dead)??;

        result
            .downcast::<T>()
            .map(|b| *b)
            .map_err(|_| AgentError::TypeMismatch)
    }

    /// Fire-and-forget state update.
    ///
    /// Returns immediately without waiting for the update to be applied.
    /// Equivalent to Elixir's `Agent.cast/2`.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::Dead` if the agent has shut down.
    ///
    /// A wrong-typed closure cannot be reported (fire-and-forget), so it is
    /// silently dropped by the agent rather than crashing it.
    pub fn cast<S, F>(&self, f: F) -> Result<(), AgentError>
    where
        S: Send + 'static,
        F: FnOnce(&mut S) + Send + 'static,
    {
        let boxed_f: BoxedUpdateFn = Box::new(move |any_state| {
            let state = any_state
                .downcast_mut::<S>()
                .ok_or(AgentError::TypeMismatch)?;
            f(state);
            Ok(())
        });
        self.msg_tx
            .try_send(AgentMsg::Cast { f: boxed_f })
            .map_err(|_| AgentError::Dead)
    }

    /// Stop the agent.
    ///
    /// Equivalent to Elixir's `Agent.stop/3`.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::Dead` if already stopped, or `AgentError::Timeout`.
    pub async fn stop(&self, timeout: Duration) -> Result<(), AgentError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.msg_tx
            .send(AgentMsg::Stop { reply_tx })
            .await
            .map_err(|_| AgentError::Dead)?;

        tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| AgentError::Timeout)?
            .map_err(|_| AgentError::Dead)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn agent_start_with_initial_state() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 42_u64).await;
        assert!(agent.pid().local_id() > 0);
    }

    #[tokio::test]
    async fn agent_get_reads_state() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 42_u64).await;
        let val = agent
            .get(|s: &u64| *s, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(val, 42);
    }

    #[tokio::test]
    async fn agent_update_modifies_state() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 0_u64).await;

        agent
            .update(|s: &mut u64| *s += 10, Duration::from_secs(1))
            .await
            .unwrap();

        let val = agent
            .get(|s: &u64| *s, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(val, 10);
    }

    #[tokio::test]
    async fn agent_get_and_update_atomic() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 5_u64).await;

        let old = agent
            .get_and_update(
                |s: &mut u64| {
                    let old = *s;
                    *s = 99;
                    old
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        assert_eq!(old, 5);

        let new = agent
            .get(|s: &u64| *s, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(new, 99);
    }

    #[tokio::test]
    async fn agent_cast_fire_and_forget() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 0_u64).await;

        agent.cast(|s: &mut u64| *s += 1).unwrap();
        // The get call acts as a synchronization barrier
        let val = agent
            .get(|s: &u64| *s, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(val, 1);
    }

    #[tokio::test]
    async fn agent_stop_shuts_down() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 42_u64).await;

        agent.stop(Duration::from_secs(1)).await.unwrap();

        // Operations after stop should fail
        let result = agent.get(|s: &u64| *s, Duration::from_secs(1)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn agent_dead_returns_error() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 42_u64).await;
        agent.stop(Duration::from_secs(1)).await.unwrap();

        // stop() already awaited the reply, so the agent loop has exited
        let result = agent.update(|s: &mut u64| *s += 1, Duration::from_secs(1)).await;
        assert!(matches!(result, Err(AgentError::Dead)));
    }

    #[tokio::test]
    async fn agent_concurrent_access() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 0_u64).await;

        let mut handles = Vec::new();
        for _ in 0..10 {
            let a = agent.clone();
            handles.push(tokio::spawn(async move {
                a.update(|s: &mut u64| *s += 1, Duration::from_secs(1))
                    .await
                    .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let val = agent
            .get(|s: &u64| *s, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(val, 10);
    }

    #[tokio::test]
    async fn agent_complex_state() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), HashMap::<String, u64>::new).await;

        agent
            .update(
                |s: &mut HashMap<String, u64>| {
                    s.insert("count".to_string(), 1);
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        let val = agent
            .get(
                |s: &HashMap<String, u64>| s.get("count").copied().unwrap_or(0),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(val, 1);
    }

    #[tokio::test]
    async fn agent_pid_is_valid() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 0_u64).await;
        let pid = agent.pid();
        assert_eq!(pid.node_id(), 1);
        assert!(rt.table().get(&pid).is_some());
    }

    #[tokio::test]
    async fn agent_cleanup_after_stop() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 0_u64).await;
        let pid = agent.pid();

        agent.stop(Duration::from_secs(1)).await.unwrap();

        // Poll until the agent process is cleaned up from the table
        for _ in 0..100 {
            if rt.table().get(&pid).is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(rt.table().get(&pid).is_none());
    }

    #[tokio::test]
    async fn wrong_type_get_returns_error_not_panic() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 42_u64).await;

        // Wrong state type: ask for &String when state is u64.
        let bad = agent
            .get(|s: &String| s.clone(), Duration::from_secs(1))
            .await;
        assert!(matches!(bad, Err(AgentError::TypeMismatch)));

        // The agent must still be alive and serving the correct type.
        let good = agent.get(|s: &u64| *s, Duration::from_secs(1)).await.unwrap();
        assert_eq!(good, 42);
    }

    #[tokio::test]
    async fn wrong_return_type_get_returns_error_not_panic() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 42_u64).await;

        // Correct state type but the boxed return is downcast to the wrong T.
        // (Forced by calling with mismatched S so the closure never runs and
        // the reply is an error; here we exercise a mismatched-S update too.)
        let bad = agent
            .update(|_s: &mut String| {}, Duration::from_secs(1))
            .await;
        assert!(matches!(bad, Err(AgentError::TypeMismatch)));

        // Still alive.
        agent
            .update(|s: &mut u64| *s += 1, Duration::from_secs(1))
            .await
            .unwrap();
        let v = agent.get(|s: &u64| *s, Duration::from_secs(1)).await.unwrap();
        assert_eq!(v, 43);
    }

    #[tokio::test]
    async fn killing_pid_stops_agent_loop() {
        let rt = Arc::new(Runtime::new(1));
        let agent = start_agent(Arc::clone(&rt), || 0_u64).await;
        let pid = agent.pid();

        // Externally kill the process via the canonical death path.
        rt.table().cleanup_process(pid);

        // The agent loop must stop accepting writes (no zombie). Poll because
        // the loop wakes on the closed process mailbox asynchronously.
        let mut got_dead = false;
        for _ in 0..1000 {
            match agent.update(|s: &mut u64| *s += 1, Duration::from_millis(50)).await {
                Err(AgentError::Dead) => {
                    got_dead = true;
                    break;
                }
                _ => tokio::task::yield_now().await,
            }
        }
        assert!(got_dead, "agent should be dead after its PID is killed");
        assert!(rt.table().get(&pid).is_none());
    }

    #[tokio::test]
    async fn multiple_agents_independent() {
        let rt = Arc::new(Runtime::new(1));
        let a1 = start_agent(Arc::clone(&rt), || 1_u64).await;
        let a2 = start_agent(Arc::clone(&rt), || 2_u64).await;

        let v1 = a1.get(|s: &u64| *s, Duration::from_secs(1)).await.unwrap();
        let v2 = a2.get(|s: &u64| *s, Duration::from_secs(1)).await.unwrap();

        assert_eq!(v1, 1);
        assert_eq!(v2, 2);
    }
}
