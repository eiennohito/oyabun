use super::{GroupFact, GroupMetaView, TreeView};

pub(crate) trait GroupRule {
    /// Build rule-specific indexes once per cycle. Implementations must be O(processes).
    fn prepare(&mut self, tree: &TreeView<'_>, meta: &GroupMetaView<'_>);

    /// Detect one process from prepared indexes without walking its subtree.
    fn detect(
        &self,
        tree: &TreeView<'_>,
        meta: &GroupMetaView<'_>,
        pid_idx: usize,
    ) -> Option<GroupFact>;
}
