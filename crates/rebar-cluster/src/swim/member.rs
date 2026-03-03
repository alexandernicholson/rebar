use std::collections::HashMap;
use std::net::SocketAddr;

use rand::seq::IteratorRandom;

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
    pub fn new(node_id: u64, addr: SocketAddr) -> Self {
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
        if incarnation >= self.incarnation {
            self.state = NodeState::Suspect;
            self.incarnation = incarnation;
        }
    }

    pub fn alive(&mut self, incarnation: u64) {
        if self.state == NodeState::Dead {
            return;
        }
        if incarnation > self.incarnation {
            self.state = NodeState::Alive;
            self.incarnation = incarnation;
        }
    }

    pub fn dead(&mut self) {
        self.state = NodeState::Dead;
    }
}

pub struct MembershipList {
    members: HashMap<u64, Member>,
}

impl MembershipList {
    pub fn new() -> Self {
        Self {
            members: HashMap::new(),
        }
    }

    pub fn add(&mut self, member: Member) {
        self.members.insert(member.node_id, member);
    }

    pub fn get(&self, node_id: u64) -> Option<&Member> {
        self.members.get(&node_id)
    }

    pub fn get_mut(&mut self, node_id: u64) -> Option<&mut Member> {
        self.members.get_mut(&node_id)
    }

    pub fn mark_dead(&mut self, node_id: u64) {
        if let Some(m) = self.members.get_mut(&node_id) {
            m.dead();
        }
    }

    pub fn remove_dead(&mut self) {
        self.members.retain(|_, m| m.state != NodeState::Dead);
    }

    pub fn remove_node(&mut self, node_id: u64) {
        self.members.remove(&node_id);
    }

    pub fn alive_count(&self) -> usize {
        self.members
            .values()
            .filter(|m| m.state == NodeState::Alive)
            .count()
    }

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
        member.dead();
        assert_eq!(member.state, NodeState::Dead);
    }

    #[test]
    fn dead_cannot_be_revived() {
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.dead();
        member.alive(100);
        assert_eq!(member.state, NodeState::Dead);
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
        list.mark_dead(1);
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
        list.mark_dead(2);
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
        list.mark_dead(2);
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
    fn member_with_cert_hash() {
        let hash = [42u8; 32];
        let mut member = Member::new(1, "127.0.0.1:4000".parse().unwrap());
        member.cert_hash = Some(hash);
        assert_eq!(member.cert_hash, Some(hash));
    }
}
