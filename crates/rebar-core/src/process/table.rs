use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use dashmap::mapref::one::Ref;
use rustc_hash::FxBuildHasher;
#[cfg(feature = "tracing")]
use tracing::instrument;

use crate::process::mailbox::MailboxTx;
use crate::process::monitor::{DownMessage, MonitorRef};
use crate::process::{Message, ProcessId, SendError};

/// Errors from name registration operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    /// The name is already registered to a live process.
    #[error("name '{0}' is already registered to a live process")]
    NameTaken(String),
    /// The process being registered is not alive.
    #[error("process {0} is not alive")]
    ProcessNotAlive(ProcessId),
}

/// A hook invoked with the PID of every process that exits.
type ExitHook = Arc<dyn Fn(ProcessId) + Send + Sync>;

/// Handle to a process, wrapping the mailbox sender.
///
/// Each process in the table has a handle that allows sending messages
/// to its mailbox.
pub struct ProcessHandle {
    tx: MailboxTx,
}

impl ProcessHandle {
    /// Create a new process handle wrapping the given mailbox sender.
    #[must_use]
    pub const fn new(tx: MailboxTx) -> Self {
        Self { tx }
    }

    /// Send a message to this process's mailbox.
    ///
    /// # Errors
    ///
    /// Returns `SendError::ProcessDead` or `SendError::MailboxFull` if the
    /// mailbox cannot accept the message.
    pub fn send(&self, msg: Message) -> Result<(), SendError> {
        self.tx.send(msg)
    }
}

/// Table of all processes on this node.
///
/// Uses `DashMap` for concurrent access and `AtomicU64` for lock-free
/// PID allocation. All methods are safe to call from multiple threads
/// concurrently.
pub struct ProcessTable {
    node_id: u64,
    next_id: AtomicU64,
    processes: DashMap<ProcessId, ProcessHandle, FxBuildHasher>,
    /// Registered names: name -> PID (like Erlang's `register/2`).
    names: DashMap<String, ProcessId, FxBuildHasher>,
    /// Reverse index of registered names, for cleanup on process exit.
    names_by_pid: DashMap<ProcessId, Vec<String>, FxBuildHasher>,
    /// Monitors watching a target: target -> [(ref, watcher)].
    monitors_by_target: DashMap<ProcessId, Vec<(MonitorRef, ProcessId)>, FxBuildHasher>,
    /// Monitors held by a watcher, for cleanup when the watcher exits.
    monitors_by_watcher: DashMap<ProcessId, Vec<MonitorRef>, FxBuildHasher>,
    /// Monitor ref -> (watcher, target), for demonitor.
    monitor_index: DashMap<MonitorRef, (ProcessId, ProcessId), FxBuildHasher>,
    /// Hooks invoked with the PID of every exiting process (pg cleanup, etc.).
    exit_hooks: RwLock<Vec<ExitHook>>,
}

