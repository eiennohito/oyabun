//! Identity types and the change-driven resolver.
//!
//! [`ProcessIdentity`] carries only what every kind shares (`owner_uid`); everything
//! kind-specific lives inside the [`IdentityKind`] variant, so a field that is meaningless for a
//! kind cannot be constructed for it. Whether a fold persists across runs and whether the
//! boundary is kernel- or heuristic-derived are *derived* from the kind, never stored alongside
//! it — there is no second axis to fall out of sync.

use std::borrow::Cow;

use super::{cgroup, structural::StructuralDetector};
use crate::fxhash::PidMap;
use crate::procs::ProcessEntry;

/// A process's resolved grouping identity. Processes sharing an `(owner_uid, bucket token)` —
/// after the view's optional desktop-entry refinement — form one group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    /// Owning user for display and bucketing: the cgroup session owner for cgroup kinds (0 for
    /// system scopes), or the root process's uid for structural kinds. Constant across a group's
    /// members even when their individual process uids differ.
    pub(crate) owner_uid: u32,
    pub(crate) kind: IdentityKind,
}

/// What kind of group a process belongs to, carrying that kind's payload inline. The four
/// cgroup-derived kinds are kernel-owned; [`Structural`](Self::Structural) is a session-only
/// heuristic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IdentityKind {
    /// A freedesktop desktop-session application (systemd `app.slice` `app-<id>` unit), possibly
    /// packaged as a flatpak. Its label resolves from the desktop entry via the app id.
    DesktopApp(DesktopApp),
    /// A container scope; `label` is `container/<short-id>`.
    Container { label: String },
    /// A Kubernetes pod slice; `label` is `pod/<short-uid>`.
    Pod { label: String },
    /// A system service unit; `unit` is the `.service` name (also the label).
    Systemd { unit: String },
    /// A multi-process application with no stable cgroup boundary (Chromium fan, runtime pool,
    /// container shim), detected from tree shape and process-controlled names/argv.
    Structural(Structural),
}

/// The payload of an [`IdentityKind::DesktopApp`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DesktopApp {
    /// freedesktop app id, resolved through the desktop entry by the view.
    pub(crate) app_id: String,
    /// A launcher `.scope` (vs an instantiated `.service`) — the view elects a split
    /// application's representative from its launcher.
    pub(crate) launcher_scope: bool,
    pub(crate) instantiated: bool,
    pub(crate) flatpak: bool,
}

/// The payload of an [`IdentityKind::Structural`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Structural {
    pub(crate) role: StructuralRole,
    /// A per-incarnation bucket token: session-only, evicted when the root exits or its PID is
    /// reused. Never cross-run stable, so a structural fold never persists.
    pub(crate) token: String,
    /// Ready-to-show label (the root command, or `comm` when the command is unavailable).
    pub(crate) label: String,
}

/// Which structural heuristic recognized the group. Behaviourally uniform (all session-only);
/// retained for display and diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StructuralRole {
    /// Chromium/Electron process fan.
    Chromium,
    /// Language-runtime worker pool sharing an entrypoint.
    RuntimePool,
    /// Container shim parenting a container's processes.
    ContainerShim,
}

impl IdentityKind {
    /// Whether a fold preference keyed on this identity is safe to persist across runs. Desktop
    /// applications and systemd units have cross-run-stable identifiers; container/pod ids and
    /// structural tokens are minted per run, so their folds stay session-only.
    pub(crate) fn persists(&self) -> bool {
        matches!(self, Self::DesktopApp(_) | Self::Systemd { .. })
    }

    /// The bucket token within `owner_uid`. Processes sharing `(owner_uid, token)` group together;
    /// for a desktop app the view may still refine this through the desktop entry to merge split
    /// launcher/service scopes.
    pub(crate) fn bucket_token(&self) -> Cow<'_, str> {
        match self {
            Self::DesktopApp(app) => Cow::Borrowed(&app.app_id),
            Self::Container { label } | Self::Pod { label } => Cow::Borrowed(label),
            Self::Systemd { unit } => Cow::Owned(format!("systemd\0{unit}")),
            Self::Structural(s) => Cow::Borrowed(&s.token),
        }
    }

    /// The ready-to-show label for kinds that carry one; `None` for a desktop app, whose label the
    /// view resolves from the desktop entry.
    pub(crate) fn label(&self) -> Option<&str> {
        match self {
            Self::DesktopApp(_) => None,
            Self::Container { label } | Self::Pod { label } => Some(label),
            Self::Systemd { unit } => Some(unit),
            Self::Structural(s) => Some(&s.label),
        }
    }
}

