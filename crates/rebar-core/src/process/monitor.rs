use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::process::ProcessId;

static MONITOR_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A unique reference identifying a specific monitor relationship.
///
/// Each monitor ref is globally unique, allocated from an atomic counter.
/// Implements `Copy` so it can be freely passed around without ownership concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef(u64);

impl Default for MonitorRef {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorRef {
    /// Allocate a new globally-unique monitor reference.
    #[must_use]
    pub fn new() -> Self {
        Self(MONITOR_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Tracks monitor relationships: who is monitoring whom.
///
/// Maintains a dual-index structure for efficient lookup by both
/// monitor ref and target process ID.
pub struct MonitorSet {
    /// Lookup from monitor ref to the target process being monitored.
    by_ref: HashMap<MonitorRef, ProcessId>,
    /// Lookup from target process to all monitor refs watching it.
    by_target: HashMap<ProcessId, Vec<MonitorRef>>,
}

impl Default for MonitorSet {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorSet {
    /// Create a new empty monitor set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_ref: HashMap::new(),
            by_target: HashMap::new(),
        }
    }

    /// Add a monitor on the given target process.
    ///
    /// Returns a unique `MonitorRef` that can be used to remove this
    /// specific monitor later.
    pub fn add_monitor(&mut self, target: ProcessId) -> MonitorRef {
        let mref = MonitorRef::new();
        self.by_ref.insert(mref, target);
        self.by_target.entry(target).or_default().push(mref);
        mref
    }

    /// Remove a monitor by its reference.
    ///
    /// If the reference does not exist, this is a no-op.
    pub fn remove_monitor(&mut self, mref: MonitorRef) {
        if let Some(target) = self.by_ref.remove(&mref)
            && let Some(refs) = self.by_target.get_mut(&target)
        {
            refs.retain(|r| *r != mref);
            if refs.is_empty() {
                self.by_target.remove(&target);
            }
        }
    }

    /// Iterate over all monitor refs that are watching the given target.
    pub fn monitors_for(&self, target: ProcessId) -> impl Iterator<Item = MonitorRef> + '_ {
        self.by_target
            .get(&target)
            .into_iter()
            .flat_map(|refs| refs.iter().copied())
    }
}

/// Tracks bidirectional link relationships between processes.
///
/// A link set belongs to a single process and records which other processes
/// it is linked to. Links are idempotent: adding the same link twice has
/// no additional effect.
pub struct LinkSet {
    links: HashSet<ProcessId>,
}

impl Default for LinkSet {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkSet {
    /// Create a new empty link set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            links: HashSet::new(),
        }
    }

    /// Add a link to the given process. Idempotent.
    pub fn add_link(&mut self, pid: ProcessId) {
        self.links.insert(pid);
    }

    /// Remove a link to the given process.
    pub fn remove_link(&mut self, pid: ProcessId) {
        self.links.remove(&pid);
    }

    /// Check whether this set contains a link to the given process.
    #[must_use]
    pub fn is_linked(&self, pid: ProcessId) -> bool {
        self.links.contains(&pid)
    }

    /// Iterate over all linked process IDs.
    pub fn linked_pids(&self) -> impl Iterator<Item = ProcessId> + '_ {
        self.links.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessId;

    #[test]
    fn monitor_ref_unique() {
        let r1 = MonitorRef::new();
        let r2 = MonitorRef::new();
        assert_ne!(r1, r2);
    }

    #[test]
    fn monitor_ref_is_copy() {
        let r = MonitorRef::new();
        let r2 = r;
        assert_eq!(r, r2);
    }

    #[test]
    fn monitor_set_add_and_query() {
        let mut set = MonitorSet::new();
        let pid = ProcessId::new(1, 1);
        let mref = set.add_monitor(pid);
        let monitors: Vec<_> = set.monitors_for(pid).collect();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0], mref);
    }

    #[test]
    fn monitor_set_multiple_monitors_same_target() {
        let mut set = MonitorSet::new();
        let pid = ProcessId::new(1, 1);
        let r1 = set.add_monitor(pid);
        let r2 = set.add_monitor(pid);
        assert_ne!(r1, r2);
        assert_eq!(set.monitors_for(pid).count(), 2);
    }

    #[test]
    fn monitor_set_remove() {
        let mut set = MonitorSet::new();
        let pid = ProcessId::new(1, 1);
        let mref = set.add_monitor(pid);
        set.remove_monitor(mref);
        assert_eq!(set.monitors_for(pid).count(), 0);
    }

    #[test]
    fn monitor_set_remove_one_of_many() {
        let mut set = MonitorSet::new();
        let pid = ProcessId::new(1, 1);
        let r1 = set.add_monitor(pid);
        let _r2 = set.add_monitor(pid);
        set.remove_monitor(r1);
        assert_eq!(set.monitors_for(pid).count(), 1);
    }

    #[test]
    fn monitor_set_different_targets() {
        let mut set = MonitorSet::new();
        set.add_monitor(ProcessId::new(1, 1));
        set.add_monitor(ProcessId::new(1, 2));
        assert_eq!(set.monitors_for(ProcessId::new(1, 1)).count(), 1);
        assert_eq!(set.monitors_for(ProcessId::new(1, 2)).count(), 1);
    }

    #[test]
    fn monitor_set_remove_nonexistent_is_noop() {
        let mut set = MonitorSet::new();
        set.remove_monitor(MonitorRef::new());
    }

    #[test]
    fn link_set_add_and_contains() {
        let mut set = LinkSet::new();
        let pid = ProcessId::new(1, 1);
        set.add_link(pid);
        assert!(set.is_linked(pid));
    }

    #[test]
    fn link_set_remove() {
        let mut set = LinkSet::new();
        let pid = ProcessId::new(1, 1);
        set.add_link(pid);
        set.remove_link(pid);
        assert!(!set.is_linked(pid));
    }

    #[test]
    fn link_set_linked_pids_iter() {
        let mut set = LinkSet::new();
        set.add_link(ProcessId::new(1, 1));
        set.add_link(ProcessId::new(1, 2));
        assert_eq!(set.linked_pids().count(), 2);
    }

    #[test]
    fn link_set_duplicate_add_is_idempotent() {
        let mut set = LinkSet::new();
        let pid = ProcessId::new(1, 1);
        set.add_link(pid);
        set.add_link(pid);
        assert_eq!(set.linked_pids().count(), 1);
    }
}
