use crate::group::GroupMetaSource;
use crate::procs::{NONE, ProcessEntry};

pub(crate) struct TreeView<'a> {
    procs: &'a [ProcessEntry],
}

impl<'a> TreeView<'a> {
    pub(crate) fn new(procs: &'a [ProcessEntry]) -> Self {
        Self { procs }
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
}

pub(crate) struct GroupMetaView<'a> {
    procs: &'a [ProcessEntry],
    meta: &'a dyn GroupMetaSource,
}

impl<'a> GroupMetaView<'a> {
    pub(crate) fn new(procs: &'a [ProcessEntry], meta: &'a dyn GroupMetaSource) -> Self {
        Self { procs, meta }
    }

    #[must_use]
    pub(crate) fn proc(&self, pid_idx: usize) -> &'a ProcessEntry {
        &self.procs[pid_idx]
    }

    #[must_use]
    pub(crate) fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
        self.meta.cmdline(e)
    }

    pub(crate) fn descendants(&self, pid_idx: usize) -> DescendantIter<'a> {
        DescendantIter::new(self.procs, self.procs[pid_idx].first_child)
    }
}

pub(crate) struct ChildIter<'a> {
    procs: &'a [ProcessEntry],
    next: u32,
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = &'a ProcessEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == NONE {
            return None;
        }
        let idx = self.next as usize;
        let p = &self.procs[idx];
        self.next = p.next_sibling;
        Some(p)
    }
}

pub(crate) struct DescendantIter<'a> {
    procs: &'a [ProcessEntry],
    stack: Vec<u32>,
}

impl<'a> DescendantIter<'a> {
    fn new(procs: &'a [ProcessEntry], first_child: u32) -> Self {
        let mut iter = Self {
            procs,
            stack: Vec::new(),
        };
        iter.push_siblings(first_child);
        iter
    }

    fn push_siblings(&mut self, head: u32) {
        let base = self.stack.len();
        let mut c = head;
        while c != NONE {
            self.stack.push(c);
            c = self.procs[c as usize].next_sibling;
        }
        self.stack[base..].reverse();
    }
}

impl<'a> Iterator for DescendantIter<'a> {
    type Item = &'a ProcessEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.stack.pop()? as usize;
        let p = &self.procs[idx];
        self.push_siblings(p.first_child);
        Some(p)
    }
}
