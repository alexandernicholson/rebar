# Documentation Update Design: QUIC, Distribution, and Drain

**Date:** 2026-03-03
**Scope:** Update 6 existing doc files + create 3 new internals docs to cover the three features implemented in the roadmap (QUIC transport, distribution layer, graceful node drain).

## Approach

Surgical updates to existing files + 3 new internals deep-dives. Follows the established pattern where architecture.md has the overview, api/*.md has the type/method reference, and internals/*.md has the deep dive.

## Files to Modify

### 1. `docs/architecture.md`

- Remove "(Planned)" from remote messaging section (~line 123)
- Replace conceptual remote messaging diagram with actual implementation details: `MessageRouter` trait, `DistributedRouter`, `RouterCommand` channel flow
- Add "QUIC Transport" subsection under Transport and Connection Management: stream-per-frame model, cert generation, SWIM fingerprint exchange
- Add "Graceful Node Drain" subsection: three-phase protocol flow diagram (Announce → Drain → Shutdown)
- Update SWIM Node State Machine section: mention `cert_hash` field on `Member`

### 2. `docs/api/rebar-core.md`

- Add `router` module section:
  - `MessageRouter` trait: `route(from, to, payload) -> Result<(), SendError>`
  - `LocalRouter` struct: wraps `ProcessTable`, default router
- Update `runtime` module section:
  - `Runtime::with_router(node_id, table, router)` constructor
  - `Runtime::table()` accessor
  - Note that `ProcessContext::send()` delegates to the router

### 3. `docs/api/rebar-cluster.md`

- Add `router` module section:
  - `DistributedRouter`: local delivery via ProcessTable, remote via RouterCommand channel
  - `RouterCommand::Send { node_id, frame }` enum
  - `encode_send_frame(from, to, payload) -> Frame`
  - `deliver_inbound_frame(table, frame) -> Result<(), SendError>`
- Add `drain` module section:
  - `DrainConfig`: announce_timeout (5s), drain_timeout (30s), shutdown_timeout (10s)
  - `DrainResult`: processes_stopped, messages_drained, phase_durations, timed_out
  - `NodeDrain`: new(), announce(), drain_outbound(), drain()
- Add `transport::quic` subsection:
  - `QuicTransport`, `QuicListener`, `QuicConnection`
  - `QuicTransportConnector`
  - `generate_self_signed_cert()`, `cert_fingerprint()`, `CertHash`
  - `FingerprintVerifier`
- Update `swim` section:
  - `cert_hash: Option<[u8; 32]>` on `Member`
  - `cert_hash` field on `GossipUpdate::Alive`
- Update `connection` section:
  - `ConnectionManager::drain_connections() -> usize`

### 4. `docs/getting-started.md`

- Add section "Distributed Messaging Across Nodes" after existing process examples
- Two-node example: create DistributedRouter + Runtime::with_router on each node, connect via TCP, send message cross-node, verify delivery
- Based on the actual integration test `end_to_end_cross_node_send_via_tcp`

### 5. `docs/extending.md`

- Update QUIC transport skeleton to reference actual `generate_self_signed_cert()` and `FingerprintVerifier`
- Add "Custom Drain Strategies" subsection showing how to build on NodeDrain

### 6. `docs/internals/swim-protocol.md`

- Add `cert_hash: Option<[u8; 32]>` to Member struct fields in the component diagram
- Add `cert_hash` field to GossipUpdate::Alive variant documentation
- Add "Certificate Fingerprint Exchange" subsection explaining SWIM-QUIC integration

## Files to Create

### 7. `docs/internals/quic-transport.md`

Structure (mirrors swim-protocol.md pattern):
- Overview: why QUIC, stream-per-frame model
- Certificate Generation: rcgen, self-signed, SHA-256 fingerprinting
- Certificate Verification: FingerprintVerifier, custom rustls ServerCertVerifier
- Transport Implementation: QuicTransport, QuicListener, QuicConnection
- Stream-per-Frame Model: one bidirectional stream per frame, length-prefixed framing
- SWIM Certificate Exchange: gossip fingerprints via cert_hash field
- QuicTransportConnector: integration with ConnectionManager
- Configuration: endpoint setup, crypto provider

### 8. `docs/internals/distribution-layer.md`

Structure:
- Overview: transparent remote send() across nodes
- MessageRouter Trait: design rationale (lives in rebar-core for decoupling)
- LocalRouter: default, wraps ProcessTable
- DistributedRouter: local vs remote routing decision, RouterCommand channel
- Frame Encoding: encode_send_frame / deliver_inbound_frame
- DistributedRuntime: wiring diagram (rebar facade crate)
- End-to-End Message Flow: ctx.send() → router → channel → ConnectionManager → TCP → deliver_inbound_frame → mailbox
- Diagrams: sequence diagram of cross-node send, component diagram

### 9. `docs/internals/node-drain.md`

Structure:
- Overview: why graceful drain, what problem it solves
- Three-Phase Protocol: Announce → Drain → Shutdown
- Phase 1 (Announce): Leave gossip broadcast, registry cleanup, names removed count
- Phase 2 (Drain Outbound): RouterCommand channel drain, timeout handling
- Phase 3 (Shutdown): drain_connections(), stats collection
- DrainConfig Tuning: timeout defaults and when to adjust
- DrainResult: observability fields, what each metric means
- Full Drain Orchestration: NodeDrain::drain() flow
- Sequence Diagram: full drain flow

## Conventions

- Mermaid diagrams: `<br>` only in node labels, never in state diagram transitions or sequence diagram messages
- No `&gt;` or `&lt;` in sequence diagram message text — use `[...]` for generics
- Code examples use real API signatures from the implementation
- Each new internals file has the same structure: Overview, Components, Diagrams, Implementation Details
