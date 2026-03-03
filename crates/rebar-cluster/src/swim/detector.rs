use std::collections::HashMap;
use std::time::Instant;

use rand::seq::IteratorRandom;

use super::config::SwimConfig;
use super::member::{MembershipList, NodeState};

/// Tracks suspect timers and drives the SWIM failure-detection state machine.
pub struct FailureDetector {
    /// When each node was first suspected. Key = node_id.
    suspect_timers: HashMap<u64, Instant>,
    /// When each dead node was declared dead. Key = node_id.
    dead_timers: HashMap<u64, Instant>,
}

impl FailureDetector {
    pub fn new() -> Self {
        Self {
            suspect_timers: HashMap::new(),
            dead_timers: HashMap::new(),
        }
    }

    /// Select a random alive/suspect member to probe.
    /// Returns `None` if no targets are available.
    /// `self_id` is the local node's id and is excluded from selection.
    pub fn tick(&self, members: &MembershipList, self_id: u64) -> Option<u64> {
        let mut rng = rand::rng();
        members
            .all_members()
            .filter(|m| m.node_id != self_id && m.state != NodeState::Dead)
            .choose(&mut rng)
            .map(|m| m.node_id)
    }

    /// A node responded to a ping (or indirect ping).
    /// If it was suspected, move it back to Alive and remove the suspect timer.
    pub fn record_ack(&mut self, members: &mut MembershipList, node_id: u64) {
        if let Some(member) = members.get_mut(node_id) {
            if member.state == NodeState::Suspect {
                member.state = NodeState::Alive;
                member.incarnation += 1;
            }
        }
        self.suspect_timers.remove(&node_id);
    }

    /// A node did not respond to a direct ping.
    /// Mark it suspect and start a timer (if not already suspected).
    pub fn record_nack(&mut self, members: &mut MembershipList, node_id: u64, now: Instant) {
        if let Some(member) = members.get_mut(node_id) {
            if member.state == NodeState::Alive {
                member.suspect(member.incarnation);
                self.suspect_timers.entry(node_id).or_insert(now);
            }
        }
    }

    /// Check all suspect timers against the config's suspect_timeout.
    /// Returns the list of node_ids that have been newly declared dead.
    pub fn check_suspect_timeouts(
        &mut self,
        members: &mut MembershipList,
        config: &SwimConfig,
        now: Instant,
    ) -> Vec<u64> {
        let mut newly_dead = Vec::new();

        let expired: Vec<u64> = self
            .suspect_timers
            .iter()
            .filter(|(_, suspected_at)| {
                now.duration_since(**suspected_at) >= config.suspect_timeout
            })
            .map(|(&id, _)| id)
            .collect();

        for node_id in expired {
            self.suspect_timers.remove(&node_id);
            members.mark_dead(node_id);
            self.dead_timers.insert(node_id, now);
            newly_dead.push(node_id);
        }

        newly_dead
    }

    /// Remove dead nodes whose removal delay has elapsed.
    /// Returns the list of removed node_ids.
    pub fn remove_expired_dead(
        &mut self,
        members: &mut MembershipList,
        config: &SwimConfig,
        now: Instant,
    ) -> Vec<u64> {
        let mut removed = Vec::new();

        let expired: Vec<u64> = self
            .dead_timers
            .iter()
            .filter(|(_, dead_at)| now.duration_since(**dead_at) >= config.dead_removal_delay)
            .map(|(&id, _)| id)
            .collect();

        for node_id in expired {
            self.dead_timers.remove(&node_id);
            // Remove only this specific node from the membership list.
            if let Some(m) = members.get(node_id) {
                if m.state == NodeState::Dead {
                    removed.push(node_id);
                }
            }
        }

        // Actually remove the dead nodes that have expired.
        for &node_id in &removed {
            members.remove_node(node_id);
        }

        removed
    }

    /// Returns suspect timer map (for testing / diagnostics).
    pub fn suspect_timers(&self) -> &HashMap<u64, Instant> {
        &self.suspect_timers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swim::member::Member;
    use std::time::Duration;

    fn addr(port: u16) -> std::net::SocketAddr {
        format!("127.0.0.1:{}", port).parse().unwrap()
    }

    fn default_config() -> SwimConfig {
        SwimConfig::default()
    }

    #[test]
    fn ping_alive_node_stays_alive() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        let mut detector = FailureDetector::new();

        // Node responds to ping -- record ack
        detector.record_ack(&mut members, 1);

        assert_eq!(members.get(1).unwrap().state, NodeState::Alive);
        assert!(detector.suspect_timers().is_empty());
    }

    #[test]
    fn missed_ping_marks_suspect() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        let mut detector = FailureDetector::new();
        let now = Instant::now();

        detector.record_nack(&mut members, 1, now);

