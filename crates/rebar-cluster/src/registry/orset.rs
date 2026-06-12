use std::collections::HashMap;

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

/// A detected name conflict.
///
/// Two live registrations for the same name (from different tags/nodes)
/// survived a merge. The integration layer is expected to act on these
/// (e.g. terminate the losing process / alert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameConflict {
    pub name: String,
    /// The registration that `lookup` resolves to (deterministic LWW winner).
    pub winner: RegistryEntry,
    /// The registration that lost but is still a live process elsewhere.
    pub loser: RegistryEntry,
}

/// An OR-Set CRDT-based global process name registry.
///
/// Each registration gets a unique tag (UUID v4). Conflict resolution uses
/// Last-Writer-Wins (LWW) based on timestamp, with deterministic tiebreaker
/// on `node_id` (higher `node_id` wins).
///
/// Tombstoned tags cannot be re-added, preventing resurrection after merge.
pub struct Registry {
    entries: HashMap<String, Vec<RegistryEntry>>,
    /// Tombstoned tags. We keep the name the tag was removed under (when known)
    /// so a full-sync `Remove` can reap the live entry regardless of which name
    /// it currently lives under, and a monotonically increasing generation so
    /// stable tombstones can be expired (bounding growth).
    tombstones: HashMap<Uuid, Tombstone>,
    /// Monotonic generation counter assigned to each new tombstone.
    tombstone_gen: u64,
    /// Hard cap on the number of tombstones retained. Oldest are evicted when
    /// exceeded (their tags can no longer be resurrected only if the cluster
    /// has converged, hence callers should also use `expire_tombstones`).
    max_tombstones: usize,
    /// Same-name split-brain conflicts detected on merge, awaiting resolution
    /// by the integration layer.
    conflicts: Vec<NameConflict>,
}

/// Default cap on retained tombstones, bounding memory under churn.
const DEFAULT_MAX_TOMBSTONES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tombstone {
    /// The name this tag was registered under, if known. Empty for tombstones
    /// learned via full-sync where the originating name was lost.
    name: String,
    /// Generation at which this tombstone was created.
    generation: u64,
}

