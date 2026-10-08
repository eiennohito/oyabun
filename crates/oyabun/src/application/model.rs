use crate::application::DesktopResolver;
use crate::fxhash::FxMap;
use crate::identity::{IdentityKind, ProcessIdentity};
use crate::procs::{GpuMetrics, ProcessEntry};
use std::collections::VecDeque;

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
    pub(crate) members: Vec<usize>,
    pub(crate) label: String,
    pub(crate) owner_uid: u32,
    /// Cross-run-stable key for persisting a fold preference, or `None` when the identity is
    /// ephemeral (container/pod ids, structural tokens) and folds stay session-only.
    pub(crate) persist_key: Option<String>,
    pub(crate) threads: u32,
    pub(crate) cpu_pct: u32,
    pub(crate) cpu_peak: u32,
    pub(crate) mem_bytes: u64,
    pub(crate) gpu: Option<GpuMetrics>,
}

#[derive(Default)]
pub(crate) struct ApplicationGroups {
    groups: Vec<AppGroup>,
    member_group: Vec<Option<usize>>,
    peaks: FxMap<AppGroupKey, GroupCpuHistory>,
    /// Metadata epoch the current grouping reflects; `None` until the first regroup.
    epoch: Option<u64>,
}

#[derive(Default)]
struct GroupCpuHistory {
    samples: VecDeque<u32>,
}

impl GroupCpuHistory {
    fn push(&mut self, sample: u32) -> u32 {
        const WINDOW: usize = 10;
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.samples.iter().copied().max().unwrap_or(0)
    }
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
    /// Refresh the application grouping for this cycle. Membership, labels, and keys are a pure
    /// function of the per-process identities and so are recomputed only when the metadata
    /// `epoch` moves (an unchanged epoch also means an unchanged pid set, hence unchanged proc
    /// indices, since the process buffer is pid-sorted). The aggregated metrics move every cycle
    /// regardless, so they are always refreshed over the current buffer.
    pub(crate) fn rebuild<'a>(
        &mut self,
        procs: &[ProcessEntry],
        identities: impl IntoIterator<Item = (usize, &'a ProcessIdentity)>,
        resolver: &mut DesktopResolver,
        gpu_for_pid: impl Fn(u32) -> Option<GpuMetrics>,
        epoch: u64,
    ) {
        if self.epoch != Some(epoch) {
            self.epoch = Some(epoch);
            self.regroup(procs, identities, resolver);
        }
        self.reaggregate(procs, &gpu_for_pid);
    }

    /// Bucket processes by identity into groups (membership, representative, label, keys). Runs
    /// only on a moved epoch; leaves the metric fields zeroed for [`reaggregate`](Self::reaggregate).
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
        self.member_group.clear();
        self.member_group.resize(procs.len(), None);
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
            let group_idx = self.groups.len();
            for &idx in &member_indices {
                self.member_group[idx] = Some(group_idx);
            }
            self.groups.push(AppGroup {
                key,
                representative,
                members: member_indices,
                label,
                owner_uid,
                persist_key,
                threads: 0,
                cpu_pct: 0,
                cpu_peak: 0,
                mem_bytes: 0,
                gpu: None,
            });
        }
        let live: std::collections::HashSet<_> =
            self.groups.iter().map(|g| g.key.clone()).collect();
        self.peaks.retain(|key, _| live.contains(key));
    }

    /// Recompute each group's aggregated metrics (threads, CPU, memory, GPU) and push the CPU
    /// peak window from the current process buffer. Runs every cycle — these values move even
    /// when membership does not.
    fn reaggregate(
        &mut self,
        procs: &[ProcessEntry],
        gpu_for_pid: &impl Fn(u32) -> Option<GpuMetrics>,
    ) {
        let Self { groups, peaks, .. } = self;
        for group in groups.iter_mut() {
            let mut threads = 0u32;
            let mut cpu_pct = 0u32;
            let mut mem_bytes = 0u64;
            let mut gpu = None;
            for &idx in &group.members {
                let proc = &procs[idx];
                threads = threads.saturating_add(proc.num_threads);
                cpu_pct = cpu_pct.saturating_add(proc.cpu_pct);
                mem_bytes = mem_bytes.saturating_add(proc.mem_bytes);
                if let Some(sample) = gpu_for_pid(proc.pid) {
                    gpu = Some(gpu.map_or(sample, |sum: GpuMetrics| sum.saturating_add(sample)));
                }
            }
            group.threads = threads;
            group.cpu_pct = cpu_pct;
            group.mem_bytes = mem_bytes;
            group.cpu_peak = peaks.entry(group.key.clone()).or_default().push(cpu_pct);
            group.gpu = gpu;
        }
    }

    pub(crate) fn groups(&self) -> &[AppGroup] {
        &self.groups
    }

    pub(crate) fn groups_mut(&mut self) -> &mut [AppGroup] {
        &mut self.groups
    }

    pub(crate) fn member_group(&self, proc_idx: usize) -> Option<usize> {
        self.member_group.get(proc_idx).copied().flatten()
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
        groups.rebuild(procs, identities, resolver, |_| None, 1);
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
    fn regroup_is_epoch_driven_but_metrics_still_refresh() {
        let procs = vec![proc(1, 1000), proc(2, 1000), proc(3, 1000)];
        let ids = [
            desktop_id("foo", true),
            desktop_id("foo", false),
            desktop_id("foo", false),
        ];
        let mut resolver = DesktopResolver::with_roots(Vec::new(), FxMap::default());
        let mut groups = ApplicationGroups::default();
        groups.rebuild(&procs, ids.iter().enumerate(), &mut resolver, |_| None, 5);
        assert_eq!(groups.groups().len(), 1);

        // Same epoch, but feed no identities: were regroup to run, the group would vanish. The
        // epoch holds it, and metrics still refresh over the current buffer.
        groups.rebuild(&procs, std::iter::empty(), &mut resolver, |_| None, 5);
        assert_eq!(groups.groups().len(), 1);
        assert_eq!(groups.groups()[0].threads, 6);

        // A moved epoch regroups against the (now empty) identities and drops the group.
        groups.rebuild(&procs, std::iter::empty(), &mut resolver, |_| None, 6);
        assert!(groups.groups().is_empty());
    }

    #[test]
    fn group_peak_is_a_window_of_aggregate_samples() {
        let mut procs = vec![proc(1, 1000), proc(2, 1000), proc(3, 1000)];
        let ids = [
            desktop_id("foo", true),
            desktop_id("foo", false),
            desktop_id("foo", false),
        ];
        let mut resolver = DesktopResolver::with_roots(Vec::new(), FxMap::default());
        let mut groups = ApplicationGroups::default();
        rebuild(&mut groups, &procs, &mut resolver, ids.iter().enumerate());
        let first_peak = groups.groups()[0].cpu_peak;
        for proc in &mut procs {
            proc.cpu_pct = 0;
        }
        for _ in 0..10 {
            rebuild(&mut groups, &procs, &mut resolver, ids.iter().enumerate());
        }
        assert!(first_peak > 0);
        assert_eq!(groups.groups()[0].cpu_peak, 0);
    }
}
