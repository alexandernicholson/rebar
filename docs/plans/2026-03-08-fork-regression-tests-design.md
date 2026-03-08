# Fork Regression Tests Design

## Context

A fork (johnnyshields/rebar, branch rebar-v5) rewrote large portions of rebar, breaking 10 distinct API contract areas. We need regression tests to guard these contracts so any future change that would break Barkeeper is caught at compile time or test time.

## Approach

Dedicated `tests/api_contract.rs` integration test files per crate. Each file explicitly asserts the contracts the fork broke: compile-time trait bounds, async signatures, thread safety, panic isolation, wire protocol stability, and QUIC transport.

## Test Plan

### `crates/rebar-core/tests/api_contract.rs` (~18 tests)

**ProcessId contract (3 tests):**
- `process_id_is_two_arg` — ProcessId::new(node, local) compiles
- `process_id_display_is_node_dot_local` — format is `<1.42>`
- `process_id_is_copy_send_sync` — compile-time assert

**Runtime async contract (4 tests):**
- `spawn_is_async` — runtime.spawn() returns a future
- `spawn_requires_send` — handler and future must be Send
- `send_is_async` — runtime.send() returns a future
- `context_send_is_async` — ctx.send() returns a future

**Thread safety contract (3 tests):**
- `runtime_is_send_sync` — Runtime: Send + Sync
- `process_table_is_send_sync` — ProcessTable: Send + Sync
- `concurrent_spawn_from_multiple_threads` — spawn from multiple tokio tasks

**MessageRouter contract (2 tests):**
- `message_router_requires_send_sync` — trait bound check
- `local_router_is_send_sync` — LocalRouter: Send + Sync

**Panic isolation (2 tests):**
- `panicking_process_does_not_crash_runtime` — strengthened existing test
- `panicking_process_cleans_up_table_entry` — zombie check

**SendError contract (1 test):**
- `send_error_variants_are_process_dead_and_mailbox_full` — exhaustive match

**Supervisor contract (3 tests):**
- `supervisor_handle_is_send_sync` — compile-time check
- `child_factory_requires_send` — factory closure must be Send
- `start_supervisor_is_async` — returns a future

### `crates/rebar-cluster/tests/api_contract.rs` (~15 tests)

**Transport trait contract (3 tests):**
- `transport_connection_requires_send_sync` — compile-time check
- `transport_listener_requires_send_sync` — compile-time check
- `transport_connector_requires_send_sync` — compile-time check

**QUIC transport (3 tests):**
- `quic_transport_types_exist` — QuicTransport, QuicConnection, QuicListener compile
- `quic_cert_generation_works` — generate_self_signed_cert() works
- `quic_roundtrip` — send/recv over QUIC

**DistributedRuntime contract (2 tests):**
- `distributed_runtime_is_not_generic` — no type params
- `distributed_runtime_accepts_dyn_connector` — Box<dyn TransportConnector>

**ConnectionManager contract (2 tests):**
- `connection_manager_is_send_sync` — compile-time check
- `connection_manager_concurrent_access` — multi-task usage

**Wire protocol stability (2 tests):**
- `frame_encode_decode_deterministic` — known bytes match
- `msg_type_variants_complete` — all 13 variants with correct u8 values

**DistributedRouter contract (3 tests):**
- `distributed_router_is_send_sync` — compile-time check
- `encode_send_frame_exists` — public function exists
- `deliver_inbound_frame_exists` — public function exists

### `crates/rebar-ffi/tests/api_contract.rs` (~5 tests)

**FFI stability:**
- `rebar_pid_is_two_field` — node_id + local_id only
- `ffi_functions_resolve` — all extern "C" functions callable
- `runtime_survives_callback_panic` — poisoned mutex recovery
- `msg_lifecycle` — create, read, free cycle works
- `pid_components_match` — node_id and local_id round-trip

## Total: ~38 regression tests
