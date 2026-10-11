use crate::application::DesktopResolver;
use crate::fxhash::FxMap;
use crate::identity::{IdentityKind, ProcessIdentity};
use crate::procs::{NONE, ProcessEntry};

/// Minimum members before a group is worth folding: a two-process group saves one row at the cost
/// of hiding one, which is not worth it.
const MIN_MEMBERS: usize = 3;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct AppGroupKey {
    pub(crate) owner_uid: u32,
    pub(super) canonical: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AppGroup {
    pub(crate) key: AppGroupKey,
    pub(crate) representative: usize,
    pub(crate) label: String,
    pub(crate) owner_uid: u32,
    /// Cross-run-stable key for persisting a fold preference, or `None` when the identity is
    /// ephemeral (container/pod ids, structural tokens) and folds stay session-only.
    pub(crate) persist_key: Option<String>,
    /// The top member whose subtree contains the representative: where the group's row sits.
    pub(crate) anchor_top: usize,
}

/// A process's place in a group: which one, and whether it is a top member of it.
#[derive(Clone, Copy)]
struct Membership {
    group: usize,
    top: bool,
}

#[derive(Default)]
pub(crate) struct ApplicationGroups {
    groups: Vec<AppGroup>,
    membership: Vec<Option<Membership>>,
}

struct Candidate {
    proc_idx: usize,
    label: String,
    terminal: bool,
    persist_key: Option<String>,
    launcher_scope: bool,
    instantiated: bool,
    owner_uid: u32,
}

impl ApplicationGroups {
    /// Regroup: bucket processes by identity into groups (membership, representative, label,
    /// keys, top members). Membership, labels, keys, and top members are a pure function of the
    /// per-process identities and the tree shape, so the caller runs this only when the
    /// metadata epoch moves (an unchanged epoch also means an unchanged pid set and parent
    /// links, hence unchanged proc indices, since the process buffer is pid-sorted). Per-cycle
    /// numbers are not kept here: the view sums them over whatever each row hides.
    pub(crate) fn rebuild<'a>(
        &mut self,
        procs: &[ProcessEntry],
        identities: impl IntoIterator<Item = (usize, &'a ProcessIdentity)>,
        resolver: &mut DesktopResolver,
    ) {
        self.regroup(procs, identities, resolver);
    }

    fn regroup<'a>(
        &mut self,
        procs: &[ProcessEntry],
        identities: impl IntoIterator<Item = (usize, &'a ProcessIdentity)>,
        resolver: &mut DesktopResolver,
    ) {
        let mut buckets: FxMap<AppGroupKey, Vec<Candidate>> = FxMap::default();
        for (proc_idx, identity) in identities {
            let (key, label, terminal) = classify(identity, resolver, &procs[proc_idx]);
            let persist_key = persist_key(identity, &key);
            let (launcher_scope, instantiated) = match &identity.kind {
                IdentityKind::DesktopApp(app) => (app.launcher_scope, app.instantiated),
                _ => (false, false),
            };
            buckets.entry(key).or_default().push(Candidate {
                proc_idx,
                label,
                terminal,
                persist_key,
                launcher_scope,
                instantiated,
                owner_uid: identity.owner_uid,
            });
        }

        self.groups.clear();
        self.membership.clear();
        self.membership.resize(procs.len(), None);
        for (key, mut members) in buckets {
            // Tiny groups aren't worth a row; a terminal member (each shell/tab is its own
            // workspace) means folding would hide what the user is actively doing.
            if members.len() < MIN_MEMBERS || members.iter().any(|member| member.terminal) {
                continue;
            }
            // Order members topmost-then-lowest-PID: deterministic and independent of the
            // gatherer preorder's sibling direction, and stable as the specific processes churn.
            members
                .sort_by_key(|member| (procs[member.proc_idx].depth, procs[member.proc_idx].pid));
            let representative = members
                .iter()
                .min_by_key(|member| {
                    (
                        // Prefer a launcher `.scope` owned by the session over an instantiated
                        // `.service`, so a split desktop app's row is its launcher.
                        !(member.launcher_scope
                            && !member.instantiated
                            && procs[member.proc_idx].uid == member.owner_uid),
                        member.instantiated,
                        procs[member.proc_idx].depth,
                        procs[member.proc_idx].pid,
                    )
                })
                .expect("non-empty group has a representative");
            let label = representative.label.clone();
            let persist_key = representative.persist_key.clone();
            let owner_uid = representative.owner_uid;
            let representative = representative.proc_idx;

            let member_indices: Vec<usize> = members.iter().map(|m| m.proc_idx).collect();
            let group = self.groups.len();
            for &idx in &member_indices {
                self.membership[idx] = Some(Membership { group, top: true });
            }
            // A member with a member ancestor is not a top. The walk passes through foreign
            // intermediates, not just the direct parent, or a nested member would double count.
            for &idx in &member_indices {
                if self.member_ancestor(procs, idx, group).is_some() {
                    self.membership[idx] = Some(Membership { group, top: false });
                }
            }
            let anchor_top = self
                .topmost_member_ancestor(procs, representative, group)
                .unwrap_or(representative);
            self.groups.push(AppGroup {
                key,
                representative,
                label,
                owner_uid,
                persist_key,
                anchor_top,
            });
        }
    }

    /// The nearest proper ancestor of `idx` that belongs to `group`, if any. Bounded by the
    /// process count so a (corrupt) parent cycle cannot loop.
    fn member_ancestor(&self, procs: &[ProcessEntry], idx: usize, group: usize) -> Option<usize> {
        let mut ancestor = procs[idx].parent_idx;
        for _ in 0..procs.len() {
            if ancestor == NONE {
                return None;
            }
            let a = ancestor as usize;
            if self.membership[a].is_some_and(|m| m.group == group) {
                return Some(a);
            }
            ancestor = procs[a].parent_idx;
        }
        None
    }

    /// The topmost member of `group` among `idx` and its ancestors.
    fn topmost_member_ancestor(
        &self,
        procs: &[ProcessEntry],
        idx: usize,
        group: usize,
    ) -> Option<usize> {
        let mut top = self.membership[idx]
            .is_some_and(|m| m.group == group)
            .then_some(idx);
        let mut ancestor = procs[idx].parent_idx;
        for _ in 0..procs.len() {
            if ancestor == NONE {
                break;
            }
            let a = ancestor as usize;
            if self.membership[a].is_some_and(|m| m.group == group) {
                top = Some(a);
            }
            ancestor = procs[a].parent_idx;
        }
        top
    }

    pub(crate) fn groups(&self) -> &[AppGroup] {
        &self.groups
    }

    /// The group `proc_idx` belongs to, if any.
    pub(crate) fn member_group(&self, proc_idx: usize) -> Option<usize> {
        self.membership
            .get(proc_idx)
            .copied()
            .flatten()
            .map(|m| m.group)
    }

    /// The group `proc_idx` is a *top* member of (no member ancestor), if any.
    pub(crate) fn top_group(&self, proc_idx: usize) -> Option<usize> {
        self.membership
            .get(proc_idx)
            .copied()
            .flatten()
            .filter(|m| m.top)
            .map(|m| m.group)
    }
}

/// Resolve a process's identity into a bucket key, display label, and terminal flag. Desktop
/// applications resolve their label (and a split-scope-merging canonical key) from the desktop
/// entry; every other kind carries its own label and buckets on its identity token verbatim.
fn classify(
    identity: &ProcessIdentity,
    resolver: &mut DesktopResolver,
    proc: &ProcessEntry,
) -> (AppGroupKey, String, bool) {
    let owner_uid = identity.owner_uid;
    let IdentityKind::DesktopApp(app) = &identity.kind else {
        // Every non-desktop kind carries its own label and buckets on its identity token verbatim.
        let token = identity.kind.bucket_token();
        let label = identity
            .kind
            .label()
            .map_or_else(|| fallback_label(token.as_ref(), proc), str::to_owned);
        let key = AppGroupKey {
            owner_uid,
            canonical: token.into_owned(),
        };
        return (key, label, false);
    };
    let entry = resolver.resolve(owner_uid, &app.app_id);
    let canonical = if let Some(launch) = entry
        .filter(|_| !app.flatpak)
        .and_then(|e| e.launch.as_deref())
    {
        // Merge a split launcher/service scope onto its shared name+launch signature.
        let name = entry.map_or(app.app_id.as_str(), |e| e.name.as_str());
        format!("desktop\0{name}\0{launch}")
    } else {
        app.app_id.clone()
    };
    let label = entry.map_or_else(|| fallback_label(&app.app_id, proc), |e| e.name.clone());
    let terminal = entry.is_some_and(|e| e.terminal);
    (
        AppGroupKey {
            owner_uid,
            canonical,
        },
        label,
        terminal,
    )
}

/// A cross-run-stable persistence key, or `None` for identities whose bucket key is ephemeral.
/// Desktop applications (their name/launch or app id) and systemd units are stable across runs;
/// container ids, pod uids, and structural tokens are not, so their folds stay session-only.
fn persist_key(identity: &ProcessIdentity, key: &AppGroupKey) -> Option<String> {
    identity
        .kind
        .persists()
        .then(|| format!("{}\0{}", key.owner_uid, key.canonical))
}

fn fallback_label(app_id: &str, proc: &ProcessEntry) -> String {
    let tail = app_id.rsplit('.').next().unwrap_or(app_id);
    let cleaned = tail.trim_matches(|c: char| matches!(c, '-' | '_' | '.'));
    if !cleaned.is_empty() {
        return cleaned.replace(['-', '_'], " ");
    }
    String::from_utf8_lossy(proc.comm()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::DesktopResolver;
    use crate::fxhash::FxMap;
    use crate::identity::{DesktopApp, IdentityKind};
    use std::fs;

    fn proc(pid: u32, uid: u32) -> ProcessEntry {
        let mut p = ProcessEntry::TOMBSTONE;
        p.pid = pid;
        p.uid = uid;
        p.num_threads = 2;
        p.cpu_pct = pid;
        p.mem_bytes = u64::from(pid) * 10;
        p.set_comm(b"helper");
        p
    }

    fn desktop_id(app_id: &str, launcher: bool) -> ProcessIdentity {
        ProcessIdentity {
            owner_uid: 1000,
            kind: IdentityKind::DesktopApp(DesktopApp {
                app_id: app_id.into(),
                launcher_scope: launcher,
                instantiated: !launcher,
                flatpak: false,
            }),
        }
    }

    fn systemd_id(unit: &str) -> ProcessIdentity {
        ProcessIdentity {
            owner_uid: 0,
            kind: IdentityKind::Systemd { unit: unit.into() },
        }
    }

    fn rebuild<'a>(
        groups: &mut ApplicationGroups,
        procs: &[ProcessEntry],
        resolver: &mut DesktopResolver,
        identities: impl IntoIterator<Item = (usize, &'a ProcessIdentity)>,
    ) {
        groups.rebuild(procs, identities, resolver);
    }

    #[test]
    fn split_scopes_merge_on_desktop_name_and_launch() {
        let root = std::env::temp_dir().join(format!("oya-alias-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("com.google.Chrome.desktop"),
            "[Desktop Entry]\nName=Google Chrome\nExec=/usr/bin/google-chrome %U\n",
        )
        .unwrap();
        fs::write(
            root.join("google-chrome.desktop"),
            "[Desktop Entry]\nName=Google Chrome\nExec=/usr/bin/google-chrome --foo\n",
        )
        .unwrap();
        let mut resolver = DesktopResolver::with_roots(vec![root.clone()], FxMap::default());
        let procs = vec![proc(1, 1000), proc(2, 1000), proc(3, 1000)];
        let ids = [
            desktop_id("com.google.Chrome", true),
            desktop_id("google-chrome", false),
            desktop_id("google-chrome", false),
        ];
        let mut groups = ApplicationGroups::default();
        rebuild(&mut groups, &procs, &mut resolver, ids.iter().enumerate());
        assert_eq!(groups.groups.len(), 1);
        assert_eq!(groups.groups[0].label, "Google Chrome");
        assert_eq!(groups.groups[0].representative, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn minimum_threshold_is_three() {
        let procs = vec![proc(1, 1000), proc(2, 1000)];
        let mut resolver = DesktopResolver::with_roots(Vec::new(), FxMap::default());
        let ids = [desktop_id("foo", true), desktop_id("foo", false)];
        let mut groups = ApplicationGroups::default();
        rebuild(&mut groups, &procs, &mut resolver, ids.iter().enumerate());
        assert!(groups.groups.is_empty());
    }

    #[test]
    fn systemd_service_groups_and_labels_from_its_unit() {
        let procs = vec![proc(1, 0), proc(2, 0), proc(3, 0)];
        let mut resolver = DesktopResolver::with_roots(Vec::new(), FxMap::default());
        let ids = [
            systemd_id("example.service"),
            systemd_id("example.service"),
            systemd_id("example.service"),
        ];
        let mut groups = ApplicationGroups::default();
        rebuild(&mut groups, &procs, &mut resolver, ids.iter().enumerate());
        assert_eq!(groups.groups.len(), 1);
        assert_eq!(groups.groups[0].label, "example.service");
        assert_eq!(
            groups.groups[0].persist_key.as_deref(),
            Some("0\0systemd\0example.service")
        );
    }

    #[test]
    fn rebuild_replaces_the_previous_grouping() {
        let procs = vec![proc(1, 1000), proc(2, 1000), proc(3, 1000)];
        let ids = [
            desktop_id("foo", true),
            desktop_id("foo", false),
            desktop_id("foo", false),
        ];
        let mut resolver = DesktopResolver::with_roots(Vec::new(), FxMap::default());
        let mut groups = ApplicationGroups::default();
        groups.rebuild(&procs, ids.iter().enumerate(), &mut resolver);
        assert_eq!(groups.groups().len(), 1);
        groups.rebuild(&procs, std::iter::empty(), &mut resolver);
        assert!(groups.groups().is_empty());
    }

    #[test]
    fn tops_walk_through_foreign_intermediates() {
        // 1 (member) → 2 (foreign) → 3 (member) → 4 (member); 5 (member) is a separate root.
        let mut procs: Vec<_> = (1..=5).map(|pid| proc(pid, 1000)).collect();
        for (child, parent) in [(1, 0), (2, 1), (3, 2)] {
            procs[child].parent_idx = parent;
        }
        let member = |pid| desktop_id("foo", pid == 1);
        let ids = [
            member(1),
            desktop_id("bar", false),
            member(3),
            member(4),
            member(5),
        ];
        let mut resolver = DesktopResolver::with_roots(Vec::new(), FxMap::default());
        let mut groups = ApplicationGroups::default();
        groups.rebuild(&procs, ids.iter().enumerate(), &mut resolver);
        assert_eq!(groups.groups().len(), 1);
        // Process 4 and 3 sit below member 1 (through foreign 2 and directly), so only 1 and 5
        // are tops.
        assert_eq!(groups.top_group(0), Some(0));
        assert_eq!(groups.top_group(4), Some(0));
        assert_eq!(groups.top_group(2), None);
        assert_eq!(groups.member_group(2), Some(0));
        assert_eq!(groups.top_group(4), Some(0));
    }
}