        assert_eq!(members.get(1).unwrap().state, NodeState::Suspect);
        assert!(detector.suspect_timers().contains_key(&1));
    }

    #[test]
    fn suspect_timeout_marks_dead() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        let mut detector = FailureDetector::new();
        let config = default_config(); // suspect_timeout = 5s

        let t0 = Instant::now();
        detector.record_nack(&mut members, 1, t0);
        assert_eq!(members.get(1).unwrap().state, NodeState::Suspect);

        // Check before timeout -- still suspect
        let t1 = t0 + Duration::from_secs(3);
        let dead = detector.check_suspect_timeouts(&mut members, &config, t1);
        assert!(dead.is_empty());
        assert_eq!(members.get(1).unwrap().state, NodeState::Suspect);

        // Check after timeout -- now dead
        let t2 = t0 + Duration::from_secs(6);
        let dead = detector.check_suspect_timeouts(&mut members, &config, t2);
        assert_eq!(dead, vec![1]);
        assert_eq!(members.get(1).unwrap().state, NodeState::Dead);
    }

    #[test]
    fn suspect_node_refutes_with_alive() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        let mut detector = FailureDetector::new();
        let now = Instant::now();

        // Miss a ping -> suspect
        detector.record_nack(&mut members, 1, now);
        assert_eq!(members.get(1).unwrap().state, NodeState::Suspect);

        // Node responds (refutes suspicion)
        detector.record_ack(&mut members, 1);
        assert_eq!(members.get(1).unwrap().state, NodeState::Alive);
        assert!(!detector.suspect_timers().contains_key(&1));
    }

    #[test]
    fn indirect_probe_succeeds_keeps_alive() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        let mut detector = FailureDetector::new();
        let now = Instant::now();

        // Direct probe fails
        detector.record_nack(&mut members, 1, now);
        assert_eq!(members.get(1).unwrap().state, NodeState::Suspect);

        // Indirect probe succeeds -> ack
        detector.record_ack(&mut members, 1);
        assert_eq!(members.get(1).unwrap().state, NodeState::Alive);
        assert!(detector.suspect_timers().is_empty());
    }

    #[test]
    fn dead_node_triggers_callback() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        let mut detector = FailureDetector::new();
        let config = default_config();

        let t0 = Instant::now();
        detector.record_nack(&mut members, 1, t0);

        let t1 = t0 + Duration::from_secs(6);
        let newly_dead = detector.check_suspect_timeouts(&mut members, &config, t1);

        // The return value acts as the "callback" -- caller receives dead node ids
        assert_eq!(newly_dead.len(), 1);
        assert_eq!(newly_dead[0], 1);
    }

    #[test]
    fn multiple_suspects_tracked() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        members.add(Member::new(2, addr(4002)));
        members.add(Member::new(3, addr(4003)));
        let mut detector = FailureDetector::new();
        let config = default_config();

        let t0 = Instant::now();
        detector.record_nack(&mut members, 1, t0);
        detector.record_nack(&mut members, 2, t0);

        assert_eq!(detector.suspect_timers().len(), 2);
        assert_eq!(members.get(1).unwrap().state, NodeState::Suspect);
        assert_eq!(members.get(2).unwrap().state, NodeState::Suspect);
        assert_eq!(members.get(3).unwrap().state, NodeState::Alive);

        // Only node 2 recovers
        detector.record_ack(&mut members, 2);
        assert_eq!(detector.suspect_timers().len(), 1);

        // Expire remaining suspect
        let t1 = t0 + Duration::from_secs(6);
        let dead = detector.check_suspect_timeouts(&mut members, &config, t1);
        assert_eq!(dead, vec![1]);
    }

    #[test]
    fn dead_removal_delay_respected() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        let mut detector = FailureDetector::new();
        let config = SwimConfig::builder()
            .suspect_timeout(Duration::from_secs(1))
            .dead_removal_delay(Duration::from_secs(10))
            .build();

        let t0 = Instant::now();
        detector.record_nack(&mut members, 1, t0);

        // Expire suspect -> dead
        let t1 = t0 + Duration::from_secs(2);
        detector.check_suspect_timeouts(&mut members, &config, t1);
        assert_eq!(members.get(1).unwrap().state, NodeState::Dead);

        // Before removal delay: node still in list
        let t2 = t1 + Duration::from_secs(5);
        let removed = detector.remove_expired_dead(&mut members, &config, t2);
        assert!(removed.is_empty());
        assert!(members.get(1).is_some());

        // After removal delay: node removed
        let t3 = t1 + Duration::from_secs(11);
        let removed = detector.remove_expired_dead(&mut members, &config, t3);
        assert_eq!(removed, vec![1]);
        assert!(members.get(1).is_none());
    }

    #[test]
    fn tick_selects_random_target() {
        let mut members = MembershipList::new();
        members.add(Member::new(1, addr(4001)));
        members.add(Member::new(2, addr(4002)));
        members.add(Member::new(3, addr(4003)));
        let detector = FailureDetector::new();

        // Run many ticks; should always pick a node that isn't self (id=0)
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            let target = detector.tick(&members, 0).unwrap();
            assert!(target == 1 || target == 2 || target == 3);
            seen.insert(target);
        }
        // Probabilistically should have seen all 3 nodes
        assert!(
            seen.len() >= 2,
            "Expected at least 2 distinct targets, got {}",
            seen.len()
        );
    }

    #[test]
    fn no_targets_available_skips_tick() {
        let members = MembershipList::new();
        let detector = FailureDetector::new();

        // No members at all
        assert!(detector.tick(&members, 0).is_none());
    }
}
