# Building Stateful Services with GenServer

A `GenServer` is a process that holds mutable state and handles synchronous calls, asynchronous casts, and raw messages in a single event loop. If you need a long-lived service that other parts of your system interact with through a typed request/response protocol, GenServer is the right abstraction.

---

## Table of Contents

1. [Why GenServer](#1-why-genserver)
2. [How: A Session Store](#2-how-a-session-store)
3. [Key Points](#3-key-points)
4. [See Also](#4-see-also)

---

## 1. Why GenServer

Consider a session store that tracks active user sessions. Multiple request handlers need to read sessions, touch them to keep them alive, and expired sessions need to be reaped periodically. Without GenServer, you would use an `Arc<Mutex<HashMap<...>>>` shared across tasks. That works, but you lose:

- **Serialized access.** A GenServer processes one message at a time, eliminating data races by design. There is no lock contention because there is no lock.
- **Typed protocol.** Associated types (`Call`, `Cast`, `Reply`) define a compile-time contract between the server and its callers. The wrong message type is a compile error, not a runtime panic.
- **Lifecycle hooks.** `init` for setup, `handle_info` for timer ticks, `handle_continue` for deferred work, `terminate` for cleanup. The event loop is built for you.
- **Debuggability.** `sys_get_state` lets you inspect the server's state in production without stopping it.

---

## 2. How: A Session Store

### Defining the server

The `GenServer` trait requires four associated types and three mandatory callbacks. Here is a session store that tracks active sessions with an expiry timer.

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rebar_core::gen_server::{
    spawn_gen_server, CallError, GenServer, GenServerContext, GenServerRef,
};
use rebar_core::process::ProcessId;
use rebar_core::runtime::Runtime;

/// The state: a map of session ID to (user ID, last-touched time).
type SessionMap = HashMap<String, (String, Instant)>;

/// Synchronous requests.
enum SessionCall {
    Get(String),
}

/// Asynchronous notifications.
enum SessionCast {
    Touch(String),
    Remove(String),
}

/// Replies to calls.
enum SessionReply {
    Found(String),
    NotFound,
}

struct SessionStore {
    expiry: Duration,
}

#[async_trait::async_trait]
impl GenServer for SessionStore {
    type State = SessionMap;
    type Call = SessionCall;
    type Cast = SessionCast;
    type Reply = SessionReply;

    async fn init(&self, ctx: &GenServerContext) -> Result<Self::State, String> {
        // Schedule periodic expiry sweep every 30 seconds.
        ctx.send_interval(
            ctx.self_pid(),
            rmpv::Value::String("sweep".into()),
            Duration::from_secs(30),
        );
        // Defer heavy initialization to handle_continue so init returns fast.
        ctx.continue_with(rmpv::Value::String("load_from_db".into()));
        Ok(HashMap::new())
    }

    async fn handle_call(
        &self,
        msg: Self::Call,
        _from: ProcessId,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) -> Self::Reply {
        match msg {
            SessionCall::Get(session_id) => match state.get(&session_id) {
                Some((user_id, _)) => SessionReply::Found(user_id.clone()),
                None => SessionReply::NotFound,
            },
        }
    }

    async fn handle_cast(
        &self,
        msg: Self::Cast,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
        match msg {
            SessionCast::Touch(session_id) => {
                if let Some(entry) = state.get_mut(&session_id) {
                    entry.1 = Instant::now();
                }
            }
            SessionCast::Remove(session_id) => {
                state.remove(&session_id);
            }
        }
    }

    async fn handle_info(
        &self,
        msg: rebar_core::process::Message,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
        // Timer messages arrive here as raw info messages.
        if msg.payload().as_str() == Some("sweep") {
            let cutoff = Instant::now() - self.expiry;
            state.retain(|_, (_, touched)| *touched > cutoff);
        }
    }

    async fn handle_continue(
        &self,
        msg: rmpv::Value,
        state: &mut Self::State,
        _ctx: &GenServerContext,
    ) {
        if msg.as_str() == Some("load_from_db") {
            // Simulate loading existing sessions from a database.
            // This runs before the next call/cast is processed,
            // so callers see a fully initialized server.
            state.insert(
                "abc123".into(),
                ("user_42".into(), Instant::now()),
            );
        }
    }
}
```

### Starting the server and interacting with it

```rust
async fn example() {
    let rt = Arc::new(Runtime::new(1));

    let server: GenServerRef<SessionStore> = spawn_gen_server(
        Arc::clone(&rt),
        SessionStore {
            expiry: Duration::from_secs(300),
        },
    )
    .await;

    // Synchronous call: get a session. Blocks until the server replies.
    let reply = server
        .call(SessionCall::Get("abc123".into()), Duration::from_secs(1))
        .await;
    match reply {
        Ok(SessionReply::Found(user_id)) => println!("session belongs to {user_id}"),
        Ok(SessionReply::NotFound) => println!("session expired or invalid"),
        Err(CallError::Timeout) => println!("server overloaded"),
        Err(CallError::ServerDead) => println!("server crashed"),
    }

    // Asynchronous cast: touch a session. Returns immediately.
    let _ = server.cast(SessionCast::Touch("abc123".into()));
}
```

### Debugging production state

`sys_get_state` clones the server's state without stopping the event loop. The state type must implement `Clone`.

```rust
async fn debug_sessions(server: &GenServerRef<SessionStore>) {
    match server.sys_get_state(Duration::from_secs(1)).await {
        Ok(sessions) => {
            for (id, (user, touched)) in &sessions {
                println!("  session {id}: user={user}, age={:?}", touched.elapsed());
            }
        }
        Err(e) => eprintln!("could not inspect state: {e}"),
    }
}
```

You can also suspend and resume the server to prevent it from processing messages while you inspect state:

```rust
async fn inspect_safely(server: &GenServerRef<SessionStore>) {
    server.sys_suspend(Duration::from_secs(1)).await.unwrap();
    let state = server.sys_get_state(Duration::from_secs(1)).await.unwrap();
    println!("sessions while suspended: {}", state.len());
    server.sys_resume(Duration::from_secs(1)).await.unwrap();
}
```

---

## 3. Key Points

- **One message at a time.** The event loop is sequential. No locks, no races. State is only ever accessed by one callback at a time.
- **Calls have timeouts.** `call()` takes a `Duration`. If the server is overloaded or stuck, the caller gets `CallError::Timeout` instead of waiting forever.
- **Casts are fire-and-forget.** `cast()` returns immediately. The message is queued and processed later. Use casts for operations where the caller does not need a response.
- **`handle_info` handles raw messages.** Timer ticks, messages from other processes via `ctx.send()`, and any other non-typed messages arrive here.
- **`handle_continue` runs before the next mailbox message.** Use it for deferred initialization or multi-step processing. Call `ctx.continue_with(payload)` from any callback.
- **`sys_get_state` requires `State: Clone`.** The state is cloned inside the event loop and sent back to the caller. If your state is expensive to clone, consider returning a summary instead.
- **`GenServerRef` is `Clone + Send + Sync`.** Pass it to any number of tasks. Each clone shares the same channels to the server.

---

## 4. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `GenServer`, `GenServerContext`, `GenServerRef`, `CallError`
- [Shared State with Agent](agents.md) -- when you need simple get/update without a custom message protocol
- [Delayed and Periodic Messages with Timer](timers.md) -- the timer functions used by `GenServerContext::send_after` and `send_interval`
