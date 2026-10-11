//! The user's collapse choices, as one object with a version.
//!
//! Two kinds of state, both following stable identity rather than row position: a process's
//! native collapse (keyed by PID, transient — a vanished PID loses it, and the kernel-thread
//! forest starts collapsed by default), and an application group's fold (keyed by the group's
//! identity; folded by default, with a persisted override for identities stable across runs).
//! Only the user's explicit choice is stored, never a derived state, so the last choice always
//! wins over a default. Every mutation bumps [`Collapse::version`], which is what lets the view
//! skip rebuilding while neither the structure nor the choices moved.

use crate::application::{AppGroupKey, ApplicationGroups, FoldPreferences};
use crate::fxhash::{FxMap, PidMap};
use crate::procs::{NONE, ProcessEntry};

/// A group's fold, plus the sync pass that last saw the group alive (retain-by-mark: a group
/// whose mark is stale has vanished).
#[derive(Clone, Copy)]
struct GroupEntry {
    folded: bool,
    mark: u64,
}

#[derive(Default)]
pub(crate) struct Collapse {
    /// The user's explicit choices that differ from the built-in default, `true` = collapsed.
    /// Keeping the user's last choice (not a derived state) is what makes it stick: there is no
    /// cycle of states a repeated toggle can fall back through to the default.
    procs: PidMap<bool>,
    /// The kernel-thread forest root, collapsed unless the user chose otherwise.
    default_collapsed: Option<u32>,
    groups: FxMap<AppGroupKey, GroupEntry>,
    /// Counts sync passes; the mark of a live group.
    sync_pass: u64,
    version: u64,
}

impl Collapse {
    /// Bumped by every user mutation; unchanged ⇒ the choices are unchanged.
    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    /// Reconcile with the live processes and groups: drop state of vanished PIDs and groups,
    /// locate the kernel-thread forest's default collapse, and seed a new group's default
    /// (folded, unless the user persisted an expansion for its stable identity). Not a user
    /// mutation, so the version is untouched — callers run it as part of a rebuild.
    pub(crate) fn sync(
        &mut self,
        procs: &[ProcessEntry],
        groups: &ApplicationGroups,
        folds: &FoldPreferences,
    ) {
        // A PID absent from a gathered frame has lost its UI state. PID reuse within one gather
        // interval may inherit it, which is acceptable for display state and signal-safe.
        self.procs
            .retain(|pid, _| procs.binary_search_by(|p| p.pid.cmp(pid)).is_ok());
        self.default_collapsed = procs
            .iter()
            .find(|proc| proc.comm() == b"kthreadd" && proc.first_child != NONE)
            .map(|proc| proc.pid);

        // Mark the live groups (a hash lookup by borrowed key, a clone only for a new group),
        // then drop whatever was not marked — linear and allocation-free in steady state.
        self.sync_pass += 1;
        let pass = self.sync_pass;
        for group in groups.groups() {
            if let Some(entry) = self.groups.get_mut(&group.key) {
                entry.mark = pass;
                continue;
            }
            let expanded = group
                .persist_key
                .as_deref()
                .is_some_and(|key| folds.is_expanded(key));
            self.groups.insert(
                group.key.clone(),
                GroupEntry {
                    folded: !expanded,
                    mark: pass,
                },
            );
        }
        self.groups.retain(|_, entry| entry.mark == pass);
    }

    pub(crate) fn proc_collapsed(&self, pid: u32) -> bool {
        self.procs
            .get(&pid)
            .copied()
            .unwrap_or(self.default_collapsed == Some(pid))
    }

    pub(crate) fn group_folded(&self, key: &AppGroupKey) -> bool {
        self.groups.get(key).is_none_or(|entry| entry.folded)
    }

    /// Flip a process's native collapse.
    pub(crate) fn toggle_proc(&mut self, pid: u32) {
        self.set_proc_collapsed(pid, !self.proc_collapsed(pid));
    }

    /// Record the user's choice for a process. A choice equal to the default is stored as no
    /// entry, so the map holds only deviations.
    pub(crate) fn set_proc_collapsed(&mut self, pid: u32, collapsed: bool) {
        if collapsed == (self.default_collapsed == Some(pid)) {
            self.procs.remove(&pid);
        } else {
            self.procs.insert(pid, collapsed);
        }
        self.version += 1;
    }

    /// Fold (`true`) or expand (`false`) a group.
    pub(crate) fn set_group_folded(&mut self, key: &AppGroupKey, folded: bool) {
        let mark = self.sync_pass;
        self.groups.insert(key.clone(), GroupEntry { folded, mark });
        self.version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_explicit_choice_sticks_on_the_default_collapsed_process() {
        let mut kthreadd = ProcessEntry::TOMBSTONE;
        kthreadd.pid = 2;
        kthreadd.set_comm(b"kthreadd");
        kthreadd.first_child = 0;
        let procs = [kthreadd];
        let (groups, folds) = (ApplicationGroups::default(), FoldPreferences::disabled());
        let mut c = Collapse::default();
        c.sync(&procs, &groups, &folds);
        assert!(c.proc_collapsed(2), "kthreadd starts collapsed");
        c.toggle_proc(2);
        assert!(!c.proc_collapsed(2));
        c.toggle_proc(2);
        assert!(c.proc_collapsed(2));
        c.toggle_proc(2);
        c.sync(&procs, &groups, &folds);
        assert!(!c.proc_collapsed(2), "a sync must not re-apply the default");
        c.set_proc_collapsed(2, true);
        c.sync(&procs, &groups, &folds);
        assert!(c.proc_collapsed(2));
    }
}
