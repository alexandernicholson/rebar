//! SWIM failure-detection / gossip service.
//!
//! [`SwimService`] ties the membership list, failure detector, and gossip queue
//! together into a runnable protocol. It is transport-agnostic: every method
//! produces the wire frames that should be sent ([`Outgoing`]) and the caller
//! is responsible for delivering them. This keeps all protocol state behind
//! short, non-`await` critical sections (no lock is ever held across a network
//! call) and makes the protocol unit-testable by wiring two services' outputs
//! directly into each other's [`SwimService::handle_frame`].
//!
//! Protocol per period ([`SwimService::tick`]):
//! 1. Resolve the previous period's probe: if its target was not acked
//!    (directly or via a relayed indirect ack), fire indirect `PingReq`s to up
//!    to `indirect_probe_count` helpers (a last chance for a reachable-but-
//!    slow node to be confirmed alive before the next tick) and record one
//!    failed probe. The detector only suspects after `1 + indirect_probe_count`
//!    failures, so a single miss never evicts a node.
//! 2. Expire suspect timers into `Dead` (gossiping the death) and remove nodes
//!    whose dead-removal delay has elapsed.
//! 3. Pick a fresh random target and direct-`Ping` it, piggybacking gossip.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::config::SwimConfig;
use super::detector::FailureDetector;
use super::gossip::{GossipQueue, GossipUpdate};
use super::member::{Member, MembershipList, NodeState};
use crate::protocol::{Frame, MsgType};

const PROTOCOL_VERSION: u8 = 1;

/// The kind of a SWIM message, carried inside a [`SwimEnvelope`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum SwimKind {
    /// A direct liveness probe.
    Ping,
    /// An acknowledgement. `about` is `Some(target)` for a relayed indirect ack
    /// confirming `target`'s liveness, or `None` for a direct ack (the sender
    /// itself is alive).
    Ack { about: Option<u64> },
    /// Ask the receiver to probe `target` on the sender's behalf (indirect probe).
    PingReq { target: u64, target_addr: SocketAddr },
}

/// The payload of a `Swim` frame: a message plus piggybacked gossip.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SwimEnvelope {
    from: u64,
    from_addr: SocketAddr,
    seq: u64,
    kind: SwimKind,
    gossip: Vec<GossipUpdate>,
}

/// A frame to be sent to a peer, produced by the service.
#[derive(Debug, Clone)]
pub struct Outgoing {
    /// Destination node id.
    pub node_id: u64,
    /// Destination address (so the caller can connect if not already).
    pub addr: SocketAddr,
    /// The frame to send.
    pub frame: Frame,
}

/// The result of one protocol period.
#[derive(Debug, Default)]
pub struct TickOutcome {
    /// Frames to send.
    pub outgoing: Vec<Outgoing>,
    /// Nodes newly declared dead this tick.
    pub newly_dead: Vec<u64>,
    /// Nodes removed from membership this tick (dead-removal delay elapsed).
    pub removed: Vec<u64>,
}

/// Tracks the in-flight direct probe for the current period.
#[derive(Default)]
struct ProbeState {
    target: Option<u64>,
    acked: bool,
}

/// A relay's record of an indirect probe it is performing on an origin's behalf.
struct RelayEntry {
    origin: u64,
    origin_addr: SocketAddr,
    origin_seq: u64,
    target: u64,
}

/// Runnable SWIM service. Cheaply shareable; all state is internally locked.
pub struct SwimService {
    self_id: u64,
    self_addr: SocketAddr,
    config: SwimConfig,
    seq: AtomicU64,
    members: Mutex<MembershipList>,
    detector: Mutex<FailureDetector>,
    gossip: Mutex<GossipQueue>,
    probe: Mutex<ProbeState>,
    /// Outstanding indirect probes this node is relaying, keyed by the probe
    /// ping's seq so the target's ack can be forwarded to the origin.
    relays: Mutex<HashMap<u64, RelayEntry>>,
}

