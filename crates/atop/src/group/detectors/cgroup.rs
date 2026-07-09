use crate::group::{GroupEvidence, GroupFact, GroupLabel, GroupMetaView, GroupRule, TreeView};

#[derive(Default)]
pub(crate) struct CgroupRule {
    identities: Vec<Option<CgroupIdentity>>,
    coherent: Vec<bool>,
    subtree_same: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CgroupIdentity {
    Pod(Vec<u8>),
    Container(Vec<u8>),
    Systemd(Vec<u8>),
}

impl GroupRule for CgroupRule {
    fn prepare(&mut self, tree: &TreeView<'_>, meta: &GroupMetaView<'_>) {
        self.identities.clear();
        self.identities.extend(
            tree.procs()
                .iter()
                .map(|proc| parse_identity(meta.cgroup(proc))),
        );
        self.coherent.clear();
        self.coherent.resize(tree.procs().len(), false);
        self.subtree_same.clear();
        self.subtree_same.resize(tree.procs().len(), false);

        for &idx in tree.preorder().iter().rev() {
            let idx = idx as usize;
            let Some(identity) = self.identities[idx].as_ref() else {
                continue;
            };
            let mut has_child = false;
            let mut same = true;
            let mut child = tree.proc(idx).first_child;
            while child != crate::procs::NONE {
                has_child = true;
                let child_idx = child as usize;
                same &= self.identities[child_idx].as_ref() == Some(identity)
                    && self.subtree_same[child_idx];
                child = tree.proc(child_idx).next_sibling;
            }
            self.subtree_same[idx] = same;
            self.coherent[idx] = has_child && same;
        }
    }

    fn detect(
        &self,
        tree: &TreeView<'_>,
        _meta: &GroupMetaView<'_>,
        pid_idx: usize,
    ) -> Option<GroupFact> {
        if !self.coherent[pid_idx] {
            return None;
        }
        let root = tree.proc(pid_idx);
        let label = match self.identities[pid_idx].as_ref()? {
            CgroupIdentity::Pod(uid) => joined(b"pod/", b"", short_id(&uid)),
            CgroupIdentity::Container(id) => joined(b"container/", b"", short_id(&id)),
            CgroupIdentity::Systemd(unit) => {
                return Some(GroupFact::new(
                    "cgroup-systemd",
                    GroupLabel::new(unit, root.non_ascii),
                    50,
                    GroupEvidence::KernelCgroup,
                ));
            }
        };
        let rule_id = match self.identities[pid_idx].as_ref()? {
            CgroupIdentity::Pod(_) | CgroupIdentity::Container(_) => "cgroup-container",
            CgroupIdentity::Systemd(_) => unreachable!(),
        };
        Some(GroupFact::new(
            rule_id,
            GroupLabel::new(&label, root.non_ascii),
            90,
            GroupEvidence::KernelCgroup,
        ))
    }
}

fn parse_identity(raw: &[u8]) -> Option<CgroupIdentity> {
    let path = selected_cgroup_path(raw)?;
    pod_uid(path)
        .map(CgroupIdentity::Pod)
        .or_else(|| container_id(path).map(CgroupIdentity::Container))
        .or_else(|| systemd_unit(path).map(CgroupIdentity::Systemd))
}

pub(crate) fn selected_cgroup_path(raw: &[u8]) -> Option<&[u8]> {
    let mut fallback = None;
    for line in raw.split(|&b| b == b'\n') {
        let line = trim_cr(line);
        if line.is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix(b"0::") {
            return useful_path(path);
        }
        let Some(first) = byte_pos(line, b':') else {
            continue;
        };
        let Some(second_rel) = byte_pos(&line[first + 1..], b':') else {
            continue;
        };
        let path = &line[first + 1 + second_rel + 1..];
        if let Some(path) = useful_path(path)
            && fallback.is_none_or(|old: &[u8]| path.len() > old.len())
        {
            fallback = Some(path);
        }
    }
    fallback
}

fn useful_path(path: &[u8]) -> Option<&[u8]> {
    (!path.is_empty() && path != b"/").then_some(path)
}

fn systemd_unit(path: &[u8]) -> Option<Vec<u8>> {
    path.split(|&b| b == b'/')
        .rev()
        .find(|part| useful_systemd_unit(part))
        .map(Vec::from)
}

fn useful_systemd_unit(unit: &[u8]) -> bool {
    if unit.ends_with(b".scope") {
        return unit.starts_with(b"app-");
    }
    unit.ends_with(b".service") && !unit.starts_with(b"user@")
}

fn pod_uid(path: &[u8]) -> Option<Vec<u8>> {
    path.split(|&b| b == b'/').find_map(|part| {
        let tail = if let Some(rest) = part.strip_prefix(b"pod") {
            rest
        } else {
            let idx = rfind_bytes(part, b"-pod")?;
            &part[idx + 4..]
        };
        let tail = tail
            .strip_prefix(b"-")
            .or_else(|| tail.strip_prefix(b"_"))
            .unwrap_or(tail);
        let end = tail
            .iter()
            .position(|&b| !(b.is_ascii_hexdigit() || b == b'_' || b == b'-'))
            .unwrap_or(tail.len());
        let uid = tail[..end].to_vec();
        (uid.iter().filter(|&&b| b.is_ascii_hexdigit()).count() >= 12).then_some(uid)
    })
}

fn container_id(path: &[u8]) -> Option<Vec<u8>> {
    let mut previous_runtime_dir = false;
    for part in path.split(|&b| b == b'/') {
        let part = strip_systemd_scope(part);
        if previous_runtime_dir {
            let id = hex_prefix(part);
            if id.len() >= 12 {
                return Some(id.to_vec());
            }
        }
        previous_runtime_dir = matches!(part, b"docker" | b"containerd" | b"crio" | b"libpod");
        for prefix in [
            b"docker-".as_slice(),
            b"containerd-",
            b"cri-containerd-",
            b"crio-",
            b"libpod-",
        ] {
            if let Some(rest) = part.strip_prefix(prefix) {
                let id = hex_prefix(rest);
                if id.len() >= 12 {
                    return Some(id.to_vec());
                }
            }
        }
        let id = hex_prefix(part);
        if id.len() >= 32 {
            return Some(id.to_vec());
        }
    }
    None
}

fn strip_systemd_scope(part: &[u8]) -> &[u8] {
    part.strip_suffix(b".scope").unwrap_or(part)
}

fn hex_prefix(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|b| !b.is_ascii_hexdigit())
        .unwrap_or(bytes.len());
    &bytes[..end]
}