/// Per-process metadata the resolver reads (implemented by the gather table). Replaces a direct
/// dependency on gather internals so the identity layer stays self-contained.
pub(crate) trait ProcMeta {
    fn cmdline(&self, e: &ProcessEntry) -> &[u8];
    fn cgroup(&self, e: &ProcessEntry) -> &[u8];
    fn flatpak_info(&self, e: &ProcessEntry) -> &[u8];
}

/// One process incarnation's resolved identity, keyed by PID and validated by `start_time` so a
/// reused PID never inherits the prior incarnation's grouping.
struct CacheEntry {
    start_time: u64,
    identity: Option<ProcessIdentity>,
}

/// Resolves and caches per-PID identities for the whole process buffer.
///
/// Identity is a pure function of each process's cgroup/argv/comm and the tree shape around it —
/// inputs that change only on birth, death, PID reuse, or an argv/cgroup rewrite. The gather
/// table folds all of those into a monotonic *epoch*; while the epoch holds steady the cached
/// identities are provably still correct, so [`update`](Self::update) returns without touching
/// the process buffer. A rebuild resolves cgroup identity per process, then runs structural
/// detection over the subtrees that remain unidentified.
#[derive(Default)]
pub(crate) struct IdentityResolver {
    by_pid: PidMap<CacheEntry>,
    /// Proc-index-keyed scratch reused across rebuilds: cgroup pass fills it, structural detection
    /// stamps into it, then it is drained (moved, not cloned) into `by_pid`.
    scratch: Vec<Option<ProcessIdentity>>,
    preorder_pos: Vec<usize>,
    structural: StructuralDetector,
    /// The metadata epoch the cache currently reflects; `None` until the first rebuild.
    epoch: Option<u64>,
}

impl IdentityResolver {
    /// Recompute identities for `procs` (in `preorder` tree order) if `epoch` has moved since the
    /// last rebuild; otherwise reuse the cache untouched. The caller must pass a monotonically
    /// advancing `epoch` that changes whenever any identity-relevant input does.
    pub(crate) fn update(
        &mut self,
        procs: &[ProcessEntry],
        preorder: &[u32],
        meta: &dyn ProcMeta,
        epoch: u64,
    ) {
        if self.epoch == Some(epoch) {
            return;
        }
        self.epoch = Some(epoch);

        self.scratch.clear();
        self.scratch.extend(
            procs
                .iter()
                .map(|p| cgroup::extract(meta.cgroup(p), meta.flatpak_info(p))),
        );

        self.preorder_pos.clear();
        self.preorder_pos.resize(procs.len(), 0);
        for (pos, &idx) in preorder.iter().enumerate() {
            self.preorder_pos[idx as usize] = pos;
        }

        self.structural
            .detect(procs, preorder, &self.preorder_pos, meta, &mut self.scratch);

        self.by_pid.clear();
        for (idx, p) in procs.iter().enumerate() {
            self.by_pid.insert(
                p.pid,
                CacheEntry {
                    start_time: p.start_time,
                    identity: self.scratch[idx].take(),
                },
            );
        }
    }

