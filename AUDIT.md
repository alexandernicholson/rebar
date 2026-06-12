> **RESOLUTION (2026-06-12).** All findings below — including C14 — have been
> fixed and validated. Workspace: `cargo clippy --workspace --all-targets` clean
> (deny all/pedantic/nursery, zero `#[allow]` added), `cargo test --workspace`
> 661 passing / 0 failing, with ~80 new regression tests. The fix landed a single
> canonical process-death path (`cleanup_process`) that all behavior engines and
> supervisors now route through; routable PIDs for gen_statem/gen_stage/supervised
> children; panic-driven restart; real TLS verification; a frame-size cap; and
> non-panicking peer-frame decoding.
>
> **C14 (resolved).** SWIM is now wired end-to-end into `DistributedRuntime`:
> a `Swim` wire message type; a `SwimService` (`crates/rebar-cluster/src/swim/service.rs`)
> implementing direct Ping/Ack, indirect `PingReq` relay, suspect→dead timing,
> and gossip dissemination with piggybacking + self-refutation; `MembershipList::apply_update`;
> and `DistributedRuntime::{enable_swim, swim_add_seed, swim_tick, handle_inbound_frame}`
> driving it over the `ConnectionManager`/transport. Proven by 4 protocol unit
> tests plus an over-the-wire TCP integration test (`crates/rebar/tests/swim_integration.rs`)
> in which two real runtimes discover each other Alive and the survivor detects a
> killed peer as Dead through the full stack.

# Rebar correctness & data-integrity audit

Scope: all ~21K lines of `crates/{rebar-core,rebar-cluster,rebar-ffi,rebar}`.
Method: 9 parallel subsystem auditors (several run in duplicate for independent
corroboration) hunting the bug class we already hit — silent lifecycle/identity,
message loss, data corruption, distributed-state staleness — followed by direct
re-verification of every critical against the source.

Confidence tags:
- **[V]** I read the cited code and confirmed the mechanism myself.
- **[Cn]** n independent auditors reported it (corroboration).
- **[R]** single-source, plausible, not yet line-verified by me.

The central result: **the bug you found is systemic, not a one-off.** The
"allocate a PID but never make it routable / never run death cleanup" pattern
recurs across `gen_statem`, `gen_stage`, the partition supervisor, and the
distributed layer. The monitor/registry/pg machinery we just wired into
`Runtime::spawn` is bypassed by every behavior engine. Separately, the network
boundary has no input validation and no real TLS authentication.

---

## CRITICAL

