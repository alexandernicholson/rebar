use std::sync::Arc;

use dashmap::DashMap;

use crate::process::{ProcessId, SendError};
use crate::router::MessageRouter;

/// Errors from process group operations.
#[derive(Debug, thiserror::Error)]
pub enum PgError {
    /// The process is not a member of the specified group.
    #[error("process not a member of group")]
    NotJoined,
}

/// Events delivered to group monitors.
#[derive(Debug, Clone)]
pub enum PgEvent {
    /// One or more processes joined a group.
    Join {
        /// The group name.
        group: String,
        /// PIDs that joined.
        pids: Vec<ProcessId>,
    },
    /// One or more processes left a group.
    Leave {
        /// The group name.
        group: String,
        /// PIDs that left.
        pids: Vec<ProcessId>,
    },
}

/// A process group scope.
///
/// Groups are organized into scopes to partition the namespace.
/// Equivalent to Erlang's `pg` module.
pub struct PgScope {
    groups: DashMap<String, Vec<ProcessId>>,
}

impl PgScope {
    /// Create a new empty scope.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            groups: DashMap::new(),
        })
    }

    // --- Membership ---

    /// Add a process to a group. A process can join the same group multiple times.
    ///
    /// Equivalent to Erlang's `pg:join/3`.
    pub fn join(&self, group: &str, pid: ProcessId) {
        self.groups
            .entry(group.to_string())
            .or_default()
            .push(pid);
    }

    /// Add multiple processes to a group.
    pub fn join_many(&self, group: &str, pids: &[ProcessId]) {
        self.groups
            .entry(group.to_string())
            .or_default()
            .extend_from_slice(pids);
    }

    /// Remove one membership of a process from a group.
    ///
    /// If the process joined multiple times, only one membership is removed.
    /// Equivalent to Erlang's `pg:leave/3`.
    ///
    /// # Errors
    ///
    /// Returns `PgError::NotJoined` if the process is not a member of the group.
    #[allow(clippy::option_if_let_else)]
    pub fn leave(&self, group: &str, pid: ProcessId) -> Result<(), PgError> {
        let mut entry = self.groups.get_mut(group).ok_or(PgError::NotJoined)?;
        let members = entry.value_mut();
        if let Some(pos) = members.iter().position(|&p| p == pid) {
            members.swap_remove(pos);
            if members.is_empty() {
                drop(entry);
                self.groups.remove(group);
            }
            Ok(())
        } else {
            Err(PgError::NotJoined)
        }
    }

    // --- Queries ---

    /// Get all members of a group.
    ///
    /// Equivalent to Erlang's `pg:get_members/2`.
    #[must_use]
    pub fn get_members(&self, group: &str) -> Vec<ProcessId> {
        self.groups
            .get(group)
            .map(|entry| entry.value().clone())
            .unwrap_or_default()
    }

    /// Get members of a group on a specific node.
    ///
    /// Equivalent to Erlang's `pg:get_local_members/2`.
    #[must_use]
    pub fn get_local_members(&self, group: &str, node_id: u64) -> Vec<ProcessId> {
        self.groups
            .get(group)
            .map(|entry| {
                entry
                    .value()
                    .iter()
                    .filter(|p| p.node_id() == node_id)
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List all groups in this scope.
    ///
    /// Equivalent to Erlang's `pg:which_groups/1`.
    #[must_use]
    pub fn which_groups(&self) -> Vec<String> {
        self.groups.iter().map(|e| e.key().clone()).collect()
    }

    // --- Lifecycle ---

    /// Remove all memberships for a process (called on process death).
    pub fn remove_pid(&self, pid: ProcessId) {
        let mut empty_groups = Vec::new();
        for mut entry in self.groups.iter_mut() {
            entry.value_mut().retain(|&p| p != pid);
            if entry.value().is_empty() {
                empty_groups.push(entry.key().clone());
            }
        }
        for group in empty_groups {
            // Re-check under write lock in case someone joined between iteration
            self.groups.remove_if(&group, |_, v| v.is_empty());
        }
    }

    /// Remove all memberships for a node (called on node disconnect).
    pub fn remove_node(&self, node_id: u64) {
        let mut empty_groups = Vec::new();
        for mut entry in self.groups.iter_mut() {
            entry.value_mut().retain(|p| p.node_id() != node_id);
            if entry.value().is_empty() {
                empty_groups.push(entry.key().clone());
            }
        }
        for group in empty_groups {
            self.groups.remove_if(&group, |_, v| v.is_empty());
        }
    }

    // --- Messaging convenience ---

    /// Send a message to all members of a group.
    ///
    /// Returns a result for each member send attempt.
    pub fn broadcast(
        &self,
        group: &str,
        from: ProcessId,
        payload: &rmpv::Value,
        router: &dyn MessageRouter,
    ) -> Vec<Result<(), SendError>> {
        let members = self.get_members(group);
        members
            .iter()
            .map(|&dest| router.route(from, dest, payload.clone()))
            .collect()
    }

    /// Get the number of members in a group.
    #[must_use]
    pub fn member_count(&self, group: &str) -> usize {
        self.groups
            .get(group)
            .map_or(0, |e| e.value().len())
    }
}

impl Default for PgScope {
    fn default() -> Self {
        Self {
            groups: DashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_adds_to_group() {
        let scope = PgScope::new();
        let pid = ProcessId::new(1, 1);
        scope.join("workers", pid);
        assert_eq!(scope.get_members("workers"), vec![pid]);
    }

    #[test]
    fn join_same_group_twice() {
        let scope = PgScope::new();
        let pid = ProcessId::new(1, 1);
        scope.join("workers", pid);
        scope.join("workers", pid);
        assert_eq!(scope.get_members("workers").len(), 2);
    }

    #[test]
    fn join_multiple_groups() {
        let scope = PgScope::new();
        let pid = ProcessId::new(1, 1);
        scope.join("a", pid);
        scope.join("b", pid);
        assert_eq!(scope.get_members("a"), vec![pid]);
        assert_eq!(scope.get_members("b"), vec![pid]);
    }

    #[test]
    fn leave_removes_from_group() {
        let scope = PgScope::new();
        let pid = ProcessId::new(1, 1);
        scope.join("workers", pid);
        scope.leave("workers", pid).unwrap();
        assert!(scope.get_members("workers").is_empty());
    }

    #[test]
    fn leave_one_membership_when_joined_twice() {
        let scope = PgScope::new();
        let pid = ProcessId::new(1, 1);
        scope.join("workers", pid);
        scope.join("workers", pid);
        scope.leave("workers", pid).unwrap();
        assert_eq!(scope.get_members("workers"), vec![pid]);
    }

    #[test]
    fn leave_not_joined_returns_error() {
        let scope = PgScope::new();
        let pid = ProcessId::new(1, 1);
        assert!(matches!(
            scope.leave("workers", pid),
            Err(PgError::NotJoined)
        ));
    }

    #[test]
    fn join_many_adds_all() {
        let scope = PgScope::new();
        let pids = vec![ProcessId::new(1, 1), ProcessId::new(1, 2)];
        scope.join_many("workers", &pids);
        assert_eq!(scope.get_members("workers").len(), 2);
    }

    #[test]
    fn get_members_empty_group() {
        let scope = PgScope::new();
        assert!(scope.get_members("nonexistent").is_empty());
    }

    #[test]
    fn get_local_members_filters_by_node() {
        let scope = PgScope::new();
        let pid1 = ProcessId::new(1, 1);
        let pid2 = ProcessId::new(2, 1);
        scope.join("workers", pid1);
        scope.join("workers", pid2);
        assert_eq!(scope.get_local_members("workers", 1), vec![pid1]);
        assert_eq!(scope.get_local_members("workers", 2), vec![pid2]);
    }

    #[test]
    fn which_groups_lists_all() {
        let scope = PgScope::new();
        scope.join("a", ProcessId::new(1, 1));
        scope.join("b", ProcessId::new(1, 2));
        let mut groups = scope.which_groups();
        groups.sort();
        assert_eq!(groups, vec!["a", "b"]);
    }

    #[test]
    fn which_groups_empty_scope() {
        let scope = PgScope::new();
        assert!(scope.which_groups().is_empty());
    }

    #[test]
    fn remove_pid_clears_all_memberships() {
        let scope = PgScope::new();
        let pid = ProcessId::new(1, 1);
        scope.join("a", pid);
        scope.join("b", pid);
        scope.remove_pid(pid);
        assert!(scope.get_members("a").is_empty());
        assert!(scope.get_members("b").is_empty());
    }

    #[test]
    fn remove_node_clears_all_for_node() {
        let scope = PgScope::new();
        let pid1 = ProcessId::new(1, 1);
        let pid2 = ProcessId::new(1, 2);
        let pid3 = ProcessId::new(2, 1);
        scope.join("workers", pid1);
        scope.join("workers", pid2);
        scope.join("workers", pid3);
        scope.remove_node(1);
        assert_eq!(scope.get_members("workers"), vec![pid3]);
    }

    #[test]
    fn empty_group_removed_after_last_leave() {
        let scope = PgScope::new();
        let pid = ProcessId::new(1, 1);
        scope.join("workers", pid);
        scope.leave("workers", pid).unwrap();
        assert!(scope.which_groups().is_empty());
    }

    #[test]
    fn broadcast_sends_to_all_members() {
        use crate::process::mailbox::Mailbox;
        use crate::process::table::{ProcessHandle, ProcessTable};
        use crate::router::LocalRouter;

        let table = Arc::new(ProcessTable::new(1));
        let pid1 = table.allocate_pid();
        let pid2 = table.allocate_pid();
        let (tx1, mut rx1) = Mailbox::unbounded();
        let (tx2, mut rx2) = Mailbox::unbounded();
        table.insert(pid1, ProcessHandle::new(tx1));
        table.insert(pid2, ProcessHandle::new(tx2));

        let router = LocalRouter::new(table);
        let scope = PgScope::new();
        scope.join("workers", pid1);
        scope.join("workers", pid2);

        let from = ProcessId::new(1, 0);
        let results = scope.broadcast(
            "workers",
            from,
            &rmpv::Value::String("hello".into()),
            &router,
        );
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(rx1.try_recv().unwrap().payload().as_str().unwrap(), "hello");
        assert_eq!(rx2.try_recv().unwrap().payload().as_str().unwrap(), "hello");
    }

    #[test]
    fn broadcast_empty_group() {
        use crate::process::table::ProcessTable;
        use crate::router::LocalRouter;

        let table = Arc::new(ProcessTable::new(1));
        let router = LocalRouter::new(table);
        let scope = PgScope::new();
        let from = ProcessId::new(1, 0);
        let results = scope.broadcast("empty", from, &rmpv::Value::Nil, &router);
        assert!(results.is_empty());
    }

    #[test]
    fn member_count() {
        let scope = PgScope::new();
        assert_eq!(scope.member_count("workers"), 0);
        scope.join("workers", ProcessId::new(1, 1));
        scope.join("workers", ProcessId::new(1, 2));
        assert_eq!(scope.member_count("workers"), 2);
    }
}