impl ProcessTable {
    /// Create a new process table for the given node ID.
    #[must_use]
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            next_id: AtomicU64::new(1),
            processes: DashMap::with_hasher(FxBuildHasher),
            names: DashMap::with_hasher(FxBuildHasher),
            names_by_pid: DashMap::with_hasher(FxBuildHasher),
            monitors_by_target: DashMap::with_hasher(FxBuildHasher),
            monitors_by_watcher: DashMap::with_hasher(FxBuildHasher),
            monitor_index: DashMap::with_hasher(FxBuildHasher),
            exit_hooks: RwLock::new(Vec::new()),
        }
    }

    /// Create a new process table with a pre-sized capacity hint.
    ///
    /// Pre-sizing avoids rehashing when the expected number of processes is known.
    #[must_use]
    pub fn with_capacity(node_id: u64, capacity: usize) -> Self {
        Self {
            node_id,
            next_id: AtomicU64::new(1),
            processes: DashMap::with_capacity_and_hasher(capacity, FxBuildHasher),
            names: DashMap::with_hasher(FxBuildHasher),
            names_by_pid: DashMap::with_hasher(FxBuildHasher),
            monitors_by_target: DashMap::with_hasher(FxBuildHasher),
            monitors_by_watcher: DashMap::with_hasher(FxBuildHasher),
            monitor_index: DashMap::with_hasher(FxBuildHasher),
            exit_hooks: RwLock::new(Vec::new()),
        }
    }

    /// Allocate a new unique process ID on this node.
    ///
    /// Uses atomic fetch-and-add for lock-free, concurrent-safe allocation.
    /// PIDs start at 1 and increment monotonically.
    #[must_use]
    pub fn allocate_pid(&self) -> ProcessId {
        let local_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        ProcessId::new(self.node_id, local_id)
    }

    /// Insert a process handle into the table under the given PID.
    #[cfg_attr(feature = "tracing", instrument(level = "trace", skip(self, handle)))]
    pub fn insert(&self, pid: ProcessId, handle: ProcessHandle) {
        self.processes.insert(pid, handle);
    }

    /// Look up a process by its PID.
    ///
    /// Returns a reference guard that holds a read lock on the entry.
    /// Returns `None` if the PID is not in the table.
    pub fn get(&self, pid: &ProcessId) -> Option<Ref<'_, ProcessId, ProcessHandle>> {
        self.processes.get(pid)
    }

    /// Remove a process from the table.
    ///
    /// Returns the removed PID and handle, or `None` if the PID was not found.
    #[cfg_attr(feature = "tracing", instrument(level = "trace", skip(self)))]
    pub fn remove(&self, pid: &ProcessId) -> Option<(ProcessId, ProcessHandle)> {
        self.processes.remove(pid)
    }

    /// Send a message to a process by its PID.
    ///
    /// # Errors
    ///
    /// Returns `SendError::ProcessDead` if the PID is not in the table.
    #[cfg_attr(feature = "tracing", instrument(level = "trace", skip(self, msg)))]
    pub fn send(&self, pid: ProcessId, msg: Message) -> Result<(), SendError> {
        // Re-map the mailbox error to the *destination* PID. The mailbox layer
        // only knows the sender, so without this a `ProcessDead` / `MailboxFull`
        // would name the wrong process and mislead callers that key liveness
        // decisions off the reported PID.
        self.processes.get(&pid).map_or(
            Err(SendError::ProcessDead(pid)),
            |handle| {
                handle.send(msg).map_err(|e| match e {
                    SendError::MailboxFull(_) => SendError::MailboxFull(pid),
                    _ => SendError::ProcessDead(pid),
                })
            },
        )
    }

    /// Return the number of processes currently in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.processes.len()
    }

    /// Return whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }

    // --- Name registry ---

    /// Register a name for a live process (like Erlang's `register/2`).
    ///
    /// The name is automatically unregistered when the process exits.
    /// A name that still points to a dead process may be re-claimed.
    ///
    /// # Errors
    ///
    /// Returns `RegistryError::ProcessNotAlive` if `pid` is not in the table,
    /// or `RegistryError::NameTaken` if the name is registered to a different
    /// live process.
    pub fn register(&self, name: impl Into<String>, pid: ProcessId) -> Result<(), RegistryError> {
        self.register_inner(name.into(), pid, false)
    }

    /// Register a name, replacing any existing registration (takeover).
    ///
    /// Used by supervisors when restarting a named child: the new incarnation
    /// claims the name even if the old incarnation has not finished its
    /// cleanup yet.
    ///
    /// # Errors
    ///
    /// Returns `RegistryError::ProcessNotAlive` if `pid` is not in the table.
    pub fn reregister(
        &self,
        name: impl Into<String>,
        pid: ProcessId,
    ) -> Result<(), RegistryError> {
        self.register_inner(name.into(), pid, true)
    }

    fn register_inner(
        &self,
        name: String,
        pid: ProcessId,
        force: bool,
    ) -> Result<(), RegistryError> {
        if !self.processes.contains_key(&pid) {
            return Err(RegistryError::ProcessNotAlive(pid));
        }
        let previous = match self.names.entry(name.clone()) {
            Entry::Occupied(mut entry) => {
                let existing = *entry.get();
                if existing == pid {
                    return Ok(()); // already registered to this pid
                }
                if !force && self.processes.contains_key(&existing) {
                    return Err(RegistryError::NameTaken(name));
                }
                *entry.get_mut() = pid;
                Some(existing)
            }
            Entry::Vacant(entry) => {
                entry.insert(pid);
                None
            }
        };
        if let Some(old_pid) = previous
            && let Some(mut names) = self.names_by_pid.get_mut(&old_pid)
        {
            names.retain(|n| n != &name);
        }
        self.names_by_pid.entry(pid).or_default().push(name.clone());

        // The process may have exited — and its `cleanup_process` already run —
        // between the initial liveness check and these inserts. Re-check and
        // roll back so a name never lingers pointing at a dead PID (the same
        // race `monitor` guards against).
        if !self.processes.contains_key(&pid) {
            self.names.remove_if(&name, |_, p| *p == pid);
            if let Some(mut names) = self.names_by_pid.get_mut(&pid) {
                names.retain(|n| n != &name);
            }
            self.names_by_pid.remove_if(&pid, |_, v| v.is_empty());
            return Err(RegistryError::ProcessNotAlive(pid));
        }
        Ok(())
    }

    /// Look up the PID registered under a name (like Erlang's `whereis/1`).
    #[must_use]
    pub fn whereis(&self, name: &str) -> Option<ProcessId> {
        self.names.get(name).map(|entry| *entry)
    }

    /// Remove a name registration. Returns the PID it pointed to, if any.
    #[must_use = "returns the PID the name pointed to; check for None to detect a missing registration"]
    pub fn unregister(&self, name: &str) -> Option<ProcessId> {
        let (name, pid) = self.names.remove(name)?;
        if let Some(mut names) = self.names_by_pid.get_mut(&pid) {
            names.retain(|n| n != &name);
        }
        Some(pid)
    }

    // --- Monitors ---

    /// Have `watcher` monitor `target` (like Erlang's `monitor/2`).
    ///
    /// When `target` exits, a [`DownMessage`] is delivered to `watcher`'s
    /// mailbox (sender = the dead PID). If `target` is already dead, the
    /// `DownMessage` (reason `"noproc"`) is delivered immediately.
    pub fn monitor(&self, watcher: ProcessId, target: ProcessId) -> MonitorRef {
        let mref = MonitorRef::new();
        if !self.processes.contains_key(&target) {
            self.send_down(mref, watcher, target, "noproc");
            return mref;
        }
        self.monitor_index.insert(mref, (watcher, target));
        self.monitors_by_target
            .entry(target)
            .or_default()
            .push((mref, watcher));
        self.monitors_by_watcher
            .entry(watcher)
            .or_default()
            .push(mref);
        // The target may have exited between the liveness check and the
        // bookkeeping above, in which case its cleanup may have already
        // missed this monitor. Re-check and fire manually if so.
        if !self.processes.contains_key(&target) && self.monitor_index.remove(&mref).is_some() {
            if let Some(mut entries) = self.monitors_by_target.get_mut(&target) {
                entries.retain(|(r, _)| *r != mref);
            }
            self.monitors_by_target.remove_if(&target, |_, v| v.is_empty());
            if let Some(mut refs) = self.monitors_by_watcher.get_mut(&watcher) {
                refs.retain(|r| *r != mref);
            }
            self.monitors_by_watcher.remove_if(&watcher, |_, v| v.is_empty());
            self.send_down(mref, watcher, target, "noproc");
        }
        mref
    }

    /// Remove a monitor previously created with [`monitor`](Self::monitor).
    ///
    /// After this call no `DownMessage` will be delivered for this ref.
    /// Removing an unknown or already-fired ref is a no-op.
    pub fn demonitor(&self, mref: MonitorRef) {
        let Some((_, (watcher, target))) = self.monitor_index.remove(&mref) else {
            return;
        };
        if let Some(mut entries) = self.monitors_by_target.get_mut(&target) {
            entries.retain(|(r, _)| *r != mref);
        }
        if let Some(mut refs) = self.monitors_by_watcher.get_mut(&watcher) {
            refs.retain(|r| *r != mref);
        }
    }

    fn send_down(&self, mref: MonitorRef, watcher: ProcessId, target: ProcessId, reason: &str) {
        let down = DownMessage::new(mref, target, reason);
        let _ = self.send(watcher, Message::new_internal(target, down.to_value()));
    }

    // --- Exit hooks ---

    /// Add a hook invoked with the PID of every process that exits.
    ///
    /// Used to keep external structures (e.g. pg scopes) free of dead PIDs.
    ///
    /// Exit hooks are run inside a panic-isolating boundary, so the lock is
    /// never poisoned by a misbehaving hook.
    pub fn add_exit_hook(&self, hook: impl Fn(ProcessId) + Send + Sync + 'static) {
        self.exit_hooks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::new(hook));
    }

    /// Remove a process and run all death-time cleanup: unregister its
    /// names, deliver `DOWN` messages to its monitors, drop monitors it
    /// held on others, and invoke exit hooks.
    ///
    /// This is the runtime's canonical exit path; [`remove`](Self::remove)
    /// only removes the table entry and must not be used to retire a live
    /// process (it would leak names/monitors and never fire `DOWN`).
    pub fn cleanup_process(&self, pid: ProcessId) -> Option<(ProcessId, ProcessHandle)> {
        let removed = self.processes.remove(&pid)?;

        // Unregister names that still point to this process.
        if let Some((_, names)) = self.names_by_pid.remove(&pid) {
            for name in names {
                self.names.remove_if(&name, |_, p| *p == pid);
            }
        }

        // Notify monitors watching this process.
        if let Some((_, entries)) = self.monitors_by_target.remove(&pid) {
            for (mref, watcher) in entries {
                // Only fire if the monitor is still live: a concurrent
                // `demonitor`, or `monitor`'s own race-recovery path, may have
                // already consumed this ref. Gating on the removal result is
                // the single-firing arbiter that prevents a duplicate DOWN or
                // a DOWN delivered after a successful demonitor.
                if self.monitor_index.remove(&mref).is_some() {
                    if let Some(mut refs) = self.monitors_by_watcher.get_mut(&watcher) {
                        refs.retain(|r| *r != mref);
                    }
                    self.send_down(mref, watcher, pid, "exit");
                }
            }
        }

        // Drop monitors this process held on others.
        if let Some((_, mrefs)) = self.monitors_by_watcher.remove(&pid) {
            for mref in mrefs {
                if let Some((_, (_watcher, target))) = self.monitor_index.remove(&mref)
                    && let Some(mut entries) = self.monitors_by_target.get_mut(&target)
                {
                    entries.retain(|(r, _)| *r != mref);
                }
            }
        }

        let hooks: Vec<ExitHook> = self
            .exit_hooks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for hook in hooks {
            // Isolate hook panics: one bad hook must neither abort cleanup of
            // the remaining hooks nor — since cleanup runs inside a `Drop`
            // during a process's own panic unwind — trigger a double-panic
            // that aborts the whole node.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(pid)));
        }

        Some(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessId;
    use crate::process::monitor::DownMessage;

    /// Insert a live process with a fresh mailbox; returns (pid, rx).
    fn live_process(table: &ProcessTable) -> (ProcessId, crate::process::mailbox::MailboxRx) {
        let pid = table.allocate_pid();
        let (tx, rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        (pid, rx)
    }

    #[test]
    fn send_to_dead_destination_reports_destination_pid() {
        let table = ProcessTable::new(1);
        let sender = ProcessId::new(1, 7);
        let dest = ProcessId::new(1, 999);
        let err = table
            .send(dest, Message::new_internal(sender, rmpv::Value::Nil))
            .unwrap_err();
        // Must name the destination, never the sender.
        assert!(matches!(err, SendError::ProcessDead(p) if p == dest));
    }

    #[test]
    fn cleanup_fires_single_down_to_watcher() {
        let table = ProcessTable::new(1);
        let (watcher, mut wrx) = live_process(&table);
        let (target, _trx) = live_process(&table);
        let _mref = table.monitor(watcher, target);
        table.cleanup_process(target);
        let msg = wrx.try_recv().expect("watcher gets a DOWN");
        let down = DownMessage::from_value(msg.payload()).expect("is DOWN");
        assert_eq!(down.pid, target);
        assert_eq!(down.reason, "exit");
        // Exactly one DOWN — no duplicate.
        assert!(wrx.try_recv().is_none());
    }

    #[test]
    fn demonitor_then_cleanup_delivers_no_down() {
        let table = ProcessTable::new(1);
        let (watcher, mut wrx) = live_process(&table);
        let (target, _trx) = live_process(&table);
        let mref = table.monitor(watcher, target);
        table.demonitor(mref);
        table.cleanup_process(target);
        // Contract: no DOWN after a successful demonitor.
        assert!(wrx.try_recv().is_none());
    }

    #[test]
    fn registered_name_is_unregistered_on_cleanup() {
        let table = ProcessTable::new(1);
        let (pid, _rx) = live_process(&table);
        table.register("svc", pid).unwrap();
        assert_eq!(table.whereis("svc"), Some(pid));
        table.cleanup_process(pid);
        assert_eq!(table.whereis("svc"), None);
    }

    #[test]
    fn register_dead_pid_is_rejected_and_leaves_no_entry() {
        let table = ProcessTable::new(1);
        let dead = ProcessId::new(1, 4242);
        let err = table.register("svc", dead).unwrap_err();
        assert_eq!(err, RegistryError::ProcessNotAlive(dead));
        assert_eq!(table.whereis("svc"), None);
    }

    #[test]
    fn exit_hook_panic_is_isolated_and_lock_not_poisoned() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let table = ProcessTable::new(1);
        let after = Arc::new(AtomicU32::new(0));
        let after2 = Arc::clone(&after);
        table.add_exit_hook(|_pid| panic!("bad hook"));
        table.add_exit_hook(move |_pid| {
            after2.fetch_add(1, Ordering::SeqCst);
        });

        let (pid, _rx) = live_process(&table);
        // Must not panic out of cleanup despite the first hook panicking.
        table.cleanup_process(pid);
        // Later hook still ran.
        assert_eq!(after.load(Ordering::SeqCst), 1);
        // Lock is still usable (not poisoned).
        let (pid2, _rx2) = live_process(&table);
        table.add_exit_hook(|_pid| {});
        table.cleanup_process(pid2);
    }

    #[test]
    fn allocate_pid_increments() {
        let table = ProcessTable::new(1);
        let pid1 = table.allocate_pid();
        let pid2 = table.allocate_pid();
        assert_eq!(pid1.node_id(), 1);
        assert_eq!(pid1.local_id(), 1);
        assert_eq!(pid2.local_id(), 2);
    }

    #[test]
    fn allocate_pid_node_id_preserved() {
        let table = ProcessTable::new(42);
        let pid = table.allocate_pid();
        assert_eq!(pid.node_id(), 42);
    }

    #[test]
    fn insert_and_lookup() {
        let table = ProcessTable::new(1);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        assert!(table.get(&pid).is_some());
    }

    #[test]
    fn lookup_missing_returns_none() {
        let table = ProcessTable::new(1);
        assert!(table.get(&ProcessId::new(1, 999)).is_none());
    }

    #[test]
    fn remove_process() {
        let table = ProcessTable::new(1);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        table.remove(&pid);
        assert!(table.get(&pid).is_none());
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let table = ProcessTable::new(1);
        assert!(table.remove(&ProcessId::new(1, 999)).is_none());
    }

    #[test]
    fn send_to_process() {
        let table = ProcessTable::new(1);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        let msg = crate::process::Message::new(ProcessId::new(1, 0), rmpv::Value::Nil);
        assert!(table.send(pid, msg).is_ok());
    }

    #[test]
    fn send_to_dead_process_returns_error() {
        let table = ProcessTable::new(1);
        let msg = crate::process::Message::new(ProcessId::new(1, 0), rmpv::Value::Nil);
        assert!(table.send(ProcessId::new(1, 999), msg).is_err());
    }

    #[test]
    fn process_count() {
        let table = ProcessTable::new(1);
        assert_eq!(table.len(), 0);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn concurrent_allocate_pids_unique() {
        use std::collections::HashSet;
        use std::sync::Arc;
        let table = Arc::new(ProcessTable::new(1));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let t = Arc::clone(&table);
            handles.push(std::thread::spawn(move || {
                (0..100).map(|_| t.allocate_pid()).collect::<Vec<_>>()
            }));
        }
        let mut all_pids = HashSet::new();
        for h in handles {
            for pid in h.join().unwrap() {
                assert!(all_pids.insert(pid), "duplicate PID: {pid}");
            }
        }
        assert_eq!(all_pids.len(), 1000);
    }

    #[test]
    fn concurrent_insert_and_send() {
        use std::sync::Arc;
        let table = Arc::new(ProcessTable::new(1));
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        let mut handles = Vec::new();
        for i in 0..10u64 {
            let t = Arc::clone(&table);
            handles.push(std::thread::spawn(move || {
                let msg = crate::process::Message::new(
                    ProcessId::new(1, 0),
                    rmpv::Value::Integer(i.into()),
                );
                t.send(pid, msg)
            }));
        }
        for h in handles {
            assert!(h.join().unwrap().is_ok());
        }
    }

    #[test]
    fn with_capacity_works() {
        let table = ProcessTable::with_capacity(1, 1000);
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        assert!(table.get(&pid).is_some());
    }

    #[test]
    fn is_empty() {
        let table = ProcessTable::new(1);
        assert!(table.is_empty());
        let pid = table.allocate_pid();
        let (tx, _rx) = crate::process::mailbox::Mailbox::unbounded();
        table.insert(pid, ProcessHandle::new(tx));
        assert!(!table.is_empty());
    }
}
