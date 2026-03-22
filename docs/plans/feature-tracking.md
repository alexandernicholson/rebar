# Rebar Feature Tracking

## New Elixir/OTP Paradigm Implementations

All features must be implemented with strict TDD and must NOT break any existing interfaces.
Projects like [barkeeper](https://github.com/alexandernicholson/barkeeper) rely on rebar in production.

| # | Feature | Module Location | Status | Dependencies |
|---|---------|----------------|--------|-------------|
| 1 | Task / Task.Supervisor | `rebar-core/src/task/` | DONE | None (uses Runtime, ProcessId) |
| 2 | Agent | `rebar-core/src/agent/` | DONE | None (thin GenServer wrapper) |
| 3 | Timer (send_after, send_interval) | `rebar-core/src/timer/` | DONE | None (uses Runtime, ProcessId) |
| 4 | Process Groups (pg) | `rebar-core/src/pg/` | DONE | None (uses ProcessId, ProcessTable) |
| 5 | Sys Debug Interface | `rebar-core/src/sys/` | DONE | GenServer (extends existing) |
| 6 | GenStatem (State Machine) | `rebar-core/src/gen_statem/` | DONE | None (parallel to GenServer) |
| 7 | GenStage / Flow | `rebar-core/src/gen_stage/` | DONE | GenServer patterns |
| 8 | Application | `rebar-core/src/application/` | DONE | Supervisor (uses existing) |
| 9 | PartitionSupervisor | `rebar-core/src/partition_supervisor/` | DONE | Supervisor, GenServer |

## Implementation Order (recommended)

1. **Timer** (no deps, unlocks GenServer timer support + handle_continue)
2. **Task** (no deps, foundational async primitive)
3. **Agent** (thin GenServer wrapper, quick win)
4. **Process Groups** (no deps, unlocks pub/sub patterns)
5. **Sys Debug** (extends GenServer, needs GenServer stable)
6. **GenStatem** (parallel to GenServer, self-contained)
7. **Application** (uses Supervisor, config system)
8. **GenStage** (complex, uses GenServer patterns)
9. **PartitionSupervisor** (uses Supervisor + GenServer)

## Invariants

- All existing tests must continue to pass (`cargo test --workspace`)
- No changes to existing public type signatures
- New modules are purely additive (new files, new `pub mod` entries in lib.rs)
- Re-exports through `rebar` facade crate must be additive only
- Barkeeper's usage of DistributedRuntime, Supervisor, GenServer, Registry must remain stable
