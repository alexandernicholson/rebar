# Delayed and Periodic Messages with Timer

The `timer` module provides functions for sending messages after a delay or at regular intervals. When you need a heartbeat, a scheduled retry, or a deferred cleanup, timers deliver messages through the normal routing system without busy-waiting or manual `tokio::time::sleep` loops.

---

## Table of Contents

1. [Why Timer](#1-why-timer)
2. [How: A Heartbeat Monitor](#2-how-a-heartbeat-monitor)
3. [Key Points](#3-key-points)
4. [See Also](#4-see-also)

---

## 1. Why Timer

Consider a service that monitors a downstream dependency by sending periodic pings. If three consecutive pings go unanswered, it escalates to an alerting system. You need:

- **Periodic ticks** to trigger the ping.
- **Delayed messages** for the "no response within 5 seconds" timeout.
- **Cancellation** when a response arrives so the timeout does not fire.
- **Integration with process mailboxes** so timers feel like normal messages, not a separate scheduling system.

Raw `tokio::time::sleep` in a loop works for simple cases, but it does not integrate with the process model. Timer messages arrive in a process's mailbox alongside regular messages, so a GenServer's `handle_info` or a spawned process's `recv` handles them naturally.

---

## 2. How: A Heartbeat Monitor

### Standalone timer functions

The `timer` module exposes four functions that work with the message router directly.

#### send_after: one-shot delayed message

```rust
use std::sync::Arc;
use std::time::Duration;

use rebar_core::process::ProcessId;
use rebar_core::router::MessageRouter;
use rebar_core::timer::{send_after, TimerRef};

fn schedule_timeout(
    router: Arc<dyn MessageRouter>,
    from: ProcessId,
    dest: ProcessId,
) -> TimerRef {
    // Send a "timeout" message to dest after 5 seconds.
    send_after(
        router,
        from,
        dest,
        rmpv::Value::String("timeout".into()),
        Duration::from_secs(5),
    )
}
```

The returned `TimerRef` can cancel the timer before it fires:

```rust
fn handle_response_received(pending_timeout: &TimerRef) {
    // Response arrived -- cancel the timeout.
    pending_timeout.cancel();
    assert!(pending_timeout.is_finished());
}
```

Cancellation is idempotent. Calling `cancel()` on a timer that already fired or was already cancelled is a no-op.

#### send_interval: periodic messages

```rust
use rebar_core::timer::send_interval;

fn start_heartbeat(
    router: Arc<dyn MessageRouter>,
    monitor_pid: ProcessId,
) -> TimerRef {
    // Send a "ping" to the monitor every 10 seconds.
    // The first message arrives after 10 seconds (not immediately).
    send_interval(
        router,
        monitor_pid, // from
        monitor_pid, // dest (self)
        rmpv::Value::String("ping".into()),
        Duration::from_secs(10),
    )
}
```

The interval stops automatically if the destination process is dead (the router returns an error). Cancel the timer explicitly when shutting down to avoid orphaned tasks.

#### apply_after: run a function after a delay

When you need to execute logic rather than send a message:

```rust
use rebar_core::timer::apply_after;

fn schedule_cleanup() -> TimerRef {
    apply_after(Duration::from_secs(60), || async {
        println!("running deferred cleanup");
        // Perform cleanup logic here.
    })
}
```

#### apply_interval: run a function periodically

```rust
use rebar_core::timer::apply_interval;

fn start_metrics_flush() -> TimerRef {
    apply_interval(Duration::from_secs(30), || async {
        println!("flushing metrics to backend");
    })
}
```

Each invocation waits for the previous one to complete before scheduling the next, so you do not get overlapping executions.

### ProcessContext timer methods

Inside a spawned process, `ProcessContext` provides convenience methods that target the process's own mailbox:

```rust
use rebar_core::runtime::Runtime;

async fn heartbeat_process(rt: &Runtime) {
    rt.spawn(|mut ctx| async move {
        let _interval = ctx.send_interval(
            rmpv::Value::String("tick".into()),
            Duration::from_secs(5),
        );
        let _startup = ctx.send_after(
            rmpv::Value::String("startup_complete".into()),
            Duration::from_secs(1),
        );

        while let Some(msg) = ctx.recv().await {
            match msg.payload().as_str() {
                Some("tick") => println!("heartbeat tick"),
                Some("startup_complete") => println!("startup finished"),
                _ => {}
            }
        }
    })
    .await;
}
```

Use `send_after_to` to target another process instead of self.

### GenServerContext timer methods

Inside a GenServer, the context provides `send_interval`, `send_after`, and `send_after_self`. Timer messages arrive in `handle_info` as raw `Message` values.

### Putting it together: a heartbeat monitor

A GenServer that sends periodic pings and escalates on missed responses:

```rust
use rebar_core::gen_server::*;
use rebar_core::process::{Message, ProcessId};

struct HeartbeatMonitor { target: ProcessId, max_missed: u32 }

struct MonitorState {
    missed: u32,
    timeout_timer: Option<rebar_core::timer::TimerRef>,
}

#[async_trait::async_trait]
impl GenServer for HeartbeatMonitor {
    type State = MonitorState;
    type Call = String;
    type Cast = String;
    type Reply = String;

    async fn init(&self, ctx: &GenServerContext) -> Result<Self::State, String> {
        ctx.send_interval(
            ctx.self_pid(),
            rmpv::Value::String("do_ping".into()),
            Duration::from_secs(10),
        );
        Ok(MonitorState { missed: 0, timeout_timer: None })
    }

    async fn handle_call(
        &self, _msg: String, _from: ProcessId,
        state: &mut MonitorState, _ctx: &GenServerContext,
    ) -> String { format!("missed: {}", state.missed) }

    async fn handle_cast(
        &self, msg: String, state: &mut MonitorState, _ctx: &GenServerContext,
    ) {
        if msg == "pong" {
            if let Some(timer) = state.timeout_timer.take() { timer.cancel(); }
            state.missed = 0;
        }
    }

    async fn handle_info(
        &self, msg: Message, state: &mut MonitorState, ctx: &GenServerContext,
    ) {
        match msg.payload().as_str() {
            Some("do_ping") => {
                let _ = ctx.send(self.target, rmpv::Value::String("ping".into()));
                state.timeout_timer = ctx.send_after_self(
                    rmpv::Value::String("ping_timeout".into()),
                    Duration::from_secs(5),
                );
            }
            Some("ping_timeout") => {
                state.missed += 1;
                if state.missed >= self.max_missed {
                    println!("ALERT: {} missed heartbeats", state.missed);
                }
            }
            _ => {}
        }
    }
}
```

---

## 3. Key Points

- **`send_after` fires once.** The timer task completes after delivering the message. The `TimerRef` shows `is_finished() == true` afterward.
- **`send_interval` fires repeatedly.** The first message arrives after one interval, not immediately. The interval stops if the destination dies or the timer is cancelled.
- **`cancel()` is idempotent.** Calling it on an already-fired or already-cancelled timer is safe.
- **`TimerRef` is `Clone`.** Multiple owners can hold a reference to the same timer. Cancelling from any clone cancels the timer.
- **`apply_after` and `apply_interval` run closures.** Use these when you need to execute logic, not send a message. `apply_interval` waits for each invocation to complete before starting the next.
- **`ProcessContext` methods target self by default.** `send_after` and `send_interval` on `ProcessContext` send to the process's own mailbox. Use `send_after_to` to target another process.
- **`GenServerContext` methods require an explicit destination.** `send_after` and `send_interval` on `GenServerContext` take a `dest: ProcessId` parameter. Use `send_after_self` for self-targeted timers.
- **Timer messages are raw.** They arrive as `rmpv::Value` payloads in `handle_info` (GenServer) or `recv` (spawned process). Use a tag string or structured value to distinguish timer messages from other info messages.

---

## 4. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `send_after`, `send_interval`, `apply_after`, `apply_interval`, `TimerRef`
- [Building Stateful Services with GenServer](gen-server.md) -- using timers inside `init` and `handle_info`
- [State Machines with GenStatem](gen-statem.md) -- state timeouts and event timeouts built into the state machine