    /// This incarnation's resolved identity, or `None` if it belongs to no group (or the cache
    /// holds a different incarnation of this PID).
    pub(crate) fn identity(&self, pid: u32, start_time: u64) -> Option<&ProcessIdentity> {
        self.by_pid
            .get(&pid)
            .filter(|entry| entry.start_time == start_time)
            .and_then(|entry| entry.identity.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fxhash::PidMap;
    use crate::tree;

    #[derive(Default)]
    struct Meta {
        cmdlines: PidMap<Vec<u8>>,
        cgroups: PidMap<Vec<u8>>,
        flatpaks: PidMap<Vec<u8>>,
    }

    impl ProcMeta for Meta {
        fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
            self.cmdlines.get(&e.pid).map_or(&[], Vec::as_slice)
        }
        fn cgroup(&self, e: &ProcessEntry) -> &[u8] {
            self.cgroups.get(&e.pid).map_or(&[], Vec::as_slice)
        }
        fn flatpak_info(&self, e: &ProcessEntry) -> &[u8] {
            self.flatpaks.get(&e.pid).map_or(&[], Vec::as_slice)
        }
    }

    fn row(pid: u32, parent: u32, comm: &[u8]) -> ProcessEntry {
        let mut e = ProcessEntry::TOMBSTONE;
        e.pid = pid;
        e.ppid = parent;
        e.start_time = u64::from(pid);
        e.set_comm(comm);
        e
    }

    /// Build the tree, resolve identities, and return `(resolver, procs)`.
    fn resolve(mut procs: Vec<ProcessEntry>, meta: &Meta) -> (IdentityResolver, Vec<ProcessEntry>) {
        procs.sort_unstable_by_key(|p| p.pid);
        let (mut stack, mut order) = (Vec::new(), Vec::new());
        tree::build(&mut procs, &mut stack, &mut order);
        let mut resolver = IdentityResolver::default();
        resolver.update(&procs, &order, meta, 1);
        (resolver, procs)
    }

    fn identity_of<'a>(
        resolver: &'a IdentityResolver,
        procs: &[ProcessEntry],
        pid: u32,
    ) -> Option<&'a ProcessIdentity> {
        let p = procs.iter().find(|p| p.pid == pid).unwrap();
        resolver.identity(p.pid, p.start_time)
    }

    fn is_structural(resolver: &IdentityResolver, procs: &[ProcessEntry], pid: u32) -> bool {
        matches!(
            identity_of(resolver, procs, pid).map(|id| &id.kind),
            Some(IdentityKind::Structural(_))
        )
    }

    fn chromium_procs(with_type: bool) -> (Vec<ProcessEntry>, Meta) {
        let procs = (10..=15)
            .map(|pid| row(pid, if pid == 10 { 0 } else { 10 }, b"chrome"))
            .collect();
        let mut meta = Meta::default();
        for pid in 10..=15 {
            meta.cmdlines.insert(pid, b"/usr/bin/chrome".to_vec());
        }
        if with_type {
            meta.cmdlines
                .insert(12, b"/usr/bin/chrome --type=renderer".to_vec());
        }
        (procs, meta)
    }

    #[test]
    fn chromium_fan_with_type_child_stamps_whole_subtree() {
        let (procs, meta) = chromium_procs(true);
        let (r, procs) = resolve(procs, &meta);
        for pid in 10..=15 {
            let id = identity_of(&r, &procs, pid).unwrap_or_else(|| panic!("pid {pid}"));
            assert!(
                matches!(
                    &id.kind,
                    IdentityKind::Structural(s) if s.role == StructuralRole::Chromium
                ),
                "pid {pid}"
            );
        }
        assert!(is_structural(&r, &procs, 10));
        assert!(!identity_of(&r, &procs, 10).unwrap().kind.persists());
    }

    #[test]
    fn shared_comm_fan_without_type_child_is_ungrouped() {
        let (procs, meta) = chromium_procs(false);
        let (r, procs) = resolve(procs, &meta);
        for pid in 10..=15 {
            assert!(identity_of(&r, &procs, pid).is_none(), "pid {pid}");
        }
    }

    #[test]
    fn runtime_pool_with_shared_entrypoint_groups() {
        let procs = (20..=23)
            .map(|pid| row(pid, if pid == 20 { 0 } else { 20 }, b"python3"))
            .collect();
        let mut meta = Meta::default();
        for pid in 20..=23 {
            meta.cmdlines
                .insert(pid, b"/usr/bin/python3 /srv/worker.py".to_vec());
        }
        let (r, procs) = resolve(procs, &meta);
        assert!(matches!(
            identity_of(&r, &procs, 20).map(|id| &id.kind),
            Some(IdentityKind::Structural(s)) if s.role == StructuralRole::RuntimePool
        ));
    }

    #[test]
    fn runtime_pool_without_shared_entrypoint_is_ungrouped() {
        let procs = (20..=23)
            .map(|pid| row(pid, if pid == 20 { 0 } else { 20 }, b"python3"))
            .collect();
        let mut meta = Meta::default();
        meta.cmdlines
            .insert(20, b"/usr/bin/python3 /srv/a.py".to_vec());
        meta.cmdlines
            .insert(21, b"/usr/bin/python3 /srv/b.py".to_vec());
        meta.cmdlines
            .insert(22, b"/usr/bin/python3 /srv/c.py".to_vec());
        meta.cmdlines
            .insert(23, b"/usr/bin/python3 /srv/d.py".to_vec());
        let (r, procs) = resolve(procs, &meta);
        assert!(identity_of(&r, &procs, 20).is_none());
    }

    #[test]
    fn container_shim_with_id_arg_groups() {
        let procs = vec![
            row(30, 0, b"containerd-shim"),
            row(31, 30, b"app"),
            row(32, 30, b"app"),
        ];
        let mut meta = Meta::default();
        meta.cmdlines.insert(
            30,
            b"/usr/bin/containerd-shim-runc-v2 -namespace moby -id abc123".to_vec(),
        );
        let (r, procs) = resolve(procs, &meta);
        assert!(matches!(
            identity_of(&r, &procs, 30).map(|id| &id.kind),
            Some(IdentityKind::Structural(s)) if s.role == StructuralRole::ContainerShim
        ));
    }

    #[test]
    fn cgroup_identity_wins_over_structural_heuristic() {
        // A Chromium fan that also sits in an app.slice scope is a trusted desktop app, never a
        // structural Chromium group.
        let (mut procs, mut meta) = chromium_procs(true);
        procs.sort_unstable_by_key(|p| p.pid);
        for pid in 10..=15 {
            meta.cgroups.insert(
                pid,
                b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-com.google.Chrome-10.scope\n".to_vec(),
            );
        }
        let (r, procs) = resolve(procs, &meta);
        for pid in 10..=15 {
            assert!(
                identity_of(&r, &procs, pid)
                    .is_some_and(|id| matches!(id.kind, IdentityKind::DesktopApp(_))),
                "pid {pid}"
            );
        }
    }

    #[test]
    fn bare_sandbox_without_provenance_is_ungrouped() {
        let procs = vec![row(50, 0, b"bwrap"), row(51, 50, b"sh"), row(52, 50, b"sh")];
        let mut meta = Meta::default();
        meta.cgroups.insert(
            50,
            b"0::/user.slice/user-1000.slice/session-2.scope\n".to_vec(),
        );
        let (r, procs) = resolve(procs, &meta);
        assert!(identity_of(&r, &procs, 50).is_none());
    }

    #[test]
    fn systemd_service_members_share_one_identity_regardless_of_tree_position() {
        // Membership follows shared identity, not ancestry: a scattered service still coheres.
        let procs = vec![
            row(1, 0, b"init"),
            row(60, 1, b"daemon"),
            row(61, 1, b"worker"),
            row(62, 60, b"worker"),
        ];
        let mut meta = Meta::default();
        meta.cgroups.insert(1, b"0::/init.scope\n".to_vec());
        for pid in [60, 61, 62] {
            meta.cgroups
                .insert(pid, b"0::/system.slice/example.service\n".to_vec());
        }
        let (r, procs) = resolve(procs, &meta);
        assert!(identity_of(&r, &procs, 1).is_none());
        for pid in [60, 61, 62] {
            assert!(
                matches!(
                    identity_of(&r, &procs, pid).map(|id| &id.kind),
                    Some(IdentityKind::Systemd { .. })
                ),
                "pid {pid}"
            );
        }
    }

    #[test]
    fn steady_epoch_reuses_cache_and_a_moved_epoch_rebuilds() {
        let (mut procs, meta) = chromium_procs(true);
        procs.sort_unstable_by_key(|p| p.pid);
        let (mut stack, mut order) = (Vec::new(), Vec::new());
        tree::build(&mut procs, &mut stack, &mut order);

        let mut r = IdentityResolver::default();
        r.update(&procs, &order, &meta, 7);
        assert!(is_structural(&r, &procs, 10));

        // A no-op meta with no cgroups: were it consulted, pid 10 would lose its identity. The
        // steady epoch means it is not consulted, so the cache stands.
        r.update(&procs, &order, &Meta::default(), 7);
        assert!(is_structural(&r, &procs, 10));

        // A moved epoch forces a rebuild against the (empty) meta, dropping the group.
        r.update(&procs, &order, &Meta::default(), 8);
        assert!(identity_of(&r, &procs, 10).is_none());
    }
}
