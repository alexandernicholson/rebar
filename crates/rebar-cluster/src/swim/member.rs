use std::collections::HashMap;
use std::net::SocketAddr;

use rand::seq::IteratorRandom;

use super::gossip::GossipUpdate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Alive,
    Suspect,
    Dead,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub node_id: u64,
    pub addr: SocketAddr,
    pub state: NodeState,
    pub incarnation: u64,
    pub cert_hash: Option<[u8; 32]>,
}

impl Member {
    #[must_use]
    pub const fn new(node_id: u64, addr: SocketAddr) -> Self {
        Self {
            node_id,
            addr,
            state: NodeState::Alive,
            incarnation: 0,
            cert_hash: None,
        }
    }

    pub fn suspect(&mut self, incarnation: u64) {
        if self.state == NodeState::Dead {
            return;
        }
        if !Self::incarnation_acceptable(self.incarnation, incarnation) {
            return;
        }
        if incarnation >= self.incarnation {
            self.state = NodeState::Suspect;
            self.incarnation = incarnation;
        }
    }

    pub const fn alive(&mut self, incarnation: u64) {
        // A higher-incarnation Alive may supersede a Dead declaration: the node
        // has demonstrably rejoined with a fresher incarnation.
        if !Self::incarnation_acceptable(self.incarnation, incarnation) {
            return;
        }
        if incarnation > self.incarnation {
            self.state = NodeState::Alive;
            self.incarnation = incarnation;
        }
    }

    /// Mark the member dead at the given incarnation.
    ///
    /// A Dead update only takes effect if it is at least as fresh as the
    /// incarnation we currently know about. This prevents a replayed, stale
    /// `Dead` from permanently killing a node that has since refuted.
    pub fn dead(&mut self, incarnation: u64) {
        if self.state == NodeState::Dead {
            return;
        }
        if !Self::incarnation_acceptable(self.incarnation, incarnation) {
            return;
        }
        if incarnation >= self.incarnation {
            self.state = NodeState::Dead;
            self.incarnation = incarnation;
        }
    }

    /// Reject absurd incarnation jumps. A crafted `u64::MAX` (or any jump far
    /// beyond the plausible range) would otherwise pin a node forever, since no
    /// legitimate refutation could ever exceed it. We cap the acceptable
    /// forward jump.
    const MAX_INCARNATION_JUMP: u64 = 1 << 32;

    #[must_use]
    const fn incarnation_acceptable(current: u64, proposed: u64) -> bool {
        if proposed <= current {
            return true;
        }
        proposed - current <= Self::MAX_INCARNATION_JUMP
    }
}

pub struct MembershipList {
    members: HashMap<u64, Member>,
    /// The local node's id, if this membership list belongs to a running node.
    self_id: Option<u64>,
    /// The local node's own incarnation. Only the node itself bumps this.
    self_incarnation: u64,
    /// The local node's address, used when emitting self-refuting Alive updates.
    self_addr: Option<SocketAddr>,
    /// The local node's certificate hash, echoed in self-refuting Alive updates.
    self_cert_hash: Option<[u8; 32]>,
}

impl Default for MembershipList {
    fn default() -> Self {
        Self::new()
    }
}

impl MembershipList {
    #[must_use]
    pub fn new() -> Self {
        Self {
            members: HashMap::new(),
            self_id: None,
            self_incarnation: 0,
            self_addr: None,
            self_cert_hash: None,
        }
    }

    /// Create a membership list that knows its own identity, enabling
    /// self-refutation of suspicion.
    #[must_use]
    pub fn with_self(node_id: u64, addr: SocketAddr, cert_hash: Option<[u8; 32]>) -> Self {
        let mut list = Self::new();
        list.self_id = Some(node_id);
        list.self_addr = Some(addr);
        list.self_cert_hash = cert_hash;
        let mut me = Member::new(node_id, addr);
        me.cert_hash = cert_hash;
        list.members.insert(node_id, me);
        list
    }

