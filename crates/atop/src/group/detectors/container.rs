use crate::group::{GroupEvidence, GroupFact, GroupLabel, GroupMetaView, GroupRule, TreeView};

pub(crate) struct ContainerShimRule;

impl GroupRule for ContainerShimRule {
    fn prepare(&mut self, _tree: &TreeView<'_>, _meta: &GroupMetaView<'_>) {}

    fn detect(
        &self,
        tree: &TreeView<'_>,
        meta: &GroupMetaView<'_>,
        pid_idx: usize,
    ) -> Option<GroupFact> {
        let root = tree.proc(pid_idx);
        if root.subtree_size == 0 || !is_container_shim_comm(root.comm()) {
            return None;
        }
        let cmdline = meta.cmdline(root);
        if cmdline.is_empty()
            || !(has_arg(cmdline, b"-id")
                || has_arg(cmdline, b"--id")
                || has_arg(cmdline, b"-container-id")
                || has_arg(cmdline, b"--container-id"))
        {
            return None;
        }
        Some(GroupFact::new(
            "container-shim",
            GroupLabel::new(cmdline, root.non_ascii),
            40,
            GroupEvidence::ProcessControlled,
        ))
    }
}

fn is_container_shim_comm(comm: &[u8]) -> bool {
    matches!(comm, b"containerd-shim" | b"conmon" | b"docker-init")
}

fn has_arg(cmdline: &[u8], needle: &[u8]) -> bool {
    cmdline.split(|&b| b == b' ').any(|arg| arg == needle)
}
