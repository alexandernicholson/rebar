# State Machines with GenStatem

`GenStatem` is a generic state machine process. When your domain has discrete states with rules about which transitions are allowed, what happens on entry to each state, and what to do when nothing happens for too long, GenStatem encodes those rules in a single `handle_event` callback with first-class support for timeouts, postponement, and state enter events.

---

## Table of Contents

1. [Why GenStatem](#1-why-genstatem)
2. [How: An Order Processing Workflow](#2-how-an-order-processing-workflow)
3. [Key Points](#3-key-points)
4. [See Also](#4-see-also)

---

## 1. Why GenStatem

Consider an order processing system. An order moves through states: Pending, Paid, Shipped, Delivered. The rules:

- A "ship" event only makes sense after payment. If it arrives while the order is still pending, it should be **postponed** until the order is paid.
- Each state has a **timeout**. If an order stays Pending for 24 hours, it should be cancelled. If it stays Shipped for 14 days, escalate to support.
- When entering a new state, you want to **log the transition** for auditing.

You could implement this with a GenServer and a manual state enum, but you would need to:

- Track the current state yourself.
- Implement timeout cancellation on state change.
- Build a postponement queue.
- Emit enter events manually.

GenStatem provides all of this out of the box. State timeouts are cancelled automatically on state change. Postponed events are replayed. Enter callbacks fire on every transition.

---

## 2. How: An Order Processing Workflow

### Defining the state machine

```rust
use std::sync::Arc;
use std::time::Duration;

use rebar_core::gen_statem::{
    spawn_gen_statem, Action, CallbackMode, EventType, GenStatem,
    GenStatemRef, TransitionResult,
};
use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;

#[derive(Debug, Clone, PartialEq)]
enum OrderState {
    Pending,
    Paid,
    Shipped,
    Delivered,
}

struct OrderData {
    order_id: String,
    customer: String,
    amount_cents: u64,
}

struct OrderStatem;

#[async_trait::async_trait]
impl GenStatem for OrderStatem {
    type State = OrderState;
    type Data = OrderData;
    type Call = String;   // query commands
    type Cast = String;   // action commands
    type Reply = String;  // query responses

    fn callback_mode(&self) -> (CallbackMode, bool) {
        // HandleEventFunction: all events go through handle_event.
        // true: enable state enter callbacks.
        (CallbackMode::HandleEventFunction, true)
    }

    async fn init(&self) -> Result<(Self::State, Self::Data), String> {
        Ok((
            OrderState::Pending,
            OrderData {
                order_id: "ORD-1234".into(),
                customer: "alice".into(),
                amount_cents: 4999,
            },
        ))
    }

    async fn handle_event(
        &self,
        event_type: EventType<Self::Reply>,
        event: rmpv::Value,
        state: &Self::State,
        data: &mut Self::Data,
    ) -> TransitionResult<Self::State, Self::Data, Self::Reply> {
        match event_type {
            // -- State enter: log transitions and set timeouts --
            EventType::Enter { old_state_name } => {
                println!(
                    "order {}: {} -> {:?}",
                    data.order_id, old_state_name, state
                );

                let actions = match state {
                    // Cancel pending orders after 24 hours.
                    OrderState::Pending => vec![Action::StateTimeout(
                        Duration::from_secs(86_400),
                        rmpv::Value::String("payment_expired".into()),
                    )],
                    // Escalate stuck shipments after 14 days.
                    OrderState::Shipped => vec![Action::StateTimeout(
                        Duration::from_secs(14 * 86_400),
                        rmpv::Value::String("shipment_stuck".into()),
                    )],
                    _ => vec![],
                };

                TransitionResult::KeepStateAndData { actions }
            }

            // -- Synchronous calls: query order status --
            EventType::Call(reply_tx) => {
                let status = format!(
                    "order {} is {:?} ({}c)",
                    data.order_id, state, data.amount_cents
                );
                TransitionResult::KeepStateAndData {
                    actions: vec![Action::Reply(reply_tx, status)],
                }
            }

            // -- Asynchronous casts: advance the order --
            EventType::Cast => {
                let cmd = event.as_str().unwrap_or("");
                match (state, cmd) {
                    // Payment received while pending: transition to Paid.
                    (OrderState::Pending, "pay") => TransitionResult::NextState {
                        state: OrderState::Paid,
                        data: OrderData {
                            order_id: data.order_id.clone(),
                            customer: data.customer.clone(),
                            amount_cents: data.amount_cents,
                        },
                        actions: vec![],
                    },

                    // Ship command while paid: transition to Shipped.
                    (OrderState::Paid, "ship") => TransitionResult::NextState {
                        state: OrderState::Shipped,
                        data: OrderData {
                            order_id: data.order_id.clone(),
                            customer: data.customer.clone(),
                            amount_cents: data.amount_cents,
                        },
                        actions: vec![],
                    },

                    // Ship command while still pending: postpone.
                    // The event will be replayed when the state changes.
                    (OrderState::Pending, "ship") => {
                        println!(
                            "order {}: postponing 'ship' until payment",
                            data.order_id
                        );
                        TransitionResult::KeepStateAndData {
                            actions: vec![Action::Postpone],
                        }
                    }

                    // Delivery confirmation.
                    (OrderState::Shipped, "deliver") => TransitionResult::NextState {
                        state: OrderState::Delivered,
                        data: OrderData {
                            order_id: data.order_id.clone(),
                            customer: data.customer.clone(),
                            amount_cents: data.amount_cents,
                        },
                        actions: vec![],
                    },

                    // Final state: nothing more to do.
                    (OrderState::Delivered, _) => TransitionResult::KeepStateAndData {
                        actions: vec![],
                    },

                    _ => TransitionResult::KeepStateAndData { actions: vec![] },
                }
            }

            // -- State timeout: handle expiry --
            EventType::StateTimeout => {
                let reason = event.as_str().unwrap_or("timeout");
                println!(
                    "order {}: state timeout in {:?}: {reason}",
                    data.order_id, state
                );
                TransitionResult::Stop {
                    reason: ExitReason::Normal,
                    data: OrderData {
                        order_id: data.order_id.clone(),
                        customer: data.customer.clone(),
                        amount_cents: data.amount_cents,
                    },
                }
            }

            // Ignore other event types.
            _ => TransitionResult::KeepStateAndData { actions: vec![] },
        }
    }

    async fn terminate(
        &self,
        reason: ExitReason,
        state: &Self::State,
        data: &mut Self::Data,
    ) {
        println!(
            "order {} terminated in state {:?}: {reason:?}",
            data.order_id, state
        );
    }
}
```

### Running the state machine

```rust
async fn order_example() {
    let rt = Arc::new(Runtime::new(1));

    let order: GenStatemRef<OrderStatem> =
        spawn_gen_statem(Arc::clone(&rt), OrderStatem).await;

    // Send "ship" before payment -- it will be postponed.
    let _ = order.cast("ship".to_string());

    // Now pay -- transitions Pending -> Paid.
    // The postponed "ship" replays automatically, transitioning Paid -> Shipped.
    let _ = order.cast("pay".to_string());

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Query the current status.
    let status = order.call("status".into(), Duration::from_secs(1)).await;
    println!("status: {status:?}");

    // Mark as delivered.
    let _ = order.cast("deliver".to_string());
}
```

### Actions reference

`TransitionResult` variants carry a `Vec<Action>`. Here are the available actions:

| Action | Effect |
|---|---|
| `Action::Reply(tx, value)` | Send a reply to a caller (must be used exactly once per `EventType::Call`). |
| `Action::StateTimeout(duration, payload)` | Set a timeout that fires if the state does not change within `duration`. Automatically cancelled on state change. |
| `Action::EventTimeout(duration, payload)` | Set a timeout that fires if no external event arrives within `duration`. Cancelled by any call, cast, or info. |
| `Action::GenericTimeout(name, duration, payload)` | Set a named timeout. Independent of state changes and events. Multiple named timeouts can coexist. |
| `Action::CancelTimeout(kind)` | Cancel a specific timeout: `TimeoutKind::State`, `TimeoutKind::Event`, or `TimeoutKind::Generic(name)`. |
| `Action::Postpone` | Re-queue the current event. It will be replayed when the state changes. Only works for external events (casts, info). |
| `Action::NextEvent(payload)` | Insert an internal event at the front of the queue. Processed before the next external event. |

### TransitionResult variants

| Variant | Meaning |
|---|---|
| `NextState { state, data, actions }` | Change state, replace data, execute actions. |
| `KeepState { data, actions }` | Keep the current state, replace data, execute actions. |
| `KeepStateAndData { actions }` | Keep both state and data, execute actions only. |
| `Stop { reason, data }` | Stop the state machine. |
| `StopAndReply { reason, data, replies }` | Send final replies, then stop. |

---

## 3. Key Points

- **One callback handles everything.** `handle_event` receives the event type, event payload, current state, and mutable data. Dispatch on the combination of state and event type.
- **State enter callbacks fire on every transition.** When `callback_mode` returns `(_, true)`, `EventType::Enter` is delivered on state change with the previous state name for logging.
- **State timeouts are cancelled automatically.** When the state changes, the active `StateTimeout` is cancelled. You do not need to cancel it manually.
- **Event timeouts are cancelled by any external event.** They fire only when the system is idle. Use them for "do something if nothing happens for N seconds."
- **Generic timeouts are independent.** They are not cancelled by state changes or events. Use `CancelTimeout(TimeoutKind::Generic(name))` to cancel them manually.
- **Postponement queues events.** Postponed events are replayed in FIFO order after the next state change. Use this for "hold this event until we are in the right state."
- **Calls cannot be postponed.** The reply channel is consumed when the event is delivered. Return a `Reply` action in the same `handle_event` call.
- **`GenStatemRef` is `Clone + Send + Sync`.** Share it across tasks. `call` and `cast` work the same as on `GenServerRef`.

---

## 4. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `GenStatem`, `GenStatemRef`, `EventType`, `Action`, `TransitionResult`
- [Building Stateful Services with GenServer](gen-server.md) -- when you need stateful services without formal state machine semantics
- [Delayed and Periodic Messages with Timer](timers.md) -- standalone timer functions, compared to GenStatem's built-in timeouts