    /// The local node's id, if known.
    #[must_use]
    pub const fn self_id(&self) -> Option<u64> {
        self.self_id
    }

    /// The local node's current incarnation.
    #[must_use]
    pub const fn self_incarnation(&self) -> u64 {
        self.self_incarnation
    }

    /// Handle a gossip update that refers to this node.
    ///
    /// In SWIM only the node itself bumps its own incarnation. When this node
    /// learns it is being suspected (or declared dead) at an incarnation that
    /// is `>=` its own, it bumps its own incarnation past the rumour and emits
    /// a refuting `Alive` for the cluster to disseminate.
    ///
    /// Returns the refuting update if one was produced.
    pub fn refute_about_self(&mut self, suspected_incarnation: u64) -> Option<GossipUpdate> {
        let self_id = self.self_id?;
        let addr = self.self_addr?;
        if suspected_incarnation < self.self_incarnation {
            // Stale rumour about an old incarnation; our current Alive already
            // supersedes it. Nothing new to broadcast.
            return None;
        }
        let next = self.self_incarnation.saturating_add(1).max(
            suspected_incarnation.saturating_add(1),
        );
        self.self_incarnation = next;
        if let Some(me) = self.members.get_mut(&self_id) {
            me.state = NodeState::Alive;
            me.incarnation = next;
        }
        Some(GossipUpdate::Alive {
            node_id: self_id,
            addr,
            incarnation: next,
            cert_hash: self.self_cert_hash,
        })
    }

    pub fn add(&mut self, member: Member) {
        self.members.insert(member.node_id, member);
    }

    /// Apply a gossip update received from a peer.
    ///
    /// If the update concerns this node (Suspect/Dead/Leave about self), this
    /// bumps our own incarnation and returns a refuting `Alive` to disseminate.
    /// Otherwise the update is merged respecting incarnation rules, adding the
    /// member if previously unknown so we begin probing it.
    pub fn apply_update(&mut self, update: &GossipUpdate) -> Option<GossipUpdate> {
        let node_id = update.node_id();

        if self.self_id == Some(node_id) {
            return match update {
                GossipUpdate::Suspect { incarnation, .. }
                | GossipUpdate::Dead { incarnation, .. }
                | GossipUpdate::Leave { incarnation, .. } => self.refute_about_self(*incarnation),
                GossipUpdate::Alive { .. } => None,
            };
        }

        match update {
            GossipUpdate::Alive {
                addr,
                incarnation,
                cert_hash,
                ..
            } => {
                if let Some(m) = self.members.get_mut(&node_id) {
                    m.alive(*incarnation);
                } else {
                    let mut m = Member::new(node_id, *addr);
                    m.incarnation = *incarnation;
                    m.cert_hash = *cert_hash;
                    self.members.insert(node_id, m);
                }
            }
            GossipUpdate::Suspect {
                addr, incarnation, ..
            } => {
                self.members
                    .entry(node_id)
                    .or_insert_with(|| Member::new(node_id, *addr))
                    .suspect(*incarnation);
            }
            GossipUpdate::Dead { incarnation, .. } | GossipUpdate::Leave { incarnation, .. } => {
                if let Some(m) = self.members.get_mut(&node_id) {
                    m.dead(*incarnation);
                }
            }
        }
        None
    }

    /// The address of a known member, if any.
    #[must_use]
    pub fn addr_of(&self, node_id: u64) -> Option<SocketAddr> {
        self.members.get(&node_id).map(|m| m.addr)
    }

    /// All non-dead peers (excluding self) as `(node_id, addr)` pairs.
    #[must_use]
    pub fn alive_peers(&self) -> Vec<(u64, SocketAddr)> {
        self.members
            .values()
            .filter(|m| Some(m.node_id) != self.self_id && m.state != NodeState::Dead)
            .map(|m| (m.node_id, m.addr))
            .collect()
    }

    #[must_use]
    pub fn get(&self, node_id: u64) -> Option<&Member> {
        self.members.get(&node_id)
    }