impl Registry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            tombstones: HashMap::new(),
            tombstone_gen: 0,
            max_tombstones: DEFAULT_MAX_TOMBSTONES,
            conflicts: Vec::new(),
        }
    }

    const fn next_gen(&mut self) -> u64 {
        self.tombstone_gen = self.tombstone_gen.saturating_add(1);
        self.tombstone_gen
    }

    /// Insert a tombstone for `tag` removed under `name`, enforcing the cap.
    fn tombstone(&mut self, tag: Uuid, name: &str) {
        let generation = self.next_gen();
        match self.tombstones.get_mut(&tag) {
            Some(existing) => {
                // Upgrade an empty (full-sync) name with a known one.
                if existing.name.is_empty() && !name.is_empty() {
                    existing.name = name.to_string();
                }
            }
            None => {
                self.tombstones.insert(
                    tag,
                    Tombstone {
                        name: name.to_string(),
                        generation,
                    },
                );
            }
        }
        self.enforce_tombstone_cap();
    }

    /// Reap the live entry for `tag` regardless of which name it lives under.
    /// Returns true if a live entry was removed.
    fn reap_tag(&mut self, tag: Uuid) -> bool {
        let mut removed = false;
        let mut empty_names = Vec::new();
        for (name, entries) in &mut self.entries {
            let before = entries.len();
            entries.retain(|e| e.tag != tag);
            if entries.len() != before {
                removed = true;
            }
            if entries.is_empty() {
                empty_names.push(name.clone());
            }
        }
        for name in empty_names {
            self.entries.remove(&name);
        }
        removed
    }

    fn enforce_tombstone_cap(&mut self) {
        while self.tombstones.len() > self.max_tombstones {
            if let Some(&victim) = self
                .tombstones
                .iter()
                .min_by_key(|(_, t)| t.generation)
                .map(|(tag, _)| tag)
            {
                self.tombstones.remove(&victim);
            } else {
                break;
            }
        }
    }

    /// Expire tombstones created at or before `stable_generation`, on the
    /// premise the caller knows they have propagated to all members (analogous
    /// to the dead-node removal delay). This bounds tombstone growth.
    ///
    /// Returns the number of tombstones expired.
    pub fn expire_tombstones(&mut self, stable_generation: u64) -> usize {
        let before = self.tombstones.len();
        self.tombstones
            .retain(|_, t| t.generation > stable_generation);
        before - self.tombstones.len()
    }

    /// The current tombstone generation watermark. A caller that has gossiped
    /// all tombstones up to this value to every member can pass it to
    /// `expire_tombstones`.
    #[must_use]
    pub const fn tombstone_generation(&self) -> u64 {
        self.tombstone_gen
    }

    /// Number of retained tombstones (for diagnostics / tests).
    #[must_use]
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.len()
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
    /// the entry with the higher `node_id` wins (deterministic tiebreaker).
    #[must_use]
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
        let tags: Vec<Uuid> = entries.iter().map(|e| e.tag).collect();
        let mut deltas = Vec::with_capacity(tags.len());
        for tag in tags {
            self.tombstone(tag, name);
            deltas.push(RegistryDelta::Remove {
                name: name.to_string(),
                tag,
            });
        }
        Some(deltas)
    }

    /// Return all current registrations (one winner per name).
    #[must_use]
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
            let removed: Vec<Uuid> = self.entries.get(&name).map_or_else(Vec::new, |entries| {
                entries
                    .iter()
                    .filter(|e| e.pid == pid)
                    .map(|e| e.tag)
                    .collect()
            });
            for tag in &removed {
                self.tombstone(*tag, &name);
            }
            if let Some(entries) = self.entries.get_mut(&name) {
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
            let removed: Vec<Uuid> = self.entries.get(&name).map_or_else(Vec::new, |entries| {
                entries
                    .iter()
                    .filter(|e| e.node_id == node_id)
                    .map(|e| e.tag)
                    .collect()
            });
            for tag in &removed {
                self.tombstone(*tag, &name);
            }
            if let Some(entries) = self.entries.get_mut(&name) {
                entries.retain(|e| e.node_id != node_id);
                if entries.is_empty() {
                    self.entries.remove(&name);
                }
            }
        }
    }

    /// Merge a remote delta into this registry.
    ///
    /// - `Add`: adds the entry if its tag is not tombstoned and not already
    ///   present. If the add introduces a second *live* registration for a name
    ///   that already has one (a split-brain same-name conflict), the conflict
    ///   is recorded (see [`Registry::take_conflicts`]) so the integration layer
    ///   can act. Resolution stays deterministic via LWW; nothing is silently
    ///   dropped.
    /// - `Remove`: tombstones the tag and reaps the matching live entry
    ///   regardless of which name it lives under (so full-sync removes with an
    ///   unknown name still converge).
    pub fn merge_delta(&mut self, delta: RegistryDelta) {
        match delta {
            RegistryDelta::Add(entry) => {
                // A tombstoned tag cannot be re-added (prevents resurrection)
                if self.tombstones.contains_key(&entry.tag) {
                    return;
                }
                // Detect a same-name conflict against the existing live winner
                // BEFORE inserting, so we can compare against the prior state.
                let conflicting_winner = self
                    .lookup(&entry.name)
                    .filter(|w| w.tag != entry.tag && w.pid != entry.pid)
                    .cloned();

                let entries = self.entries.entry(entry.name.clone()).or_default();
                // Check for idempotent add (tag already exists)
                if entries.iter().any(|e| e.tag == entry.tag) {
                    return;
                }
                entries.push(entry.clone());

                if let Some(prior_winner) = conflicting_winner {
                    // Both registrations are live. Determine the deterministic
                    // LWW winner/loser and surface the conflict.
                    let (winner, loser) = if Self::lww_cmp(&entry, &prior_winner).is_gt() {
                        (entry, prior_winner)
                    } else {
                        (prior_winner, entry)
                    };
                    self.conflicts.push(NameConflict {
                        name: winner.name.clone(),
                        winner,
                        loser,
                    });
                }
            }
            RegistryDelta::Remove { name, tag } => {
                self.tombstone(tag, &name);
                // Reap by tag across all names: a full-sync Remove may carry an
                // empty/wrong name, but the tag uniquely identifies the entry.
                self.reap_tag(tag);
            }
        }
    }

    /// Deterministic Last-Writer-Wins ordering between two entries: higher
    /// timestamp wins; ties broken by higher `node_id`.
    fn lww_cmp(a: &RegistryEntry, b: &RegistryEntry) -> std::cmp::Ordering {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.node_id.cmp(&b.node_id))
    }

    /// Drain and return any name conflicts detected since the last call.
    pub fn take_conflicts(&mut self) -> Vec<NameConflict> {
        std::mem::take(&mut self.conflicts)
    }

    /// Whether any unresolved conflicts are pending.
    #[must_use]
    pub const fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Generate deltas representing all current state, for full sync to another node.
    #[must_use]
    pub fn generate_deltas(&self) -> Vec<RegistryDelta> {
        let mut deltas = Vec::new();
        for entries in self.entries.values() {
            for entry in entries {
                deltas.push(RegistryDelta::Add(entry.clone()));
            }
        }
        // Also include tombstones as Remove deltas so the receiver reaps any
        // live entry it learned under that tag and refuses to re-add it. We
        // carry the name the tag was removed under so the receiver can reap
        // deterministically; even if the name is unknown (empty), the receiver
        // reaps by tag.
        for (tag, tombstone) in &self.tombstones {
            deltas.push(RegistryDelta::Remove {
                name: tombstone.name.clone(),
                tag: *tag,
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

    // ── Regression: full-sync remove converges regardless of name ──────

    #[test]
    fn full_sync_remove_with_empty_name_reaps_live_entry() {
        // Node A registers a name, then unregisters it (creating a tombstone).
        // Node B learned the name via an earlier full-sync Add. When A
        // full-syncs again, its tombstone Remove must reap B's live entry even
        // though A no longer knows the name (regression for empty-name Remove).
        let mut reg_a = Registry::new();
        let tag = reg_a.register("worker", pid(1, 1), 1, 100);

        let mut reg_b = Registry::new();
        reg_b.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "worker".to_string(),
            pid: pid(1, 1),
            tag,
            timestamp: 100,
            node_id: 1,
        }));
        assert!(reg_b.lookup("worker").is_some());

        // A unregisters; full-sync deltas now carry the tombstone.
        reg_a.unregister("worker");
        for d in reg_a.generate_deltas() {
            reg_b.merge_delta(d);
        }

        assert!(
            reg_b.lookup("worker").is_none(),
            "remove must converge and reap the live entry"
        );
    }

    #[test]
    fn unregister_propagates_and_reaps_on_peer() {
        // End-to-end: register on A, propagate to B, unregister on A, propagate
        // the Remove deltas to B -> B must drop the entry.
        let mut reg_a = Registry::new();
        let mut reg_b = Registry::new();

        let tag = reg_a.register("svc", pid(1, 5), 1, 100);
        reg_b.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "svc".to_string(),
            pid: pid(1, 5),
            tag,
            timestamp: 100,
            node_id: 1,
        }));
        assert!(reg_b.lookup("svc").is_some());

        let remove_deltas = reg_a.unregister("svc").expect("had registration");
        for d in remove_deltas {
            reg_b.merge_delta(d);
        }
        assert!(reg_b.lookup("svc").is_none());
        // And it cannot be resurrected with the same tag.
        reg_b.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "svc".to_string(),
            pid: pid(1, 5),
            tag,
            timestamp: 100,
            node_id: 1,
        }));
        assert!(reg_b.lookup("svc").is_none());
    }

    // ── Regression: same-name split-brain conflict surfaced ────────────

    #[test]
    fn same_name_conflict_is_reported_on_merge() {
        // Two nodes register the SAME name during a partition (both live).
        let mut reg_a = Registry::new();
        reg_a.register("leader", pid(1, 1), 1, 100);

        let tag_b = Uuid::new_v4();
        // A merges in B's concurrent live registration for the same name.
        reg_a.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "leader".to_string(),
            pid: pid(2, 2),
            tag: tag_b,
            timestamp: 200,
            node_id: 2,
        }));

        assert!(reg_a.has_conflicts(), "conflict must be surfaced");
        let conflicts = reg_a.take_conflicts();
        assert_eq!(conflicts.len(), 1);
        let c = &conflicts[0];
        assert_eq!(c.name, "leader");
        // Deterministic LWW: node 2 has the later timestamp, so it wins.
        assert_eq!(c.winner.pid, pid(2, 2));
        assert_eq!(c.loser.pid, pid(1, 1));
        // Resolution is deterministic and matches lookup.
        assert_eq!(reg_a.lookup("leader").unwrap().pid, pid(2, 2));
        // Conflicts drained.
        assert!(!reg_a.has_conflicts());
    }

    #[test]
    fn no_conflict_for_idempotent_or_distinct_names() {
        let mut reg = Registry::new();
        let tag = Uuid::new_v4();
        let entry = RegistryEntry {
            name: "a".to_string(),
            pid: pid(1, 1),
            tag,
            timestamp: 100,
            node_id: 1,
        };
        reg.merge_delta(RegistryDelta::Add(entry.clone()));
        reg.merge_delta(RegistryDelta::Add(entry)); // idempotent
        reg.merge_delta(RegistryDelta::Add(RegistryEntry {
            name: "b".to_string(),
            pid: pid(2, 1),
            tag: Uuid::new_v4(),
            timestamp: 100,
            node_id: 2,
        }));
        assert!(!reg.has_conflicts());
    }

    // ── Regression: bounded tombstones ─────────────────────────────────

    #[test]
    fn tombstones_can_be_expired() {
        let mut reg = Registry::new();
        reg.register("x", pid(1, 1), 1, 100);
        reg.unregister("x");
        let watermark = reg.tombstone_generation();
        assert_eq!(reg.tombstone_count(), 1);

        // Once the tombstone is known cluster-wide, it can be expired.
        let expired = reg.expire_tombstones(watermark);
        assert_eq!(expired, 1);
        assert_eq!(reg.tombstone_count(), 0);
    }

    #[test]
    fn tombstone_set_is_bounded() {
        let mut reg = Registry::new();
        reg.max_tombstones = 8;
        for i in 0..100u64 {
            let name = format!("svc{i}");
            reg.register(&name, pid(1, i), 1, 100);
            reg.unregister(&name);
        }
        assert!(
            reg.tombstone_count() <= 8,
            "tombstones must stay bounded, got {}",
            reg.tombstone_count()
        );
    }
}
