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
}

type BoxedGetFn = Box<dyn FnOnce(&dyn Any) -> Box<dyn Any + Send> + Send>;
type BoxedUpdateFn = Box<dyn FnOnce(&mut dyn Any) + Send>;
type BoxedGetAndUpdateFn = Box<dyn FnOnce(&mut dyn Any) -> Box<dyn Any + Send> + Send>;

enum AgentMsg {
    Get {
        f: BoxedGetFn,
        reply_tx: oneshot::Sender<Box<dyn Any + Send>>,
    },
    Update {
        f: BoxedUpdateFn,
        reply_tx: oneshot::Sender<()>,
    },
    GetAndUpdate {
        f: BoxedGetAndUpdateFn,
        reply_tx: oneshot::Sender<Box<dyn Any + Send>>,
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
        .spawn(move |_ctx| async move {
            let mut state: Box<dyn Any + Send> = Box::new(init());

            while let Some(msg) = msg_rx.recv().await {
                match msg {
                    AgentMsg::Get { f, reply_tx } => {
                        let result = f(state.as_ref());
                        let _ = reply_tx.send(result);
                    }
                    AgentMsg::Update { f, reply_tx } => {
                        f(state.as_mut());
                        let _ = reply_tx.send(());
                    }
                    AgentMsg::GetAndUpdate { f, reply_tx } => {
                        let result = f(state.as_mut());
                        let _ = reply_tx.send(result);
                    }
                    AgentMsg::Cast { f } => {
                        f(state.as_mut());
                    }
                    AgentMsg::Stop { reply_tx } => {
                        let _ = reply_tx.send(());
                        break;
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
    /// or `AgentError::Dead` if the agent has shut down.
    ///
    /// # Panics
    ///
    /// Panics if the state type `S` does not match the type used to create the agent.
    pub async fn get<S, F, T>(&self, f: F, timeout: Duration) -> Result<T, AgentError>
    where
        S: Send + 'static,
        F: FnOnce(&S) -> T + Send + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let boxed_f: BoxedGetFn = Box::new(move |any_state| {
            let state = any_state.downcast_ref::<S>().expect("agent state type mismatch");
            Box::new(f(state))
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
            .map_err(|_| AgentError::Dead)?;

        Ok(*result.downcast::<T>().expect("agent reply type mismatch"))
    }

    /// Update the agent state via a function.
    ///
    /// The function receives a mutable reference to the state.
    /// Equivalent to Elixir's `Agent.update/3`.
    ///
    /// # Errors
    ///
    /// Returns `AgentError::Timeout` or `AgentError::Dead`.
    ///
    /// # Panics
    ///
    /// Panics if the state type `S` does not match the type used to create the agent.
    pub async fn update<S, F>(&self, f: F, timeout: Duration) -> Result<(), AgentError>
    where
        S: Send + 'static,
        F: FnOnce(&mut S) + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let boxed_f: BoxedUpdateFn = Box::new(move |any_state| {
            let state = any_state.downcast_mut::<S>().expect("agent state type mismatch");
            f(state);
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
            .map_err(|_| AgentError::Dead)?;

        Ok(())
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
    /// Returns `AgentError::Timeout` or `AgentError::Dead`.
    ///
    /// # Panics
    ///
    /// Panics if the state type `S` does not match the type used to create the agent.
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
            let state = any_state.downcast_mut::<S>().expect("agent state type mismatch");
            Box::new(f(state))
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
            .map_err(|_| AgentError::Dead)?;

        Ok(*result.downcast::<T>().expect("agent reply type mismatch"))
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
    /// # Panics
    ///
    /// Panics if the state type `S` does not match the type used to create the agent.
    pub fn cast<S, F>(&self, f: F) -> Result<(), AgentError>
    where
        S: Send + 'static,
        F: FnOnce(&mut S) + Send + 'static,
    {
        let boxed_f: BoxedUpdateFn = Box::new(move |any_state| {
            let state = any_state.downcast_mut::<S>().expect("agent state type mismatch");
            f(state);
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