    pub fn get_mut(&mut self, node_id: u64) -> Option<&mut Member> {
        self.members.get_mut(&node_id)
    }

    /// Mark a node dead at the given incarnation. Respects incarnation rules:
    /// a stale Dead cannot override a fresher Alive/Suspect.
    pub fn mark_dead(&mut self, node_id: u64, incarnation: u64) {
        if let Some(m) = self.members.get_mut(&node_id) {
            m.dead(incarnation);
        }
    }

    pub fn remove_dead(&mut self) {
        self.members.retain(|_, m| m.state != NodeState::Dead);
    }

    pub fn remove_node(&mut self, node_id: u64) {
        self.members.remove(&node_id);
    }

    #[must_use]
    pub fn alive_count(&self) -> usize {
        self.members
            .values()
            .filter(|m| m.state == NodeState::Alive)
            .count()
    }

    #[must_use]
    pub fn random_alive_member(&self, exclude: u64) -> Option<Member> {
        let mut rng = rand::rng();
        self.members
            .values()
            .filter(|m| m.state == NodeState::Alive && m.node_id != exclude)
            .choose(&mut rng)
            .cloned()
    }

    pub fn all_members(&self) -> impl Iterator<Item = &Member> {
        self.members.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_member_is_alive() {
        let member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        assert_eq!(member.state, NodeState::Alive);
        assert_eq!(member.incarnation, 0);
    }

    #[test]
    fn suspect_member() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.suspect(1);
        assert_eq!(member.state, NodeState::Suspect);
    }