fn joined(a: &[u8], sep: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len() + sep.len() + b.len());
    out.extend_from_slice(a);
    out.extend_from_slice(sep);
    out.extend_from_slice(b);
    out
}

fn short_id(id: &[u8]) -> &[u8] {
    &id[..id.len().min(12)]
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn byte_pos(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().position(|&b| b == needle)
}

fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_user_app_scope() {
        let raw = b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.mozilla.firefox-123.scope\n";
        assert_eq!(
            selected_cgroup_path(raw),
            Some(b"/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.mozilla.firefox-123.scope".as_slice())
        );
        assert_eq!(
            parse_identity(raw),
            Some(CgroupIdentity::Systemd(
                b"app-org.mozilla.firefox-123.scope".to_vec()
            ))
        );
    }

    #[test]
    fn parses_system_service() {
        let raw = b"0::/system.slice/sshd.service\n";
        assert_eq!(
            parse_identity(raw),
            Some(CgroupIdentity::Systemd(b"sshd.service".to_vec()))
        );
    }

    #[test]
    fn skips_broad_and_transient_systemd_units() {
        assert_eq!(
            parse_identity(b"0::/user.slice/user-1000.slice/user@1000.service/init.scope\n"),
            None
        );
        assert_eq!(
            parse_identity(b"0::/user.slice/user-1000.slice/session-2.scope\n"),
            None
        );
        assert_eq!(
            parse_identity(
                b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/kitty-2278-3.scope\n"
            ),
            None
        );
    }

    #[test]
    fn parses_docker_containerd_and_libpod_scopes() {
        assert_eq!(
            parse_identity(b"0::/system.slice/docker-abcdef1234567890.scope\n"),
            Some(CgroupIdentity::Container(b"abcdef1234567890".to_vec()))
        );
        assert_eq!(
            parse_identity(b"0::/system.slice/cri-containerd-fedcba9876543210.scope\n"),
            Some(CgroupIdentity::Container(b"fedcba9876543210".to_vec()))
        );
        assert_eq!(
            parse_identity(b"0::/machine.slice/libpod-1111222233334444.scope\n"),
            Some(CgroupIdentity::Container(b"1111222233334444".to_vec()))
        );
    }

    #[test]
    fn parses_kubernetes_pod_path() {
        let raw = b"0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod12345678_90ab_cdef_1234_567890abcdef.slice/cri-containerd-aaaaaaaaaaaa.scope\n";
        assert_eq!(
            parse_identity(raw),
            Some(CgroupIdentity::Pod(
                b"12345678_90ab_cdef_1234_567890abcdef".to_vec()
            ))
        );
    }

    #[test]
    fn parses_v1_multi_controller_input() {
        let raw = b"12:cpu,cpuacct:/docker/abcdef1234567890\n11:memory:/\n";
        assert_eq!(
            selected_cgroup_path(raw),
            Some(b"/docker/abcdef1234567890".as_slice())
        );
        assert_eq!(
            parse_identity(raw),
            Some(CgroupIdentity::Container(b"abcdef1234567890".to_vec()))
        );
    }
}