impl SwimService {
    /// Create a service for a node at `self_addr`.
    #[must_use]
    pub fn new(
        self_id: u64,
        self_addr: SocketAddr,
        config: SwimConfig,
        cert_hash: Option<[u8; 32]>,
    ) -> Self {
        Self {
            self_id,
            self_addr,
            config,
            seq: AtomicU64::new(1),
            members: Mutex::new(MembershipList::with_self(self_id, self_addr, cert_hash)),
            detector: Mutex::new(FailureDetector::new()),
            gossip: Mutex::new(GossipQueue::new()),
            probe: Mutex::new(ProbeState::default()),
            relays: Mutex::new(HashMap::new()),
        }
    }

    /// This node's id.
    #[must_use]
    pub const fn self_id(&self) -> u64 {
        self.self_id
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn lock_members(&self) -> std::sync::MutexGuard<'_, MembershipList> {
        self.members.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Add a known peer (seed) so this node can begin probing it.
    pub fn add_seed(&self, node_id: u64, addr: SocketAddr) {
        if node_id == self.self_id {
            return;
        }
        let mut members = self.lock_members();
        members.add(Member::new(node_id, addr));
    }

    /// Snapshot of currently-alive peer ids (excluding self), for diagnostics/tests.
    #[must_use]
    pub fn alive_peers(&self) -> Vec<u64> {
        let members = self.lock_members();
        members
            .all_members()
            .filter(|m| m.node_id != self.self_id && m.state == NodeState::Alive)
            .map(|m| m.node_id)
            .collect()
    }

    /// The known state of a peer (for diagnostics/tests).
    #[must_use]
    pub fn state_of(&self, node_id: u64) -> Option<NodeState> {
        self.lock_members().get(node_id).map(|m| m.state)
    }

    fn build_envelope(&self, seq: u64, kind: SwimKind, gossip: Vec<GossipUpdate>) -> Frame {
        let env = SwimEnvelope {
            from: self.self_id,
            from_addr: self.self_addr,
            seq,
            kind,
            gossip,
        };
        let bytes = rmp_serde::to_vec(&env).expect("swim envelope serializes");
        Frame {
            version: PROTOCOL_VERSION,
            msg_type: MsgType::Swim,
            request_id: seq,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Binary(bytes),
        }
    }

    /// Drain up to `max_gossip_per_tick` pending updates for piggybacking.
    fn drain_gossip(&self) -> Vec<GossipUpdate> {
        self.gossip
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(self.config.max_gossip_per_tick)
    }

    fn enqueue_gossip(&self, update: GossipUpdate) {
        self.gossip
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add(update);
    }

    /// Take the previous period's probe target and whether it was acked,
    /// clearing it. Locks `probe` only for this read.
    fn take_probe(&self) -> (Option<u64>, bool) {
        let mut probe = self
            .probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (probe.target.take(), probe.acked)
    }

    /// Record the new direct-probe target for this period.
    fn set_probe(&self, target: u64) {
        let mut probe = self
            .probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *probe = ProbeState {
            target: Some(target),
            acked: false,
        };
    }

    /// Run one protocol period.
    #[must_use]
    pub fn tick(&self, now: Instant) -> TickOutcome {
        let mut out = Vec::new();
        let newly_dead;
        let removed;
        // The new target to direct-ping, decided under the locks but pinged
        // after they drop (so the guards don't outlive their last use).
        let mut next_ping: Option<(u64, SocketAddr)> = None;

        // --- Resolve previous probe, expire timers, pick a new target. ---
        {
            let mut members = self.lock_members();
            let mut detector = self
                .detector
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            // 1. Previous probe unresolved → indirect probe + record one failure.
            if let (Some(target), false) = self.take_probe() {
                if let Some(target_addr) = members.addr_of(target) {
                    let helpers: Vec<(u64, SocketAddr)> = members
                        .alive_peers()
                        .into_iter()
                        .filter(|(id, _)| *id != target)
                        .take(self.config.indirect_probe_count)
                        .collect();
                    for (hid, haddr) in helpers {
                        let seq = self.next_seq();
                        out.push(Outgoing {
                            node_id: hid,
                            addr: haddr,
                            frame: self.build_envelope(
                                seq,
                                SwimKind::PingReq {
                                    target,
                                    target_addr,
                                },
                                Vec::new(),
                            ),
                        });
                    }
                }
                detector.record_nack(&mut members, target, &self.config, now);
                // If that crossed the suspicion threshold, disseminate it.
                if let Some(m) = members.get(target)
                    && m.state == NodeState::Suspect
                {
                    self.enqueue_gossip(GossipUpdate::Suspect {
                        node_id: target,
                        addr: m.addr,
                        incarnation: m.incarnation,
                    });
                }
            }

            // 2. Expire suspect timers → dead; remove long-dead nodes.
            newly_dead = detector.check_suspect_timeouts(&mut members, &self.config, now);
            for &dead_id in &newly_dead {
                if let Some(m) = members.get(dead_id) {
                    self.enqueue_gossip(GossipUpdate::Dead {
                        node_id: dead_id,
                        addr: m.addr,
                        incarnation: m.incarnation,
                    });
                }
            }
            removed = detector.remove_expired_dead(&mut members, &self.config, now);

            // 3. Pick a fresh target to direct-ping (the ping is built below,
            //    after the membership/detector/probe guards have dropped).
            if let Some(target) = detector.tick(&members, self.self_id)
                && let Some(addr) = members.addr_of(target)
            {
                self.set_probe(target);
                next_ping = Some((target, addr));
            }
        }

        if let Some((target, addr)) = next_ping {
            let gossip = self.drain_gossip();
            let seq = self.next_seq();
            out.push(Outgoing {
                node_id: target,
                addr,
                frame: self.build_envelope(seq, SwimKind::Ping, gossip),
            });
        }

        TickOutcome {
            outgoing: out,
            newly_dead,
            removed,
        }
    }

    /// Handle an inbound `Swim` frame, returning frames to send in response.
    ///
    /// Returns an empty vec for non-`Swim` or undecodable frames.
    #[must_use]
    pub fn handle_frame(&self, frame: &Frame) -> Vec<Outgoing> {
        if frame.msg_type != MsgType::Swim {
            return Vec::new();
        }
        let rmpv::Value::Binary(bytes) = &frame.payload else {
            return Vec::new();
        };
        let Ok(env) = rmp_serde::from_slice::<SwimEnvelope>(bytes) else {
            return Vec::new();
        };
        self.handle_envelope(env)
    }

    fn handle_envelope(&self, env: SwimEnvelope) -> Vec<Outgoing> {
        // Destructure to fully consume the owned envelope.
        let SwimEnvelope {
            from,
            from_addr,
            seq,
            kind,
            gossip,
        } = env;
        let mut out = Vec::new();

        // Apply piggybacked gossip first; collect any self-refutation to spread.
        {
            let mut members = self.lock_members();
            // Hearing directly from `from` is evidence it is alive.
            if from != self.self_id {
                if members.get(from).is_none() {
                    members.add(Member::new(from, from_addr));
                }
                // Tight scope: release the detector lock before the gossip loop.
                self.detector
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .record_ack(&mut members, from);
            }
            for update in &gossip {
                if let Some(refutation) = members.apply_update(update) {
                    self.enqueue_gossip(refutation);
                }
            }
        }

        match kind {
            SwimKind::Ping => {
                // Reply with a direct ack, piggybacking our own gossip.
                let gossip = self.drain_gossip();
                let reply_seq = self.next_seq();
                out.push(Outgoing {
                    node_id: from,
                    addr: from_addr,
                    frame: self.build_envelope(reply_seq, SwimKind::Ack { about: None }, gossip),
                });
            }
            SwimKind::Ack { about } => {
                let confirmed = about.unwrap_or(from);
                // A relayed ack we forwarded? Only direct acks (about=None) from
                // a target close a relay we initiated.
                let relayed = if about.is_none() {
                    self.relays
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&seq)
                } else {
                    None
                };
                if let Some(relay) = relayed {
                    // Forward an indirect ack to the origin: target is alive.
                    out.push(Outgoing {
                        node_id: relay.origin,
                        addr: relay.origin_addr,
                        frame: self.build_envelope(
                            relay.origin_seq,
                            SwimKind::Ack {
                                about: Some(relay.target),
                            },
                            Vec::new(),
                        ),
                    });
                    // Also clear our own suspicion of the target.
                    self.record_alive(relay.target);
                } else {
                    // Direct or indirect ack confirming `confirmed` is alive.
                    self.record_alive(confirmed);
                    let mut probe = self
                        .probe
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if probe.target == Some(confirmed) {
                        probe.acked = true;
                    }
                }
            }
            SwimKind::PingReq {
                target,
                target_addr,
            } => {
                // Relay: probe the target on the origin's behalf, remembering
                // how to forward its ack back.
                let relay_seq = self.next_seq();
                self.relays
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        relay_seq,
                        RelayEntry {
                            origin: from,
                            origin_addr: from_addr,
                            origin_seq: seq,
                            target,
                        },
                    );
                out.push(Outgoing {
                    node_id: target,
                    addr: target_addr,
                    frame: self.build_envelope(relay_seq, SwimKind::Ping, Vec::new()),
                });
            }
        }

