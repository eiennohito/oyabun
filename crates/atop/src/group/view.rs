use crate::group::GroupMetaSource;
use crate::procs::{NONE, ProcessEntry};

pub(crate) struct TreeView<'a> {
    procs: &'a [ProcessEntry],
    preorder: &'a [u32],
    preorder_pos: &'a [usize],
}

impl<'a> TreeView<'a> {
    pub(crate) fn new(
        procs: &'a [ProcessEntry],
        preorder: &'a [u32],
        preorder_pos: &'a [usize],
    ) -> Self {
        Self {
            procs,
            preorder,
            preorder_pos,
        }
    }

    #[must_use]
    pub(crate) fn proc(&self, pid_idx: usize) -> &'a ProcessEntry {
        &self.procs[pid_idx]
    }

    pub(crate) fn children(&self, pid_idx: usize) -> ChildIter<'a> {
        ChildIter {
            procs: self.procs,
            next: self.procs[pid_idx].first_child,
        }
    }

    #[must_use]
    pub(crate) fn procs(&self) -> &'a [ProcessEntry] {
        self.procs
    }

    #[must_use]
    pub(crate) fn preorder(&self) -> &'a [u32] {
        self.preorder
    }

    #[must_use]
    pub(crate) fn preorder_pos(&self, pid_idx: usize) -> usize {
        self.preorder_pos[pid_idx]
    }
}

pub(crate) struct GroupMetaView<'a> {
    meta: &'a dyn GroupMetaSource,
}

impl<'a> GroupMetaView<'a> {
    pub(crate) fn new(meta: &'a dyn GroupMetaSource) -> Self {
        Self { meta }
    }

    #[must_use]
    pub(crate) fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.meta.cmdline(e)
    }

    #[must_use]
    pub(crate) fn cgroup(&self, e: &ProcessEntry) -> &[u8] {
        self.meta.cgroup(e)
    }

    #[must_use]
    pub(crate) fn flatpak_info(&self, e: &ProcessEntry) -> &[u8] {
        self.meta.flatpak_info(e)
    }
}

pub(crate) struct ChildIter<'a> {
    procs: &'a [ProcessEntry],
    next: u32,
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == NONE {
            return None;
        }
        let idx = self.next as usize;
        let p = &self.procs[idx];
        self.next = p.next_sibling;
        Some(idx)
    }
}
