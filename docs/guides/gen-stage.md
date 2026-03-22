# Back-Pressure Pipelines with GenStage

`GenStage` is a producer/consumer abstraction with built-in back-pressure. Producers emit events only when downstream consumers ask for them. Consumers pull events at their own pace. When you need a data pipeline where fast producers do not overwhelm slow consumers, GenStage enforces flow control at the framework level.

---

## Table of Contents

1. [Why GenStage](#1-why-genstage)
2. [How: A Log Ingestion Pipeline](#2-how-a-log-ingestion-pipeline)
3. [GenStage vs Task.async_map](#3-genstage-vs-taskasync_map)
4. [Key Points](#4-key-points)
5. [See Also](#5-see-also)

---

## 1. Why GenStage

Consider a log ingestion system. Raw log lines arrive at high speed. A parser normalizes them into structured records. A writer batches records into a database. The problem:

- The log source produces lines faster than the database can write them.
- Unbounded buffering between stages leads to memory exhaustion.
- If you throttle the producer with a fixed delay, you waste throughput when the consumer is idle.

GenStage solves this with **demand-driven flow control**. The consumer tells the producer how many events it can handle. The producer only generates that many. When the consumer finishes a batch, it re-requests more. No events are produced that nobody is ready to consume.

---

## 2. How: A Log Ingestion Pipeline

### The producer: generate log lines on demand

A producer implements `handle_demand`. The engine calls this callback when downstream consumers have requested events. Return a `Vec` of events to emit.

```rust
use std::sync::Arc;
use std::time::Duration;

use rebar_core::gen_stage::{
    spawn_stage, spawn_stage_with_dispatcher, GenStage, GenStageRef,
    StageType, SubscribeOpts, SubscriptionTag,
};
use rebar_core::gen_stage::dispatcher::BroadcastDispatcher;
use rebar_core::process::ExitReason;
use rebar_core::runtime::Runtime;

struct LogProducer {
    lines: Vec<String>,
}

#[async_trait::async_trait]
impl GenStage for LogProducer {
    type State = usize; // index into the line buffer

    async fn init(&self) -> Result<(StageType, Self::State), String> {
        Ok((StageType::Producer, 0))
    }

    async fn handle_demand(
        &self,
        demand: usize,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        // Emit up to `demand` lines from the buffer.
        let start = *state;
        let end = (start + demand).min(self.lines.len());
        *state = end;

        self.lines[start..end]
            .iter()
            .map(|line| rmpv::Value::String(line.clone().into()))
            .collect()
    }
}
```

### The transformer: parse log lines (producer-consumer)

A `ProducerConsumer` receives events from upstream and emits transformed events downstream.

```rust
struct LogParser;

#[async_trait::async_trait]
impl GenStage for LogParser {
    type State = u64; // count of parsed lines

    async fn init(&self) -> Result<(StageType, Self::State), String> {
        Ok((StageType::ProducerConsumer, 0))
    }

    async fn handle_events(
        &self,
        events: Vec<rmpv::Value>,
        _from: SubscriptionTag,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        // Parse each raw line into a structured record.
        let mut parsed = Vec::with_capacity(events.len());
        for event in events {
            if let Some(line) = event.as_str() {
                *state += 1;
                let record = rmpv::Value::Map(vec![
                    (
                        rmpv::Value::String("raw".into()),
                        rmpv::Value::String(line.into()),
                    ),
                    (
                        rmpv::Value::String("seq".into()),
                        rmpv::Value::Integer((*state).into()),
                    ),
                ]);
                parsed.push(record);
            }
        }
        parsed
    }
}
```

### The consumer: write records to the database

A consumer implements `handle_events` but returns an empty `Vec` (nothing to emit downstream).

```rust
struct DbWriter;

#[async_trait::async_trait]
impl GenStage for DbWriter {
    type State = Vec<rmpv::Value>; // accumulated batch

    async fn init(&self) -> Result<(StageType, Self::State), String> {
        Ok((StageType::Consumer, Vec::new()))
    }

    async fn handle_events(
        &self,
        events: Vec<rmpv::Value>,
        _from: SubscriptionTag,
        state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        // Simulate writing to a database.
        for event in &events {
            state.push(event.clone());
        }
        println!("wrote {} records (total: {})", events.len(), state.len());

        // Consumer returns empty vec -- nothing to emit.
        vec![]
    }
}
```

### Wiring the pipeline

Connect stages by subscribing consumers to producers. The subscription carries demand configuration.

```rust
async fn build_pipeline() {
    let rt = Arc::new(Runtime::new(1));

    // Spawn stages.
    let producer = spawn_stage(
        Arc::clone(&rt),
        LogProducer {
            lines: (0..1000)
                .map(|i| format!("2024-01-15 10:00:{i:02} INFO request completed"))
                .collect(),
        },
    )
    .await;

    let parser = spawn_stage(Arc::clone(&rt), LogParser).await;
    let writer = spawn_stage(Arc::clone(&rt), DbWriter).await;

    // Subscribe parser to producer.
    let _tag1 = parser
        .subscribe(
            &producer,
            SubscribeOpts {
                max_demand: 100, // request 100 events at a time
                min_demand: 50,  // re-request when 50 have been processed
            },
        )
        .await
        .unwrap();

    // Subscribe writer to parser.
    let _tag2 = writer
        .subscribe(
            &parser,
            SubscribeOpts {
                max_demand: 50,
                min_demand: 25,
            },
        )
        .await
        .unwrap();

    // The pipeline is now running. Events flow:
    //   producer -> parser -> writer
    // governed by demand from downstream.

    tokio::time::sleep(Duration::from_secs(2)).await;
}
```

### Fan-out with BroadcastDispatcher

By default, a producer uses `DemandDispatcher`, which distributes events sequentially to consumers based on their pending demand. When you need every consumer to receive every event, use `BroadcastDispatcher`.

```rust
async fn fan_out_to_multiple_consumers() {
    let rt = Arc::new(Runtime::new(1));

    // Spawn a producer with BroadcastDispatcher.
    let producer = spawn_stage_with_dispatcher(
        Arc::clone(&rt),
        LogProducer {
            lines: (0..100)
                .map(|i| format!("line {i}"))
                .collect(),
        },
        BroadcastDispatcher::new(),
    )
    .await;

    // Both consumers see every event.
    let writer_a = spawn_stage(Arc::clone(&rt), DbWriter).await;
    let writer_b = spawn_stage(Arc::clone(&rt), DbWriter).await;

    writer_a
        .subscribe(&producer, SubscribeOpts::default())
        .await
        .unwrap();
    writer_b
        .subscribe(&producer, SubscribeOpts::default())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_secs(1)).await;
}
```

### Calls and casts on stages

Stages also support synchronous calls and asynchronous casts, useful for querying pipeline state or injecting events.

```rust
async fn query_stage(stage: &GenStageRef) {
    // Synchronous call with timeout.
    let reply = stage
        .call(
            rmpv::Value::String("get_stats".into()),
            Duration::from_secs(1),
        )
        .await;
    println!("stats: {reply:?}");

    // Asynchronous cast.
    let _ = stage.cast(rmpv::Value::String("flush".into()));
}
```

### Manual demand control

By default, consumers automatically re-request demand after processing events. For advanced cases, override `handle_subscribe` to return `DemandMode::Manual` and call `ask` explicitly:

```rust
use rebar_core::gen_stage::{DemandMode, StageType};

struct ManualConsumer;

#[async_trait::async_trait]
impl GenStage for ManualConsumer {
    type State = ();

    async fn init(&self) -> Result<(StageType, Self::State), String> {
        Ok((StageType::Consumer, ()))
    }

    async fn handle_subscribe(
        &self,
        _role: StageType,
        _opts: &SubscribeOpts,
        _from: SubscriptionTag,
        _state: &mut Self::State,
    ) -> DemandMode {
        DemandMode::Manual
    }

    async fn handle_events(
        &self,
        events: Vec<rmpv::Value>,
        _from: SubscriptionTag,
        _state: &mut Self::State,
    ) -> Vec<rmpv::Value> {
        println!("manual consumer got {} events", events.len());
        vec![]
    }
}

async fn manual_demand(consumer: &GenStageRef, tag: SubscriptionTag) {
    // Manually request 10 events.
    consumer.ask(tag, 10).await.unwrap();
}
```

---

## 3. GenStage vs Task.async_map

Both process collections concurrently. The difference is flow control:

| Feature | `Task::async_map` | `GenStage` |
|---|---|---|
| Input | Fixed collection (Vec, Range) | Unbounded / streaming |
| Back-pressure | None (all items spawned up front) | Built-in demand protocol |
| Topology | Single fan-out, single fan-in | Arbitrary pipelines (chains, fan-out, fan-in) |
| Lifetime | One-shot (returns when all items done) | Long-lived (runs until stopped) |
| Complexity | One function call | Trait implementation + subscription wiring |

Use `async_map` for bounded, known-size workloads. Use GenStage for continuous data streams or pipelines where producers outpace consumers.

---

## 4. Key Points

- **Three stage types.** `Producer` emits events via `handle_demand`. `Consumer` receives events via `handle_events`. `ProducerConsumer` does both.
- **Demand-driven.** Producers only generate events when consumers ask. No unbounded buffering. No overflow.
- **`SubscribeOpts` controls batch size.** `max_demand` is the upper limit per request. `min_demand` triggers a new request when pending events drop to this level.
- **Automatic vs manual demand.** By default, consumers re-request demand automatically. Return `DemandMode::Manual` from `handle_subscribe` and call `ask()` for explicit control.
- **`DemandDispatcher` distributes sequentially.** Events go to the first consumer with demand, then the next. Use it for load-balancing across workers.
- **`BroadcastDispatcher` sends to everyone.** Every consumer with demand receives a clone of every event. Use it for fan-out (logging to multiple sinks, metrics + storage).
- **Subscription tags are unique.** Each subscription gets a monotonically increasing `SubscriptionTag`. Use it to identify which producer sent a batch in `handle_events`.
- **`cancel` unsubscribes.** Both sides receive `handle_cancel`. Clean up resources there.
- **`GenStageRef` is `Clone`.** Pass it around freely. `call`, `cast`, `subscribe`, `ask`, and `cancel` are all available.

---

## 5. See Also

- [API Reference: rebar-core](../api/rebar-core.md) -- full documentation for `GenStage`, `GenStageRef`, `StageType`, `DemandMode`, `SubscribeOpts`, `Dispatcher`, `BroadcastDispatcher`
- [Parallel Work with Task](tasks.md) -- `async_map` for bounded, one-shot parallel work
- [Building Stateful Services with GenServer](gen-server.md) -- when you need a stateful service, not a pipeline stage
