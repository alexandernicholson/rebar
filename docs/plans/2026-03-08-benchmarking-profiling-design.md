# Benchmarking & Profiling Design

**Date:** 2026-03-08
**Status:** Approved
**Context:** Establish a permanent performance benchmarking suite and capture a baseline before implementing GenServer, DynamicSupervisor, and RuntimeBuilder features. After baselining, identify 5 key optimisation areas to fold into the feature implementation plan.

## Approach: Criterion + Flamegraph Profiling

### 1. Criterion Benchmark Suite

Four benchmark groups in `crates/rebar-core/benches/`:

**`message_passing.rs`**
- Single-pair ping-pong throughput
- Fan-out: 1 sender → N receivers (N = 10, 50, 100)
- Fan-in: N senders → 1 receiver (N = 10, 50, 100)
- Varying message sizes: small rmpv::Value (string), large binary payload (1KB, 64KB)

**`process_lifecycle.rs`**
- Single process spawn latency
- Batch spawn: 100, 1000 processes
- Teardown/cleanup time after process exits
- Process table insert/remove/lookup throughput

**`supervisor.rs`**
- Supervisor startup with N children (1, 5, 10, 50)
- Restart latency: OneForOne, OneForAll, RestForOne strategies
- Dynamic add_child latency on a running supervisor

**`process_table.rs`**
- Concurrent read/write throughput on ProcessTable (DashMap)
- allocate_pid throughput (AtomicU64 contention)
- send-to-process lookup latency under load

Each benchmark uses `criterion::BenchmarkGroup` with multiple parameter sizes.

### 2. Flamegraph Profiling Support

**Build profiles** in workspace `Cargo.toml`:
```toml
[profile.bench]
debug = true

[profile.release]
debug = 1
```

**Flamegraph script** at `scripts/flamegraph.sh`:
- Runs a specific criterion benchmark
- Generates SVG via `cargo flamegraph` or `perf record` + `inferno`
- Outputs to `bench/flamegraphs/`

### 3. Tracing Instrumentation

Add `#[instrument(level = "trace")]` to hot paths:

- `Runtime::spawn` (with `pid` field)
- `Runtime::send` (with `dest` field)
- `ProcessTable::send` / `insert` / `remove`
- `supervisor_loop`
- `start_child` / `stop_child`
- `LocalRouter::route`

All at `trace` level — zero overhead unless a subscriber is active at that level.

### 4. Baseline Capture Workflow

1. Run full criterion suite → statistical baseline
2. Generate flamegraphs for each benchmark group → CPU profiling SVGs
3. Analyse flamegraphs → identify 5 biggest optimisation opportunities
4. Document baseline to `bench/results/baseline-2026-03-08/`
5. Fold 5 improvements into the GenServer/DynamicSupervisor implementation plan

## Non-goals

- CI regression gating (add later)
- Chrome trace viewer integration (overkill for now)
- Modifying the existing Docker Compose + oha benchmarks
