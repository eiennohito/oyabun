use crate::group::{GroupCandidate, GroupFact, GroupLabel, GroupMetaView, GroupRule, TreeView};

/// Avoid collapsing tiny helper pairs. Real Chromium/Electron roots quickly have zygote, GPU,
/// utility, renderer, crashpad, or broker descendants; five descendants is a conservative v1
/// fan-out floor.
const MIN_CHROMIUM_DESCENDANTS: u32 = 5;
/// Require several immediate children with the same `comm` as the root before paying the
/// cmdline precision check. Chromium-style launchers commonly spawn same-binary children; one
/// or two same-name children is too common among generic worker pools.
const MIN_SHARED_COMM_CHILDREN: u32 = 3;

pub(crate) struct ChromiumRule;

impl GroupRule for ChromiumRule {
    fn id(&self) -> &'static str {
        "chromium"
    }

    fn nominate(&self, tree: &TreeView<'_>, pid_idx: usize, out: &mut Vec<GroupCandidate>) {
        let root = tree.proc(pid_idx);
        if root.subtree_size < MIN_CHROMIUM_DESCENDANTS {
            return;
        }

        let mut shared_comm_children = 0;
        for child in tree.children(pid_idx) {
            if child.comm() == root.comm() {
                shared_comm_children += 1;
            }
        }
        if shared_comm_children >= MIN_SHARED_COMM_CHILDREN {
            out.push(GroupCandidate::new(pid_idx, self.id()));
        }
    }

    fn resolve(&self, meta: &GroupMetaView<'_>, candidate: &GroupCandidate) -> Option<GroupFact> {
        let root = meta.proc(candidate.pid_idx());
        let has_chromium_type = meta
            .descendants(candidate.pid_idx())
            .any(|child| contains_arg_prefix(meta.cmdline(child), b"--type="));
        if !has_chromium_type {
            return None;
        }

        let cmdline = meta.cmdline(root);
        let label = if cmdline.is_empty() {
            GroupLabel::new(root.comm(), root.non_ascii)
        } else {
            GroupLabel::new(cmdline, root.non_ascii)
        };
        Some(GroupFact::new(self.id(), label))
    }
}

fn contains_arg_prefix(cmdline: &[u8], prefix: &[u8]) -> bool {
    cmdline
        .split(|&b| b == b' ')
        .any(|arg| arg.starts_with(prefix))
}