        out
    }

    /// Clear local suspicion of a node we have positive liveness evidence for.
    fn record_alive(&self, node_id: u64) {
        if node_id == self.self_id {
            return;
        }
        let mut members = self.lock_members();
        let mut detector = self
            .detector
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        detector.record_ack(&mut members, node_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    fn fast_config(indirect: usize) -> SwimConfig {
        SwimConfig::builder()
            .protocol_period(Duration::from_millis(10))
            .suspect_timeout(Duration::from_millis(100))
            .indirect_probe_count(indirect)
            .max_gossip_per_tick(16)
            .build()
    }

    fn env_of(frame: &Frame) -> SwimEnvelope {
        let rmpv::Value::Binary(bytes) = &frame.payload else {
            panic!("not a swim frame");
        };
        rmp_serde::from_slice(bytes).unwrap()
    }

    /// Deliver each outgoing frame to its destination service and collect the
    /// responses, until the network is quiescent (bounded rounds).
    fn pump(nodes: &HashMap<u64, &SwimService>, mut pending: Vec<Outgoing>) {
        for _ in 0..50 {
            if pending.is_empty() {
                return;
            }
            let mut next = Vec::new();
            for o in std::mem::take(&mut pending) {
                if let Some(svc) = nodes.get(&o.node_id) {
                    next.extend(svc.handle_frame(&o.frame));
                }
            }
            pending = next;
        }
    }

    #[test]
    fn two_nodes_stay_alive_through_probing() {
        let a = SwimService::new(1, addr(9101), fast_config(0), None);
        let b = SwimService::new(2, addr(9102), fast_config(0), None);
        a.add_seed(2, addr(9102));
        b.add_seed(1, addr(9101));
        let refs: HashMap<u64, &SwimService> = [(1u64, &a), (2u64, &b)].into_iter().collect();

        let base = Instant::now();
        for i in 0..5 {
            let now = base + Duration::from_millis(i * 10);
            let mut out = a.tick(now).outgoing;
            out.extend(b.tick(now).outgoing);
            pump(&refs, out);
        }
        assert_eq!(a.state_of(2), Some(NodeState::Alive));
        assert_eq!(b.state_of(1), Some(NodeState::Alive));
    }

    #[test]
    fn silent_node_is_suspected_then_declared_dead() {
        // indirect_probe_count = 0 → a single failed probe suspects.
        let a = SwimService::new(1, addr(9111), fast_config(0), None);
        a.add_seed(2, addr(9112)); // node 2 exists but never responds

        let base = Instant::now();
        // tick 1: probe node 2 (no response delivered).
        let _ = a.tick(base);
        assert_eq!(a.state_of(2), Some(NodeState::Alive));

        // tick 2: prior probe unacked → record_nack → Suspect.
        let _ = a.tick(base + Duration::from_millis(10));
        assert_eq!(a.state_of(2), Some(NodeState::Suspect), "should be suspected");

        // tick 3: suspect_timeout elapsed → Dead, with a Dead gossip queued.
        let outcome = a.tick(base + Duration::from_millis(10) + Duration::from_millis(150));
        assert!(outcome.newly_dead.contains(&2), "node 2 newly dead");
        assert_eq!(a.state_of(2), Some(NodeState::Dead));
    }

    #[test]
    fn suspected_node_refutes_about_itself() {
        // B receives gossip suspecting itself and must refute with a higher
        // incarnation Alive, piggybacked on its reply.
        let b = SwimService::new(2, addr(9122), fast_config(0), None);

        // A Ping from node 3 carrying a Suspect rumour about B (node 2).
        let env = SwimEnvelope {
            from: 3,
            from_addr: addr(9123),
            seq: 7,
            kind: SwimKind::Ping,
            gossip: vec![GossipUpdate::Suspect {
                node_id: 2,
                addr: addr(9122),
                incarnation: 0,
            }],
        };
        let bytes = rmp_serde::to_vec(&env).unwrap();
        let frame = Frame {
            version: PROTOCOL_VERSION,
            msg_type: MsgType::Swim,
            request_id: 7,
            header: rmpv::Value::Nil,
            payload: rmpv::Value::Binary(bytes),
        };

        let out = b.handle_frame(&frame);
        // B replies to node 3 with an Ack carrying a refuting Alive about itself.
        let ack = out.iter().find(|o| o.node_id == 3).expect("ack to node 3");
        let ack_env = env_of(&ack.frame);
        let refutation = ack_env
            .gossip
            .iter()
            .find(|u| matches!(u, GossipUpdate::Alive { node_id: 2, .. }))
            .expect("refuting Alive about self");
        assert!(
            refutation.incarnation() >= 1,
            "refutation bumps own incarnation past the rumour"
        );
    }

    #[test]
    fn indirect_probe_relays_ack_to_origin() {
        // A(1) asks C(3) to indirectly probe B(2); C pings B, B acks C, C must
        // forward an indirect ack (about=Some(2)) back to A.
        let a = SwimService::new(1, addr(9131), fast_config(3), None);
        let b = SwimService::new(2, addr(9132), fast_config(3), None);
        let c = SwimService::new(3, addr(9133), fast_config(3), None);
        a.add_seed(2, addr(9132));

        // A -> C: PingReq{target: 2}.
        let pingreq = SwimEnvelope {
            from: 1,
            from_addr: addr(9131),
            seq: 42,
            kind: SwimKind::PingReq {
                target: 2,
                target_addr: addr(9132),
            },
            gossip: vec![],
        };
        let frame = {
            let bytes = rmp_serde::to_vec(&pingreq).unwrap();
            Frame {
                version: PROTOCOL_VERSION,
                msg_type: MsgType::Swim,
                request_id: 42,
                header: rmpv::Value::Nil,
                payload: rmpv::Value::Binary(bytes),
            }
        };

        // C handles the ping-req → pings B.
        let c_out = c.handle_frame(&frame);
        let to_b = c_out.iter().find(|o| o.node_id == 2).expect("C pings B");
        // B acks C.
        let b_out = b.handle_frame(&to_b.frame);
        let to_c = b_out.iter().find(|o| o.node_id == 3).expect("B acks C");
        // C forwards an indirect ack to A.
        let c_fwd = c.handle_frame(&to_c.frame);
        let to_a = c_fwd
            .iter()
            .find(|o| o.node_id == 1)
            .expect("C forwards indirect ack to A");
        let fwd_env = env_of(&to_a.frame);
        assert_eq!(
            fwd_env.kind,
            SwimKind::Ack { about: Some(2) },
            "forwarded ack confirms target 2 is alive"
        );
        // A consumes it and keeps node 2 alive.
        let _ = a.handle_frame(&to_a.frame);
        assert_eq!(a.state_of(2), Some(NodeState::Alive));
    }
}
