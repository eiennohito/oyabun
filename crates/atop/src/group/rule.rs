use super::{GroupCandidate, GroupFact, GroupMetaView, TreeView};

pub(crate) trait GroupRule {
    fn id(&self) -> &'static str;
    fn nominate(&self, tree: &TreeView<'_>, pid_idx: usize, out: &mut Vec<GroupCandidate>);
    fn resolve(&self, meta: &GroupMetaView<'_>, candidate: &GroupCandidate) -> Option<GroupFact>;
}
