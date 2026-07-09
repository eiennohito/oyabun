use crate::group::{GroupEvidence, GroupFact, GroupLabel, GroupMetaView, GroupRule, TreeView};

pub(crate) struct FlatpakRule;

impl GroupRule for FlatpakRule {
    fn prepare(&mut self, _tree: &TreeView<'_>, _meta: &GroupMetaView<'_>) {}

    fn detect(
        &self,
        tree: &TreeView<'_>,
        meta: &GroupMetaView<'_>,
        pid_idx: usize,
    ) -> Option<GroupFact> {
        let root = tree.proc(pid_idx);
        if root.subtree_size == 0 {
            return None;
        }
        let cgroup = meta.cgroup(root);
        if cgroup.is_empty()
            || !(contains(cgroup, b"flatpak")
                || root.comm() == b"bwrap"
                || contains(meta.cmdline(root), b"flatpak"))
        {
            return None;
        }
        let info = meta.flatpak_info(root);
        let app = flatpak_value(info, b"name").or_else(|| flatpak_value(info, b"app"))?;
        let branch = flatpak_value(info, b"branch");
        let runtime = flatpak_value(info, b"runtime");
        let label = match (branch, runtime) {
            (Some(branch), _) if !branch.is_empty() => joined(app, b"/", branch),
            (_, Some(runtime)) if !runtime.is_empty() => joined(app, b" ", runtime),
            _ => app.to_vec(),
        };
        Some(GroupFact::new(
            "flatpak",
            GroupLabel::new(&label, root.non_ascii),
            100,
            GroupEvidence::ProcessControlled,
        ))
    }
}

fn flatpak_value<'a>(info: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    info.split(|&b| b == b'\n').find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let idx = line.iter().position(|&b| b == b'=')?;
        let (candidate, value) = (&line[..idx], &line[idx + 1..]);
        candidate.eq_ignore_ascii_case(key).then_some(value)
    })
}

fn joined(a: &[u8], sep: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len() + sep.len() + b.len());
    out.extend_from_slice(a);
    out.extend_from_slice(sep);
    out.extend_from_slice(b);
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