    #[test]
    fn refute_suspicion_with_higher_incarnation() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.suspect(0);
        assert_eq!(member.state, NodeState::Suspect);
        member.alive(1);
        assert_eq!(member.state, NodeState::Alive);
        assert_eq!(member.incarnation, 1);
    }

    #[test]
    fn ignore_stale_alive() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.suspect(5);
        member.alive(3); // stale
        assert_eq!(member.state, NodeState::Suspect);
    }

    #[test]
    fn declare_dead() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.dead(0);
        assert_eq!(member.state, NodeState::Dead);
    }

    #[test]
    fn dead_at_same_incarnation_cannot_be_revived() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.alive(5);
        member.dead(5);
        assert_eq!(member.state, NodeState::Dead);
        // An Alive at the same (stale) incarnation does not revive it.
        member.alive(5);
        assert_eq!(member.state, NodeState::Dead);
    }

    #[test]
    fn higher_incarnation_alive_supersedes_dead() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.dead(2);
        assert_eq!(member.state, NodeState::Dead);
        // A node that rejoined with a fresher incarnation can come back.
        member.alive(3);
        assert_eq!(member.state, NodeState::Alive);
        assert_eq!(member.incarnation, 3);
    }

    #[test]
    fn stale_dead_does_not_override_fresh_alive() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.alive(10);
        // A replayed Dead from an older incarnation must not kill the node.
        member.dead(3);
        assert_eq!(member.state, NodeState::Alive);
    }

    #[test]
    fn absurd_incarnation_jump_rejected() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        // A crafted u64::MAX suspicion must be rejected, leaving us refutable.
        member.suspect(u64::MAX);
        assert_eq!(member.state, NodeState::Alive);
        assert_eq!(member.incarnation, 0);
    }

    #[test]
    fn suspect_with_lower_incarnation_ignored() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.alive(5);
        member.suspect(3); // lower than current incarnation
        assert_eq!(member.state, NodeState::Alive);
    }

    #[test]
    fn membership_list_add_and_get() {
        let mut list = MembershipList::new();
        list.add(Member::new(1, "127.0.0.1:4000".parse().unwrap()));
        assert!(list.get(1).is_some());
        assert_eq!(list.alive_count(), 1);
    }

    #[test]
    fn membership_list_remove_dead() {
        let mut list = MembershipList::new();
        list.add(Member::new(1, "127.0.0.1:4000".parse().unwrap()));
        list.mark_dead(1, 0);
        list.remove_dead();
        assert_eq!(list.alive_count(), 0);
        assert!(list.get(1).is_none());
    }

    #[test]
    fn membership_list_random_alive_excludes_self() {
        let mut list = MembershipList::new();
        list.add(Member::new(1, "127.0.0.1:4001".parse().unwrap()));
        list.add(Member::new(2, "127.0.0.1:4002".parse().unwrap()));
        // Exclude node 1 (self)
        for _ in 0..20 {
            let pick = list.random_alive_member(1).unwrap();
            assert_eq!(pick.node_id, 2);
        }
    }

    #[test]
    fn membership_list_random_alive_excludes_dead() {
        let mut list = MembershipList::new();
        list.add(Member::new(1, "127.0.0.1:4001".parse().unwrap()));
        list.add(Member::new(2, "127.0.0.1:4002".parse().unwrap()));
        list.mark_dead(2, 0);
        // Only node 1 is alive, exclude node 0 (self)
        let pick = list.random_alive_member(0);
        assert!(pick.is_some());
        assert_eq!(pick.unwrap().node_id, 1);
    }

    #[test]
    fn membership_list_random_alive_excludes_suspect() {
        let mut list = MembershipList::new();
        list.add(Member::new(1, "127.0.0.1:4001".parse().unwrap()));
        list.add(Member::new(2, "127.0.0.1:4002".parse().unwrap()));
        if let Some(m) = list.get_mut(2) {
            m.suspect(0);
        }
        // Only node 1 is Alive
        for _ in 0..20 {
            let pick = list.random_alive_member(0).unwrap();
            assert_eq!(pick.node_id, 1);
        }
    }

    #[test]
    fn membership_list_alive_count() {
        let mut list = MembershipList::new();
        list.add(Member::new(1, "127.0.0.1:4001".parse().unwrap()));
        list.add(Member::new(2, "127.0.0.1:4002".parse().unwrap()));
        list.add(Member::new(3, "127.0.0.1:4003".parse().unwrap()));
        assert_eq!(list.alive_count(), 3);
        list.mark_dead(2, 0);
        assert_eq!(list.alive_count(), 2);
    }

    #[test]
    fn membership_list_all_members_iter() {
        let mut list = MembershipList::new();
        list.add(Member::new(1, "127.0.0.1:4001".parse().unwrap()));
        list.add(Member::new(2, "127.0.0.1:4002".parse().unwrap()));
        assert_eq!(list.all_members().count(), 2);
    }

    #[test]
    fn membership_list_empty_random_returns_none() {
        let list = MembershipList::new();
        assert!(list.random_alive_member(0).is_none());
    }

    #[test]
    fn node_refutes_suspicion_about_itself() {
        let addr: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        let mut list = MembershipList::with_self(1, addr, None);
        assert_eq!(list.self_incarnation(), 0);

        // A Suspect rumour about ourselves at our current incarnation arrives.
        let refutation = list.refute_about_self(0).expect("should refute");
        match refutation {
            GossipUpdate::Alive {
                node_id,
                incarnation,
                ..
            } => {
                assert_eq!(node_id, 1);
                // We bumped our own incarnation past the rumour.
                assert!(incarnation > 0);
            }
            other => panic!("expected Alive, got {other:?}"),
        }
        assert!(list.self_incarnation() > 0);
        assert_eq!(list.get(1).unwrap().state, NodeState::Alive);
    }

    #[test]
    fn refute_ignores_stale_rumour_about_self() {
        let addr: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        let mut list = MembershipList::with_self(1, addr, None);
        // Bump self incarnation to 5 via a fresh refutation.
        list.refute_about_self(4);
        let inc = list.self_incarnation();
        // A stale rumour at incarnation 1 needs no new refutation.
        assert!(list.refute_about_self(1).is_none());
        assert_eq!(list.self_incarnation(), inc);
    }

    #[test]
    fn member_with_cert_hash() {
        let hash = [42u8; 32];
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.cert_hash = Some(hash);
        assert_eq!(member.cert_hash, Some(hash));
    }
}
