//! Index-based process tree construction.
//!
//! Builds an intrusive child/sibling forest over `procs` (sorted by PID) with no
//! per-cycle `HashMap` or per-node `Vec`: parent lookup is a binary search, child
//! lists are prepend-linked, and depth/subtree sizes come from one pre-order pass
//! plus its reverse. Scratch buffers are caller-owned and reused.

use crate::procs::{NONE, ProcessEntry};

/// Build the structural tree. `procs` MUST be sorted ascending by `pid`.
///
/// Sets `parent_idx`, `first_child`, `next_sibling`, `subtree_size`, and `depth` on
/// every entry; returns the head index of the root sibling chain (or [`NONE`]).
/// `stack` and `order` are reused scratch (cleared on entry).
// Indices are `< procs.len()`, which fits `u32` by the row-buffer design.
#[allow(clippy::cast_possible_truncation)]
pub fn build(procs: &mut [ProcessEntry], stack: &mut Vec<u32>, order: &mut Vec<u32>) -> u32 {
    let n = procs.len();
    for p in procs.iter_mut() {
        p.parent_idx = NONE;
        p.first_child = NONE;
        p.next_sibling = NONE;
        p.subtree_size = 0;
        p.depth = 0;
    }

    let mut first_root = NONE;

    // Reverse index order == reverse PID order; prepending then yields ascending
    // sibling/root chains.
    for i in (0..n).rev() {
        let ppid = procs[i].ppid;
        let parent = if ppid == 0 {
            None
        } else {
            procs.binary_search_by(|p| p.pid.cmp(&ppid)).ok()
        };
        let iu = i as u32;
        match parent {
            Some(p) if p != i => {
                procs[i].parent_idx = p as u32;
                procs[i].next_sibling = procs[p].first_child;
                procs[p].first_child = iu;
            }
            _ => {
                procs[i].next_sibling = first_root;
                first_root = iu;
            }
        }
    }

    // Pre-order DFS: assign depth (parent always visited first), record order.
    stack.clear();
    order.clear();
    push_chain(procs, first_root, stack);
    while let Some(iu) = stack.pop() {
        let i = iu as usize;
        let depth = match procs[i].parent_idx {
            NONE => 0,
            p => procs[p as usize].depth.saturating_add(1),
        };
        procs[i].depth = depth;
        order.push(iu);
        push_chain(procs, procs[i].first_child, stack);
    }

    // Reverse pre-order: every node precedes its ancestors, so subtree sizes are
    // final before being folded into the parent.
    for &iu in order.iter().rev() {
        let i = iu as usize;
        if procs[i].parent_idx != NONE {
            let sz = procs[i].subtree_size.saturating_add(1);
            let p = procs[i].parent_idx as usize;
            procs[p].subtree_size = procs[p].subtree_size.saturating_add(sz);
        }
    }

    first_root
}

/// Compute inclusive subtree aggregates (`subtree_cpu`, `subtree_mem`) via the same
/// reverse-pre-order pass. Must be called after `build` and after CPU% /
/// `mem_bytes` are finalized. `order` is the pre-order from `build`.
pub fn aggregate(procs: &mut [ProcessEntry], order: &[u32]) {
    // Initialize to self, then accumulate children bottom-up.
    for &iu in order {
        let i = iu as usize;
        procs[i].subtree_cpu = procs[i].cpu_pct;
        procs[i].subtree_mem = procs[i].mem_bytes;
    }
    for &iu in order.iter().rev() {
        let i = iu as usize;
        if procs[i].parent_idx != NONE {
            let p = procs[i].parent_idx as usize;
            procs[p].subtree_cpu = procs[p].subtree_cpu.saturating_add(procs[i].subtree_cpu);
            procs[p].subtree_mem = procs[p].subtree_mem.saturating_add(procs[i].subtree_mem);
        }
    }
}