### C1. `gen_statem` and `gen_stage` processes are never inserted into the process table — unroutable while alive  [V]
`gen_statem/engine.rs:122` and `gen_stage/engine.rs:231` call `allocate_pid()`
but never `table.insert(pid, handle)` (verified: zero `ProcessHandle`
construction in either file; gen_stage's only `insert`s are subscription maps).
Both still call `table.remove(&pid)` on exit. While the actor is alive and
serving via its typed ref, `table.send`/`monitor`/`register`/pg all see it as
`ProcessDead`. `EventType::Info` in gen_statem is unreachable dead code.
**Same class as the original supervised-child bug.** Tests pass because they
only use the typed ref, never the PID.

### C2. Every behavior engine bypasses `cleanup_process` on death  [V] [C3]
`cleanup_process` (the only path that fires monitor `DOWN`s, unregisters names,
runs pg/exit hooks) has exactly one caller: `Runtime::spawn`'s drop guard
(`runtime.rs:235`). `gen_server/engine.rs:415`, `gen_stage/engine.rs:244`,
`gen_statem/engine.rs:397`, `coordinator/engine.rs:427,444` all exit via bare
`table.remove`. Consequence: monitor a gen_server, it dies — the watcher waits
for a `DOWN` that never arrives (hang); its registered name stays pointing at a
dead PID forever; monitor/name bookkeeping leaks. This is the same gap we just
fixed for raw processes, un-propagated to the behaviors built on them.
Fix direction: make `remove` crate-private and route all engine exits through
`cleanup_process`.

### C3. A panicking supervised child is never restarted  [V] [C2]
`supervisor/engine.rs:382-399`: the child runs as `select!{ child_future =>
reason, shutdown_rx => Normal }` and only then `msg_tx.send(ChildExited)`. There
is **no `catch_unwind` anywhere in rebar-core** (verified). A panic in the child
future unwinds the whole spawn body, skipping the `ChildExited` send. The
supervisor keeps `pid = Some(..)`/`active = true` forever and never restarts —
so a `Permanent` child that panics (the single most common failure) dies
silently and permanently. Same pattern in `dynamic.rs:366-380`. This defeats
the core purpose of the supervisor.

### C4. `gen_statem` cancels a state-timeout armed during the same transition  [C2]
`gen_statem/engine.rs:469-489`: in the `NextState` arm, `process_actions` first
installs an `Action::StateTimeout`, then `if state_changed { *state_timeout =
None }` unconditionally wipes it. The canonical OTP idiom — transition to
`Connecting` and arm a handshake timeout in the same step — silently never
fires. Machine hangs in the new state forever.

### C5. `gen_statem` discards Call/Cast payloads  [C2]
`gen_statem/engine.rs:207-208, 252-254`: `drop(envelope.msg)` and `payload =
Nil`. `handle_event` receives `EventType::Call(reply_tx)`/`Cast` with a **Nil
payload** — the actual message content is gone. Any state machine whose
transitions depend on message data silently misbehaves; the typed API compiles
and "works," making this an invisible data-loss trap.

### C6. `BroadcastDispatcher` silently drops events for lower-demand subscribers  [C2]
`gen_stage/dispatcher.rs:136-173`: for a batch, each subscriber gets
`min(len, its_demand)` events but `leftover` is always returned empty. With
subscribers at demand 5 and 2 and a 5-event batch, the second permanently loses
events 3–5 (not buffered, not redelivered). Broadcast is supposed to give every
consumer every event. Silent, unrecoverable fan-out data loss.

### C7. Partition supervisor routes by cached first-incarnation PIDs  [V] [C2]
`partition_supervisor/engine.rs`: `partition_pids` is a fixed `Vec` captured once
at startup (`:143`); `which_partition*` returns the cached PID (`:58-80`).
Children are created with `ChildSpec::new("partition_i")` — **not**
`.registered()` (`:135`), so there is no name fallback. After any partition
crash+restart (new PID), every key hashing to that partition routes to a dead
PID **permanently** — 1/N of the keyspace black-holed. This is the original bug
reintroduced one layer up.

### C8. QUIC TLS signature verification is fully stubbed — cert pinning is bypassable  [V]
`transport/quic.rs:309-325`: `verify_tls12_signature` and
`verify_tls13_signature` both `return Ok(HandshakeSignatureValid::assertion())`
unconditionally. The only check is that the cert's SHA-256 matches a pinned
fingerprint — but the fingerprint is the hash of the *public* cert, transmitted
in the clear. An attacker presenting that public cert with a *different private
key* passes both checks. TLS's proof-of-key-possession is disabled →
MITM / node impersonation → user data shipped to an attacker. The existing
`quic_cert_fingerprint_mismatch` test gives false confidence (it only checks a
*wrong* hash is rejected).

### C9. Unbounded allocation from a peer-controlled length field  [V]
`transport/tcp.rs:91` and `transport/quic.rs:264`: read a 4-byte big-endian
length, then `vec![0u8; len]` with **no maximum** (verified: no `MAX_FRAME`
constant anywhere). A peer sending `FF FF FF FF` makes the node allocate ~4 GiB
per frame/stream. TCP has no auth at all. Trivial remote OOM/DoS taking down the
node and all hosted state. `frame.rs:135-138` has the same uncapped trust and
can integer-overflow the size check on 32-bit targets.

### C10. `deliver_inbound_frame` panics on malformed peer input  [V] [C2]
`router.rs:102,113-116`: `frame.header.as_map().expect("frame header must be a
Map")` and `value.as_u64().expect("... must be u64")` on a fully
attacker-controlled msgpack header (decode only validated it's *some* value). A
peer sending a `Send` frame with a non-map header, or `to_local` as a string,
panics the inbound delivery task. Remote-triggerable crash; across the FFI
boundary (C15) it's UB.

### C11. SWIM refutation increments the wrong node's incarnation and never re-gossips  [C2]
`swim/detector.rs:47-55`: `record_ack` does `member.incarnation += 1` on the
*prober's* local copy of a *remote* member, and nothing ever turns that into a
gossiped `Alive`. SWIM requires the suspected node itself to bump its own
incarnation and broadcast. Because refutation never propagates and `alive()`
requires strictly-greater incarnation, a stale `Suspect` that reaches a third
node sticks and marches to `Dead`. Result: healthy nodes permanently evicted
under transient load; their registry entries get reaped, names point nowhere.

### C12. OR-Set registry split-brain silently clobbers a live PID  [R]
`registry/orset.rs:49-78`: `register` unconditionally pushes; resolution is pure
last-writer-wins by `(timestamp, node_id)`. Two nodes registering the same name
during a partition both keep a live entry; after merge, `lookup` returns one and
silently discards the other — but *both* processes are alive and each thinks it
owns the name. User data sent to two different processes for one logical name;
the loser is orphaned with no notification.

### C13. OR-Set full-sync `Remove` uses an empty name and never reaps the live entry  [R]
`registry/orset.rs:161-208`: `generate_deltas` emits tombstones as
`Remove{name:"", tag}`, and `merge_delta(Remove)` looks up `entries[""]` — which
is empty — so a peer that learned a name via full-sync keeps resolving it to the
(now unregistered, dead) PID forever. Registry never converges; silent wrong
delivery.

### C14. SWIM is not wired into the runtime at all  [V]
Verified: `FailureDetector` has zero callers outside its own file;
`MembershipList` is never instantiated in `DistributedRuntime`; nothing calls
`.tick()`; `MsgType` (`protocol/frame.rs:19-33`) has no Ping/PingReq/Ack/Gossip
variants; no code applies a received `GossipUpdate` to a `MembershipList`
(only the integration tests hand-roll it). **There is currently no failure
detection in production** — a crashed peer is never detected, marked, or
announced. (So C11/C12/C13 are latent until SWIM is actually run — but they are
real bugs in code that is presented as production-ready.)

### C15. No `catch_unwind` at any FFI entry point  [V]
`rebar-ffi/src/lib.rs`: none of the `extern "C"` fns guard against unwinding;
panic across the C ABI is UB. Reachable via a poisoned `registry` mutex
(`lock().unwrap()` after any prior panic-while-locked), a panicking user
callback in `rebar_spawn`, or C10's panic propagating through `deliver_inbound`.

---

## HIGH (summary — file:line in the per-subsystem notes)

Lifecycle / identity
- `register_inner` (table.rs:204-237) races with process death on the **external**
  `Runtime::register`/`reregister` path (no post-insert re-check like `monitor`
  has) → name left pointing at a dead PID, `names_by_pid` leak. [C2]
- `cleanup_process` (table.rs:350-357) fires `DOWN` unconditionally, ignoring the
  `monitor_index.remove` result → **duplicate DOWN**, and DOWN delivered *after*
  a successful `demonitor` (violates the demonitor contract). [R, traced]
- Agent (agent/types.rs): killing the PID doesn't stop the loop → zombie agent
  keeps accepting writes while the table says it's dead; and `AgentRef` is
  untyped so a wrong-type `expect()` downcast panics the agent and wipes all
  shared state. [C2]
- `Task::shutdown()` (task/engine.rs:118) is a no-op — `join_handle` is always
  `None`, so the abort path is dead code; cancellation silently lies and tasks
  leak. [C2]
- `send_interval` (timer/engine.rs) to a third party survives the owner's death
  → leaked timer task firing stale messages forever; an interval whose callback
  panics stops silently. [C2]

Message loss / backpressure
- gen_stage: dead/unsubscribed consumer never cleaned up; dispatcher keeps
  allocating it demand and dropping events into a closed channel. [C2]
- gen_stage: `CancelReason::Down` is never emitted → stage death not propagated,
  peers dangle forever. [C2]
- gen_stage: `event_buffer` (engine.rs:288) and the cast channel are unbounded →
  OOM under a slow/absent downstream. [C2]
- gen_stage: producer↔consumer blocking `send().await` on bounded (256) channels
  in both directions → cross-stage deadlock (a self-subscribed stage deadlocks
  trivially). [C2]
- gen_server / statem / stage / agent: `call()` only times out the *reply*, not
  the `call_tx.send().await` → a suspended/backed-up server hangs the caller
  with no timeout. [C2]
- gen_server: pending casts/info silently dropped on shutdown (shutdown keys off
  whichever channel closes first). [R]
- coordinator: routes to a dead-but-not-yet-cleaned worker (unbounded mailbox
  `send` still succeeds) → submit hangs the full timeout with no failover, and
  the worker is never removed from the pool. [R]
- cluster router: outbound `try_send` full → message dropped, mislabeled
  `NodeUnreachable` (router.rs:49-55); inbound frame to a respawned PID silently
  lost (no name/incarnation indirection cross-node). [R]
- `DistributedRuntime::process_outbound` (rebar/src/lib.rs:66) drops the frame on
  a `route` error and still returns `true`. [R]
- drain (drain.rs:126-162): announces Leave + unregisters names *before* draining
  the outbound queue → in-flight replies lost; timeout path discards undelivered
  frames and over-reports `messages_drained`. [R]

Distributed correctness / growth
- SWIM: single missed direct ping → `Suspect` immediately; `indirect_probe_count`
  is config-only, never used → false evictions under load. [C2]
- SWIM: restarted node starts at incarnation 0 and loses to circulating
  Suspect/Dead; no rejoin handshake → can't re-enter cluster. [R]
- SWIM gossip queue is unbounded and never coalesces by node → never converges
  under churn + slow memory leak. [R]
- OR-Set tombstone set grows without bound and is re-broadcast on every full
  sync (orset.rs:32). [R]

Robustness
- Exit-hook panic inside `CleanupGuard::drop` *during* a panic unwind →
  double-panic → `std::process::abort()` of the whole node. [C2]
- Supervisor `ShutdownStrategy::Timeout(d)` is never enforced — `d` is discarded;
  graceful shutdown is identical to BrutalKill and drops the child mid-`await`,
  losing any flush/cleanup. [C2]
- `stop_child` doesn't await task termination (single `yield_now`) → old and new
  incarnations run concurrently after a restart → double-ownership of exclusive
  resources/names. [C2]
- Supervisor `Shutdown` only signals children then breaks → children outlive the
  supervisor as orphan tasks. [R]
- `ApplicationManager::start` check-then-act race → two live instances of one
  app, one orphaned forever; `running`/`start_order` updated non-atomically. [C2]
- FFI `bytes_from`/`from_raw_parts` trusts the caller `len` with no
  `len <= isize::MAX`/OOB guard → memory unsafety from a bad length. [R]

---

## MEDIUM / LOW (representative)

- `MailboxTx::send` reports `ProcessDead(from)`/`MailboxFull(from)` using the
  **sender's** PID, not the destination (mailbox.rs:101-137) — callers that mark
  "which process is dead" act on the wrong one; inconsistent with
  `ProcessTable::send` which reports the destination. [R]
- Biased `select!` in gen_server/gen_statem starves casts/info/timeouts under
  sustained call load, and can reorder cast-then-call from one sender (the repo's
  own test inserts `yield_now()` to mask it) — violates per-sender ordering. [C2]
- gen_server `handle_continue` drains greedily → a self-reenqueuing continue
  starves sys/shutdown. [R]
- gen_stage: `handle_demand` gets cumulative outstanding demand, not the
  increment → convention-following producers double-produce (engine.rs:567); a
  `min_demand > max_demand` subscription stalls after one batch (no validation);
  re-ask isn't capped to `max_demand`. [C2]
- FFI null-checks live only in `debug_assert` inside the helpers (compiled out in
  release); today every public fn guards correctly, but there's no release-time
  safety net — one future omission = release-only null deref. FFI registry is
  never cleaned on process death (own `HashMap`, separate from core). [C2]
- `tokio::time::sleep`/`interval` panic on zero/huge `Duration` from
  `send_interval` callers (timer/engine.rs). [R]
- All mailboxes are unconditionally unbounded (runtime.rs:205) — no backpressure,
  no queue introspection; combined with the no-force-kill gap (the `JoinHandle`
  is dropped at spawn), one stuck process = silent node OOM with no way to kill
  it. [R]

---

## Disputed / not-a-bug (recorded for completeness)
- Static vs dynamic supervisor restart-intensity limiter: one auditor flagged an
  off-by-one, a second checked carefully and found both allow exactly
  `max_restarts` per window. **Treat as needs-confirmation, low.**
- PID allocation wraparound: monotonic `u64`, unreachable in practice — fine
  (but `MonitorRef` shares the pattern; document the assumption).
- Name takeover on restart (`reregister` + `cleanup_process`'s
  `remove_if(|p| *p==pid)`) is correct — a late old-incarnation cleanup cannot
  erase the new registration. Verified safe.

---

## Recommended remediation order
1. **Unify process death** (C1, C2, C3, and several HIGH): one code path that
   inserts a routable handle at spawn and runs `cleanup_process` on every exit
   incl. panic (`catch_unwind` → synthesize `ExitReason::Abnormal` →
   `ChildExited` + DOWN). Make `ProcessTable::remove` crate-private. This single
   change closes the largest cluster of findings, including the
   supervisor-doesn't-restart-on-panic bug.
2. **Network input hardening** (C9, C10, frame size cap): a `MAX_FRAME_SIZE`
   constant enforced before allocation, and replace every `.expect/.unwrap` in
   `deliver_inbound_frame` with error returns.
3. **TLS** (C8): delegate signature verification to a real verifier; keep
   fingerprint pinning on top.
4. **Identity over PIDs** (C7, cross-node loss): route partitions and cross-node
   sends through the (cleaned-on-death) name registry, not cached PIDs.
5. **Distributed layer** (C11–C14): either wire SWIM in properly with correct
   refutation/incarnation semantics and bounded gossip, or clearly mark it
   unfinished and not load-bearing for production.
6. **gen_statem / gen_stage correctness** (C4, C5, C6 + HIGH): payload passing,
   state-timeout ordering, broadcast leftover buffering, demand accounting,
   bounded buffers, death propagation.
