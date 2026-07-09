use crate::group::{GroupEvidence, GroupFact, GroupLabel, GroupMetaView, GroupRule, TreeView};

/// Avoid collapsing tiny helper pairs. Real Chromium/Electron roots quickly have zygote, GPU,
/// utility, renderer, crashpad, or broker descendants; five descendants is a conservative v1
/// fan-out floor.
const MIN_CHROMIUM_DESCENDANTS: u32 = 5;
/// Require several immediate children with the same `comm` as the root before paying the
/// cmdline precision check. Chromium-style launchers commonly spawn same-binary children; one
/// or two same-name children is too common among generic worker pools.
const MIN_SHARED_COMM_CHILDREN: u32 = 3;

#[derive(Default)]
pub(crate) struct ChromiumRule {
    subtree_has_type: Vec<bool>,
}

impl GroupRule for ChromiumRule {
    fn prepare(&mut self, tree: &TreeView<'_>, meta: &GroupMetaView<'_>) {
        self.subtree_has_type.clear();
        self.subtree_has_type.resize(tree.procs().len(), false);
        for (idx, p) in tree.procs().iter().enumerate() {
            self.subtree_has_type[idx] = contains_arg_prefix(meta.cmdline(p), b"--type=");
        }
        for &idx in tree.preorder().iter().rev() {
            let p = tree.proc(idx as usize);
            if p.parent_idx != crate::procs::NONE && self.subtree_has_type[idx as usize] {
                self.subtree_has_type[p.parent_idx as usize] = true;
            }
        }
    }

    fn detect(
        &self,
        tree: &TreeView<'_>,
        meta: &GroupMetaView<'_>,
        pid_idx: usize,
    ) -> Option<GroupFact> {
        let root = tree.proc(pid_idx);
        if root.subtree_size < MIN_CHROMIUM_DESCENDANTS {
            return None;
        }

        let mut shared_comm_children = 0;
        for child_idx in tree.children(pid_idx) {
            let child = tree.proc(child_idx);
            if child.comm() == root.comm() {
                shared_comm_children += 1;
            }
        }
        if shared_comm_children < MIN_SHARED_COMM_CHILDREN {
            return None;
        }

        let has_type_descendant = tree
            .children(pid_idx)
            .any(|child_idx| self.subtree_has_type[child_idx]);
        if !has_type_descendant {
            return None;
        }

        let cmdline = meta.cmdline(root);
        let label = if cmdline.is_empty() {
            GroupLabel::new(root.comm(), root.non_ascii)
        } else {
            GroupLabel::new(cmdline, root.non_ascii)
        };
        Some(GroupFact::new(
            "chromium",
            label,
            80,
            GroupEvidence::ProcessControlled,
        ))
    }
}

fn contains_arg_prefix(cmdline: &[u8], prefix: &[u8]) -> bool {
    cmdline
        .split(|&b| b == b' ')
        .any(|arg| arg.starts_with(prefix))
}