/// Push a sibling chain (head + its `next_sibling`s) onto the DFS stack.
fn push_chain(procs: &[ProcessEntry], head: u32, stack: &mut Vec<u32>) {
    let mut c = head;
    while c != NONE {
        stack.push(c);
        c = procs[c as usize].next_sibling;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent: u32) -> ProcessEntry {
        ProcessEntry {
            pid,
            ppid: parent,
            uid: 0,
            state: b'S',
            priority: 20,
            nice: 0,
            comm_len: 0,
            num_threads: 1,
            cpu_pct: 0,
            cpu_peak: 0,
            mem_bytes: 0,
            ticks: 0,
            start_time: 0,
            comm_bytes: [0; crate::procs::COMM_CAP],
            cmdline: thoop::StringRef::EMPTY,
            non_ascii: false,
            is_kthread: false,
            parent_idx: NONE,
            first_child: NONE,
            next_sibling: NONE,
            subtree_size: 0,
            depth: 0,
            subtree_cpu: 0,
            subtree_mem: 0,
        }
    }

    fn build_v(mut procs: Vec<ProcessEntry>) -> (Vec<ProcessEntry>, u32) {
        let (mut s, mut o) = (Vec::new(), Vec::new());
        let root = build(&mut procs, &mut s, &mut o);
        (procs, root)
    }

    /// Collect display order (depth) by walking the built tree, for assertions.
    fn flat(procs: &[ProcessEntry], first_root: u32) -> Vec<(u32, u16)> {
        let mut out = Vec::new();
        let mut stack = Vec::new();
        // Push roots reversed so we pop ascending.
        let mut chain = Vec::new();
        let mut c = first_root;
        while c != NONE {
            chain.push(c);
            c = procs[c as usize].next_sibling;
        }
        for &r in chain.iter().rev() {
            stack.push(r);
        }
        while let Some(iu) = stack.pop() {
            let i = iu as usize;
            out.push((procs[i].pid, procs[i].depth));
            let mut kids = Vec::new();
            let mut k = procs[i].first_child;
            while k != NONE {
                kids.push(k);
                k = procs[k as usize].next_sibling;
            }
            for &kid in kids.iter().rev() {
                stack.push(kid);
            }
        }
        out
    }

    #[test]
    fn flat_roots() {
        let (procs, root) = build_v(vec![proc(1, 0), proc(2, 0)]);
        assert_eq!(flat(&procs, root), vec![(1, 0), (2, 0)]);
    }

    #[test]
    fn parent_child_depth_and_order() {
        let (procs, root) = build_v(vec![proc(1, 0), proc(10, 1), proc(100, 10)]);
        assert_eq!(flat(&procs, root), vec![(1, 0), (10, 1), (100, 2)]);
    }

    #[test]
    fn subtree_sizes() {
        // 1 → {10 → {100}, 11}
        let (procs, _root) = build_v(vec![proc(1, 0), proc(10, 1), proc(11, 1), proc(100, 10)]);
        let by_pid = |pid: u32| procs.iter().find(|p| p.pid == pid).unwrap();
        assert_eq!(by_pid(1).subtree_size, 3);
        assert_eq!(by_pid(10).subtree_size, 1);
        assert_eq!(by_pid(11).subtree_size, 0);
        assert_eq!(by_pid(100).subtree_size, 0);
    }

    #[test]
    fn siblings_pid_ascending() {
        // children of 1 should appear in pid order: 5, 9, 20
        let (procs, root) = build_v(vec![proc(1, 0), proc(20, 1), proc(5, 1), proc(9, 1)]);
        // procs must be pid-sorted for build; sort first like the gatherer does.
        let mut v = procs;
        v.sort_by_key(|p| p.pid);
        let (v, root2) = build_v(v);
        let _ = root;
        let order: Vec<u32> = flat(&v, root2).iter().map(|&(pid, _)| pid).collect();
        assert_eq!(order, vec![1, 5, 9, 20]);
    }

    #[test]
    fn orphan_becomes_root() {
        let (procs, root) = build_v(vec![proc(1, 0), proc(50, 999)]);
        let order: Vec<u32> = flat(&procs, root).iter().map(|&(pid, _)| pid).collect();
        assert_eq!(order, vec![1, 50]);
        let by_pid = |pid: u32| procs.iter().find(|p| p.pid == pid).unwrap();
        assert_eq!(by_pid(50).parent_idx, NONE);
    }

    #[test]
    fn aggregate_sums_subtree() {
        // 1 → {10 → {100}, 11}
        // Set cpu/mem values on each node.
        let mut procs = vec![proc(1, 0), proc(10, 1), proc(11, 1), proc(100, 10)];
        procs[0].cpu_pct = 100;
        procs[0].mem_bytes = 1000;
        procs[1].cpu_pct = 200;
        procs[1].mem_bytes = 2000;
        procs[2].cpu_pct = 300;
        procs[2].mem_bytes = 3000;
        procs[3].cpu_pct = 400;
        procs[3].mem_bytes = 4000;

        let (mut s, mut o) = (Vec::new(), Vec::new());
        build(&mut procs, &mut s, &mut o);
        aggregate(&mut procs, &o);

        let by_pid = |pid: u32| procs.iter().find(|p| p.pid == pid).unwrap();
        // Root: 100 + 200 + 300 + 400 = 1000
        assert_eq!(by_pid(1).subtree_cpu, 1000);
        assert_eq!(by_pid(1).subtree_mem, 10000);
        // PID 10: 200 + 400 = 600
        assert_eq!(by_pid(10).subtree_cpu, 600);
        assert_eq!(by_pid(10).subtree_mem, 6000);
        // Leaves: just themselves.
        assert_eq!(by_pid(11).subtree_cpu, 300);
        assert_eq!(by_pid(100).subtree_cpu, 400);
    }
}
