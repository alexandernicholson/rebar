use std::collections::{HashMap, HashSet};

use rebar_core::process::ProcessId;
use uuid::Uuid;

/// A single registration entry in the OR-Set registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub name: String,
    pub pid: ProcessId,
    pub tag: Uuid,
    pub timestamp: u64,
    pub node_id: u64,
}

/// A delta operation produced by register/unregister, used for replication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryDelta {
    Add(RegistryEntry),
    Remove { name: String, tag: Uuid },
}

/// An OR-Set CRDT-based global process name registry.
///
/// Each registration gets a unique tag (UUID v4). Conflict resolution uses
/// Last-Writer-Wins (LWW) based on timestamp, with deterministic tiebreaker
/// on node_id (higher node_id wins).
///
/// Tombstoned tags cannot be re-added, preventing resurrection after merge.
pub struct Registry {
    entries: HashMap<String, Vec<RegistryEntry>>,
    tombstones: HashSet<Uuid>,
}

impl Registry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            tombstones: HashSet::new(),
        }
    }

    /// Register a name to a process. Returns the unique tag for this registration.
    ///
    /// If the name is already registered, the new registration is added alongside
    /// existing ones. `lookup` uses LWW to pick the winner.
    pub fn register(&mut self, name: &str, pid: ProcessId, node_id: u64, timestamp: u64) -> Uuid {
        let tag = Uuid::new_v4();
        let entry = RegistryEntry {
            name: name.to_string(),
            pid,
            tag,
            timestamp,
            node_id,
        };
        self.entries
            .entry(name.to_string())
            .or_default()
            .push(entry);
        tag
    }

    /// Look up the winning registration for a name.
    ///
    /// Returns the entry with the highest timestamp. If timestamps are equal,
    /// the entry with the higher node_id wins (deterministic tiebreaker).
    pub fn lookup(&self, name: &str) -> Option<&RegistryEntry> {
        self.entries.get(name).and_then(|entries| {
            entries.iter().max_by(|a, b| {
                a.timestamp
                    .cmp(&b.timestamp)
                    .then_with(|| a.node_id.cmp(&b.node_id))
            })
        })
    }

    /// Unregister a name. Moves all tags for this name to the tombstone set.
    ///
    /// Returns a list of `Remove` deltas (one per tag) for replication,
    /// or `None` if the name was not registered.
    pub fn unregister(&mut self, name: &str) -> Option<Vec<RegistryDelta>> {
        let entries = self.entries.remove(name)?;
        if entries.is_empty() {
            return None;
        }
        let deltas: Vec<RegistryDelta> = entries
            .iter()
            .map(|e| {
                self.tombstones.insert(e.tag);
                RegistryDelta::Remove {
                    name: name.to_string(),
                    tag: e.tag,
                }
            })
            .collect();
        Some(deltas)
    }

    /// Return all current registrations (one winner per name).
    pub fn registered(&self) -> Vec<(String, ProcessId)> {
        let mut result = Vec::new();
        for name in self.entries.keys() {
            if let Some(entry) = self.lookup(name) {
                result.push((entry.name.clone(), entry.pid));
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    /// Remove all registrations for a given PID.
    pub fn remove_by_pid(&mut self, pid: ProcessId) {
        let names: Vec<String> = self.entries.keys().cloned().collect();
        for name in names {
            if let Some(entries) = self.entries.get_mut(&name) {
                let removed: Vec<Uuid> = entries
                    .iter()
                    .filter(|e| e.pid == pid)
                    .map(|e| e.tag)
                    .collect();
                for tag in &removed {
                    self.tombstones.insert(*tag);
                }
                entries.retain(|e| e.pid != pid);
                if entries.is_empty() {
                    self.entries.remove(&name);
                }
            }
        }
    }

    /// Remove all registrations from a given node.
    pub fn remove_by_node(&mut self, node_id: u64) {
        let names: Vec<String> = self.entries.keys().cloned().collect();
        for name in names {
            if let Some(entries) = self.entries.get_mut(&name) {
                let removed: Vec<Uuid> = entries
                    .iter()
                    .filter(|e| e.node_id == node_id)
                    .map(|e| e.tag)
                    .collect();
                for tag in &removed {
                    self.tombstones.insert(*tag);
                }
                entries.retain(|e| e.node_id != node_id);
                if entries.is_empty() {
                    self.entries.remove(&name);
                }
            }
        }
    }

    /// Merge a remote delta into this registry.
    ///
    /// - `Add`: adds the entry if its tag is not tombstoned and not already present.
    /// - `Remove`: tombstones the tag and removes the entry from the entries map.
    pub fn merge_delta(&mut self, delta: RegistryDelta) {
        match delta {
            RegistryDelta::Add(entry) => {
                // A tombstoned tag cannot be re-added (prevents resurrection)
                if self.tombstones.contains(&entry.tag) {
                    return;
                }
                // Check for idempotent add (tag already exists)
                let entries = self.entries.entry(entry.name.clone()).or_default();
                if entries.iter().any(|e| e.tag == entry.tag) {
                    return;
                }
                entries.push(entry);
            }
            RegistryDelta::Remove { name, tag } => {
                self.tombstones.insert(tag);
                if let Some(entries) = self.entries.get_mut(&name) {
                    entries.retain(|e| e.tag != tag);
                    if entries.is_empty() {
                        self.entries.remove(&name);
                    }
                }
            }
        }
    }

    /// Generate deltas representing all current state, for full sync to another node.
    pub fn generate_deltas(&self) -> Vec<RegistryDelta> {
        let mut deltas = Vec::new();
        for entries in self.entries.values() {
            for entry in entries {
                deltas.push(RegistryDelta::Add(entry.clone()));
            }
        }
        // Also include tombstones as Remove deltas so the receiver
        // knows not to re-add those tags. We don't have the name for
        // tombstones that have already been fully removed, so we emit
        // them with an empty name -- the receiver only needs the tag
        // to prevent resurrection.
        for &tag in &self.tombstones {
            deltas.push(RegistryDelta::Remove {
                name: String::new(),
                tag,
            });
        }
        deltas
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(node: u64, local: u64) -> ProcessId {
        ProcessId::new(node, local)
    }

    // ── Basic operations ──────────────────────────────────────────────

    #[test]
    fn register_and_lookup() {
        let mut reg = Registry::new();
        let p = pid(1, 100);
        reg.register("my_server", p, 1, 1000);

        let entry = reg.lookup("my_server").expect("should find entry");
        assert_eq!(entry.name, "my_server");
        assert_eq!(entry.pid, p);
        assert_eq!(entry.timestamp, 1000);
        assert_eq!(entry.node_id, 1);
    }

    #[test]
    fn unregister() {
        let mut reg = Registry::new();
        let p = pid(1, 100);
        reg.register("my_server", p, 1, 1000);

        let deltas = reg.unregister("my_server");
        assert!(deltas.is_some());
        assert!(reg.lookup("my_server").is_none());
    }

    #[test]
    fn lookup_nonexistent_returns_none() {
        let reg = Registry::new();
        assert!(reg.lookup("ghost").is_none());
    }

    #[test]
    fn registered_returns_all() {
        let mut reg = Registry::new();
        reg.register("alpha", pid(1, 1), 1, 100);
        reg.register("beta", pid(1, 2), 1, 200);
        reg.register("gamma", pid(2, 1), 2, 300);

        let all = reg.registered();
        assert_eq!(all.len(), 3);
        // sorted by name
        assert_eq!(all[0].0, "alpha");
        assert_eq!(all[1].0, "beta");
        assert_eq!(all[2].0, "gamma");
    }

    // ── Conflict resolution ───────────────────────────────────────────

    #[test]
    fn last_writer_wins() {
        let mut reg = Registry::new();
        let p1 = pid(1, 1);
        let p2 = pid(2, 2);

        reg.register("name_server", p1, 1, 100);
        reg.register("name_server", p2, 2, 200);

        let winner = reg.lookup("name_server").unwrap();
        assert_eq!(winner.pid, p2, "later timestamp should win");
        assert_eq!(winner.timestamp, 200);
    }

    #[test]
    fn conflict_with_same_timestamp_deterministic() {
        let mut reg = Registry::new();
        let p1 = pid(1, 1);
        let p2 = pid(2, 2);

        reg.register("leader", p1, 1, 500);
        reg.register("leader", p2, 2, 500);

        let winner = reg.lookup("leader").unwrap();
        // Higher node_id wins as tiebreaker
        assert_eq!(winner.node_id, 2, "higher node_id should win tiebreaker");
        assert_eq!(winner.pid, p2);
    }

    // ── Cleanup ───────────────────────────────────────────────────────

    #[test]
    fn remove_by_pid_cleans_all_names() {
        let mut reg = Registry::new();
        let p = pid(1, 42);

        reg.register("service_a", p, 1, 100);
        reg.register("service_b", p, 1, 200);

        reg.remove_by_pid(p);

        assert!(reg.lookup("service_a").is_none());
        assert!(reg.lookup("service_b").is_none());
    }

    #[test]
    fn remove_by_pid_doesnt_affect_others() {
        let mut reg = Registry::new();
        let p1 = pid(1, 1);
        let p2 = pid(2, 2);

        reg.register("mine", p1, 1, 100);
        reg.register("yours", p2, 2, 200);

        reg.remove_by_pid(p1);

        assert!(reg.lookup("mine").is_none());
        assert!(reg.lookup("yours").is_some());
    }

    #[test]
    fn remove_by_node_cleans_all_from_node() {
        let mut reg = Registry::new();
        reg.register("svc1", pid(1, 1), 1, 100);
        reg.register("svc2", pid(1, 2), 1, 200);
        reg.register("svc3", pid(2, 1), 2, 300);

        reg.remove_by_node(1);

        assert!(reg.lookup("svc1").is_none());
        assert!(reg.lookup("svc2").is_none());
        assert!(reg.lookup("svc3").is_some());
    }

    #[test]
    fn remove_by_node_preserves_other_nodes() {
        let mut reg = Registry::new();
        reg.register("shared", pid(1, 1), 1, 100);
        reg.register("shared", pid(2, 1), 2, 200);

        reg.remove_by_node(1);

        let entry = reg.lookup("shared").expect("node 2 entry should remain");
        assert_eq!(entry.node_id, 2);
    }

    // ── Delta merging ─────────────────────────────────────────────────

    #[test]
    fn merge_delta_add() {
        let mut reg = Registry::new();
        let tag = Uuid::new_v4();
        let entry = RegistryEntry {
            name: "remote_svc".to_string(),
            pid: pid(2, 10),
            tag,
            timestamp: 500,
            node_id: 2,
        };

        reg.merge_delta(RegistryDelta::Add(entry));

        let found = reg.lookup("remote_svc").unwrap();
        assert_eq!(found.tag, tag);
        assert_eq!(found.pid, pid(2, 10));
    }

    #[test]
    fn merge_delta_remove() {
        let mut reg = Registry::new();
        let tag = reg.register("doomed", pid(1, 1), 1, 100);

        reg.merge_delta(RegistryDelta::Remove {
            name: "doomed".to_string(),
            tag,
        });

        assert!(reg.lookup("doomed").is_none());
    }

    #[test]
    fn merge_delta_add_then_remove() {
        let mut reg = Registry::new();
        let tag = Uuid::new_v4();
        let entry = RegistryEntry {
            name: "ephemeral".to_string(),
            pid: pid(3, 1),
            tag,
            timestamp: 100,
            node_id: 3,
        };

        reg.merge_delta(RegistryDelta::Add(entry));
        assert!(reg.lookup("ephemeral").is_some());

        reg.merge_delta(RegistryDelta::Remove {
            name: "ephemeral".to_string(),
            tag,
        });
        assert!(reg.lookup("ephemeral").is_none());
    }

    #[test]
    fn merge_delta_remove_then_add_with_new_tag() {
        let mut reg = Registry::new();
        let old_tag = reg.register("phoenix", pid(1, 1), 1, 100);

        // Remote removes the old tag
        reg.merge_delta(RegistryDelta::Remove {
            name: "phoenix".to_string(),
            tag: old_tag,
        });
        assert!(reg.lookup("phoenix").is_none());

        // Remote re-registers with a new tag
        let new_tag = Uuid::new_v4();
        reg.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "phoenix".to_string(),
            pid: pid(2, 5),
            tag: new_tag,
            timestamp: 200,
            node_id: 2,
        }));

        let entry = reg.lookup("phoenix").unwrap();
        assert_eq!(entry.tag, new_tag);
        assert_eq!(entry.pid, pid(2, 5));
    }

    #[test]
    fn merge_idempotent_add() {
        let mut reg = Registry::new();
        let tag = Uuid::new_v4();
        let entry = RegistryEntry {
            name: "idempotent".to_string(),
            pid: pid(1, 1),
            tag,
            timestamp: 100,
            node_id: 1,
        };

        reg.merge_delta(RegistryDelta::Add(entry.clone()));
        reg.merge_delta(RegistryDelta::Add(entry));

        // Should still only have one entry for this name with this tag
        let all = reg.registered();
        let count = all.iter().filter(|(n, _)| n == "idempotent").count();
        assert_eq!(count, 1);

        // Also verify in the internal entries map there is only one
        let internal = reg.entries.get("idempotent").unwrap();
        assert_eq!(internal.len(), 1);
    }

    #[test]
    fn merge_tombstoned_tag_not_re_added() {
        let mut reg = Registry::new();
        let tag = Uuid::new_v4();
        let entry = RegistryEntry {
            name: "zombie".to_string(),
            pid: pid(1, 1),
            tag,
            timestamp: 100,
            node_id: 1,
        };

        // Add then remove
        reg.merge_delta(RegistryDelta::Add(entry.clone()));
        reg.merge_delta(RegistryDelta::Remove {
            name: "zombie".to_string(),
            tag,
        });
        assert!(reg.lookup("zombie").is_none());

        // Try to re-add with the same tag -- should be rejected
        reg.merge_delta(RegistryDelta::Add(entry));
        assert!(
            reg.lookup("zombie").is_none(),
            "tombstoned tag must not be resurrected"
        );
    }

    // ── Convergence ───────────────────────────────────────────────────

    #[test]
    fn two_registries_converge_after_delta_exchange() {
        let mut reg_a = Registry::new();
        let mut reg_b = Registry::new();

        // Node A registers "counter"
        let tag_a = reg_a.register("counter", pid(1, 10), 1, 100);

        // Node B registers "logger"
        let tag_b = reg_b.register("logger", pid(2, 20), 2, 200);

        // Exchange deltas: A -> B
        let deltas_a = vec![RegistryDelta::Add(RegistryEntry {
            name: "counter".to_string(),
            pid: pid(1, 10),
            tag: tag_a,
            timestamp: 100,
            node_id: 1,
        })];
        for d in deltas_a {
            reg_b.merge_delta(d);
        }

        // Exchange deltas: B -> A
        let deltas_b = vec![RegistryDelta::Add(RegistryEntry {
            name: "logger".to_string(),
            pid: pid(2, 20),
            tag: tag_b,
            timestamp: 200,
            node_id: 2,
        })];
        for d in deltas_b {
            reg_a.merge_delta(d);
        }

        // Both should now see both names
        assert_eq!(reg_a.registered().len(), 2);
        assert_eq!(reg_b.registered().len(), 2);

        let a_counter = reg_a.lookup("counter").unwrap();
        let b_counter = reg_b.lookup("counter").unwrap();
        assert_eq!(a_counter.pid, b_counter.pid);

        let a_logger = reg_a.lookup("logger").unwrap();
        let b_logger = reg_b.lookup("logger").unwrap();
        assert_eq!(a_logger.pid, b_logger.pid);
    }

    #[test]
    fn concurrent_adds_different_names_merge_cleanly() {
        let mut reg_a = Registry::new();
        let mut reg_b = Registry::new();

        let tag_a = reg_a.register("cache", pid(1, 1), 1, 100);
        let tag_b = reg_b.register("db", pid(2, 1), 2, 100);

        // A -> B
        reg_b.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "cache".to_string(),
            pid: pid(1, 1),
            tag: tag_a,
            timestamp: 100,
            node_id: 1,
        }));

        // B -> A
        reg_a.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "db".to_string(),
            pid: pid(2, 1),
            tag: tag_b,
            timestamp: 100,
            node_id: 2,
        }));

        // Both see both
        let a_all = reg_a.registered();
        let b_all = reg_b.registered();
        assert_eq!(a_all.len(), 2);
        assert_eq!(b_all.len(), 2);
        assert_eq!(a_all, b_all);
    }

    #[test]
    fn concurrent_adds_same_name_lww_after_merge() {
        let mut reg_a = Registry::new();
        let mut reg_b = Registry::new();

        // Both register same name concurrently, but B has later timestamp
        let tag_a = reg_a.register("leader", pid(1, 1), 1, 100);
        let tag_b = reg_b.register("leader", pid(2, 1), 2, 200);

        // A -> B
        reg_b.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "leader".to_string(),
            pid: pid(1, 1),
            tag: tag_a,
            timestamp: 100,
            node_id: 1,
        }));

        // B -> A
        reg_a.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "leader".to_string(),
            pid: pid(2, 1),
            tag: tag_b,
            timestamp: 200,
            node_id: 2,
        }));

        // Both should agree: node 2 wins (higher timestamp)
        let a_leader = reg_a.lookup("leader").unwrap();
        let b_leader = reg_b.lookup("leader").unwrap();
        assert_eq!(a_leader.pid, pid(2, 1));
        assert_eq!(b_leader.pid, pid(2, 1));
        assert_eq!(a_leader.node_id, b_leader.node_id);
    }

    // ── Edge cases ────────────────────────────────────────────────────

    #[test]
    fn register_empty_name() {
        let mut reg = Registry::new();
        let p = pid(1, 1);
        reg.register("", p, 1, 100);

        let entry = reg.lookup("").expect("empty name should be valid");
        assert_eq!(entry.pid, p);
        assert_eq!(entry.name, "");
    }
}
