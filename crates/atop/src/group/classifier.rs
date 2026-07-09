use super::detectors;
use super::{GroupFact, GroupMetaView, GroupRule, TreeView};
use crate::fxhash::PidMap;
use crate::procs::{NONE, ProcessEntry};

const GROUP_SETTLE_GENS: u64 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
struct GroupState {
    start_time: u64,
    last_seen_gen: u64,
    phase: GroupPhase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GroupPhase {
    Settling { first_seen_gen: u64 },
    NoGroup,
    Group { fact: GroupFact, auto_rank: u8 },
}

pub(crate) trait GroupMetaSource {
    fn cmdline(&self, e: &ProcessEntry) -> &[u8];
    fn cgroup(&self, e: &ProcessEntry) -> &[u8];
    fn flatpak_info(&self, e: &ProcessEntry) -> &[u8];
}

pub(crate) struct GroupClassifier {
    states: PidMap<GroupState>,
    rules: Vec<Box<dyn GroupRule>>,
    tree_stack: Vec<u32>,
    tree_order: Vec<u32>,
    preorder_pos: Vec<usize>,
    auto_roots: Vec<u32>,
    rank_path: Vec<u8>,
    settling: Vec<usize>,
}

impl GroupClassifier {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            states: PidMap::default(),
            rules: detectors::default_rules(),
            tree_stack: Vec::new(),
            tree_order: Vec::new(),
            preorder_pos: Vec::new(),
            auto_roots: Vec::new(),
            rank_path: Vec::new(),
            settling: Vec::new(),
        }
    }

    pub(crate) fn update(
        &mut self,
        procs: &[ProcessEntry],
        meta: &dyn GroupMetaSource,
        cur_gen: u64,
    ) {
        build_preorder(
            procs,
            &mut self.tree_stack,
            &mut self.tree_order,
            &mut self.preorder_pos,
        );
        let tree = TreeView::new(procs, &self.tree_order, &self.preorder_pos);
        let meta = GroupMetaView::new(meta);

        self.settling.clear();
        for (pid_idx, p) in procs.iter().enumerate() {
            let state = self
                .states
                .entry(p.pid)
                .and_modify(|state| {
                    if state.start_time == p.start_time {
                        state.last_seen_gen = cur_gen;
                    } else {
                        *state = GroupState {
                            start_time: p.start_time,
                            last_seen_gen: cur_gen,
                            phase: GroupPhase::Settling {
                                first_seen_gen: cur_gen,
                            },
                        };
                    }
                })
                .or_insert(GroupState {
                    start_time: p.start_time,
                    last_seen_gen: cur_gen,
                    phase: GroupPhase::Settling {
                        first_seen_gen: cur_gen,
                    },
                });

            if matches!(state.phase, GroupPhase::Settling { .. }) {
                self.settling.push(pid_idx);
            }
        }

        if !self.settling.is_empty() {
            for rule in &mut self.rules {
                rule.prepare(&tree, &meta);
            }
        }

        for &pid_idx in &self.settling {
            let p = &procs[pid_idx];
            let state = self.states.get_mut(&p.pid).expect("settling PID has state");
            let GroupPhase::Settling { first_seen_gen } = state.phase else {
                continue;
            };
            for rule in &self.rules {
                if let Some(fact) = rule.detect(&tree, &meta, pid_idx) {
                    state.phase.consider(fact);
                }
            }
            if matches!(state.phase, GroupPhase::Settling { .. })
                && cur_gen.wrapping_sub(first_seen_gen) >= GROUP_SETTLE_GENS
            {
                state.phase = GroupPhase::NoGroup;
            }
        }

        self.states
            .retain(|_, state| state.last_seen_gen == cur_gen);
        self.rebuild_auto_roots(procs);
    }

    #[must_use]
    pub(crate) fn fact(&self, pid: u32, start_time: u64) -> Option<&GroupFact> {
        match self.states.get(&pid) {
            Some(GroupState {
                start_time: state_start,
                phase: GroupPhase::Group { fact, .. },
                ..
            }) if *state_start == start_time => Some(fact),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) fn auto_roots(&self) -> &[u32] {
        &self.auto_roots
    }

    #[cfg(test)]
    pub(crate) fn contains_pid(&self, pid: u32) -> bool {
        self.states.contains_key(&pid)
    }
}

impl GroupPhase {
    fn consider(&mut self, candidate: GroupFact) {
        let candidate_auto_rank = candidate
            .auto_collapse()
            .then_some(candidate.rank())
            .unwrap_or(0);
        match self {
            Self::Settling { .. } => {
                *self = Self::Group {
                    fact: candidate,
                    auto_rank: candidate_auto_rank,
                };
            }
            Self::Group { fact, auto_rank } => {
                *auto_rank = (*auto_rank).max(candidate_auto_rank);
                if candidate.rank() > fact.rank() {
                    *fact = candidate;
                }
            }
            Self::NoGroup => {}
        }
    }

    fn auto_rank(&self) -> u8 {
        match self {
            Self::Group { auto_rank, .. } => *auto_rank,
            Self::Settling { .. } | Self::NoGroup => 0,
        }
    }
}

impl GroupClassifier {
    fn rebuild_auto_roots(&mut self, procs: &[ProcessEntry]) {
        self.auto_roots.clear();
        self.rank_path.clear();
        for &idx in &self.tree_order {
            let p = &procs[idx as usize];
            let depth = usize::from(p.depth);
            self.rank_path.truncate(depth);
            let ancestor_rank = self.rank_path.last().copied().unwrap_or(0);
            let own_rank = self
                .states
                .get(&p.pid)
                .filter(|state| state.start_time == p.start_time)
                .map_or(0, |state| state.phase.auto_rank());
            if own_rank > ancestor_rank {
                self.auto_roots.push(p.pid);
            }
            self.rank_path.push(ancestor_rank.max(own_rank));
        }
    }
}

fn build_preorder(
    procs: &[ProcessEntry],
    stack: &mut Vec<u32>,
    order: &mut Vec<u32>,
    positions: &mut Vec<usize>,
) {
    stack.clear();
    order.clear();
    positions.clear();
    positions.resize(procs.len(), 0);
    for (idx, p) in procs.iter().enumerate().rev() {
        if p.parent_idx == NONE {
            stack.push(idx as u32);
        }
    }
    while let Some(idx) = stack.pop() {
        positions[idx as usize] = order.len();
        order.push(idx);
        let base = stack.len();
        let mut child = procs[idx as usize].first_child;
        while child != NONE {
            stack.push(child);
            child = procs[child as usize].next_sibling;
        }
        stack[base..].reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::{GroupClassifier, GroupMetaSource};
    use crate::fxhash::PidMap;
    use crate::procs::ProcessEntry;
    use crate::tree;

    #[derive(Default)]
    struct Meta {
        cmdlines: PidMap<Vec<u8>>,
        cgroups: PidMap<Vec<u8>>,
        flatpaks: PidMap<Vec<u8>>,
    }

    impl GroupMetaSource for Meta {
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

    fn row(process_id: u32, parent_pid: u32, start_time: u64, comm: &[u8]) -> ProcessEntry {
        let mut e = ProcessEntry::TOMBSTONE;
        e.pid = process_id;
        e.ppid = parent_pid;
        e.start_time = start_time;
        e.set_comm(comm);
        e
    }

    fn build(mut procs: Vec<ProcessEntry>) -> Vec<ProcessEntry> {
        procs.sort_unstable_by_key(|p| p.pid);
        let (mut stack, mut order) = (Vec::new(), Vec::new());
        tree::build(&mut procs, &mut stack, &mut order);
        procs
    }

    fn chromium_tree(type_child: bool) -> (Vec<ProcessEntry>, Meta) {
        let procs = build(vec![
            row(10, 0, 100, b"chrome"),
            row(11, 10, 101, b"chrome"),
            row(12, 10, 102, b"chrome"),
            row(13, 10, 103, b"chrome"),
            row(14, 10, 104, b"chrome"),
            row(15, 10, 105, b"chrome"),
        ]);
        let mut meta = Meta::default();
        meta.cmdlines.insert(10, b"/usr/bin/chrome".to_vec());
        if type_child {
            meta.cmdlines
                .insert(12, b"/usr/bin/chrome --type=renderer".to_vec());
        }
        (procs, meta)
    }

    fn runtime_tree(script: &[u8]) -> (Vec<ProcessEntry>, Meta) {
        let procs = build(vec![
            row(20, 0, 200, b"python3"),
            row(21, 20, 201, b"python3"),
            row(22, 20, 202, b"python3"),
            row(23, 20, 203, b"python3"),
        ]);
        let mut meta = Meta::default();
        for pid in 20..=23 {
            let mut cmd = b"/usr/bin/python3 ".to_vec();
            cmd.extend_from_slice(script);
            meta.cmdlines.insert(pid, cmd);
        }
        (procs, meta)
    }

    #[test]
    fn shared_comm_fan_with_type_child_groups() {
        let (procs, meta) = chromium_tree(true);
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        let fact = classifier.fact(10, 100).expect("root grouped");
        assert_eq!(fact.rule_id(), "chromium");
        assert_eq!(fact.label().bytes(), b"/usr/bin/chrome");
    }

    #[test]
    fn shared_comm_fan_without_type_child_settles_to_no_group() {
        let (procs, meta) = chromium_tree(false);
        let mut classifier = GroupClassifier::new();
        for generation in 1..=4 {
            classifier.update(&procs, &meta, generation);
        }
        assert!(classifier.fact(10, 100).is_none());
    }

    #[test]
    fn pid_reuse_resets_classification() {
        let (procs, meta) = chromium_tree(false);
        let mut classifier = GroupClassifier::new();
        for generation in 1..=4 {
            classifier.update(&procs, &meta, generation);
        }
        assert!(classifier.fact(10, 100).is_none());

        let (mut reused, meta) = chromium_tree(true);
        reused.iter_mut().find(|p| p.pid == 10).unwrap().start_time = 200;
        classifier.update(&reused, &meta, 5);
        assert!(classifier.fact(10, 200).is_some());
    }

    #[test]
    fn vanished_pid_is_evicted() {
        let (procs, meta) = chromium_tree(true);
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        assert!(classifier.contains_pid(10));
        classifier.update(&[], &meta, 2);
        assert!(!classifier.contains_pid(10));
    }

    #[test]
    fn late_type_child_after_no_group_does_not_rerun() {
        let (procs, meta) = chromium_tree(false);
        let mut classifier = GroupClassifier::new();
        for generation in 1..=4 {
            classifier.update(&procs, &meta, generation);
        }

        let (procs, meta) = chromium_tree(true);
        classifier.update(&procs, &meta, 5);
        assert!(classifier.fact(10, 100).is_none());
    }

    #[test]
    fn runtime_pool_with_shared_entrypoint_groups() {
        let (procs, meta) = runtime_tree(b"/srv/worker.py");
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        let fact = classifier.fact(20, 200).expect("runtime pool grouped");
        assert_eq!(fact.rule_id(), "runtime-pool");
        assert_eq!(fact.label().bytes(), b"/usr/bin/python3 /srv/worker.py");
    }

    #[test]
    fn runtime_pool_requires_shared_entrypoint() {
        let (procs, mut meta) = runtime_tree(b"/srv/worker.py");
        meta.cmdlines
            .insert(21, b"/usr/bin/python3 /srv/other-a.py".to_vec());
        meta.cmdlines
            .insert(22, b"/usr/bin/python3 /srv/other-b.py".to_vec());
        let mut classifier = GroupClassifier::new();
        for generation in 1..=4 {
            classifier.update(&procs, &meta, generation);
        }
        assert!(classifier.fact(20, 200).is_none());
    }

    #[test]
    fn container_shim_with_container_id_groups() {
        let procs = build(vec![
            row(30, 0, 300, b"containerd-shim"),
            row(31, 30, 301, b"pause"),
            row(32, 30, 302, b"app"),
        ]);
        let mut meta = Meta::default();
        meta.cmdlines.insert(
            30,
            b"/usr/bin/containerd-shim-runc-v2 -namespace moby -id abc123".to_vec(),
        );
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        let fact = classifier.fact(30, 300).expect("container shim grouped");
        assert_eq!(fact.rule_id(), "container-shim");
    }

    #[test]
    fn flatpak_cgroup_with_info_groups_by_app_id() {
        let procs = build(vec![row(40, 0, 400, b"bwrap"), row(41, 40, 401, b"app")]);
        let mut meta = Meta::default();
        meta.cgroups.insert(
            40,
            b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.example.App-123.scope\n".to_vec(),
        );
        meta.flatpaks.insert(
            40,
            b"[Application]\nname=org.example.App\nbranch=stable\n".to_vec(),
        );
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        let fact = classifier.fact(40, 400).expect("flatpak grouped");
        assert_eq!(fact.rule_id(), "flatpak");
        assert_eq!(fact.label().bytes(), b"org.example.App/stable");
    }

    #[test]
    fn plain_bwrap_without_flatpak_info_does_not_group() {
        let procs = build(vec![row(50, 0, 500, b"bwrap"), row(51, 50, 501, b"sh")]);
        let mut meta = Meta::default();
        meta.cgroups.insert(
            50,
            b"0::/user.slice/user-1000.slice/session-2.scope\n".to_vec(),
        );
        let mut classifier = GroupClassifier::new();
        for generation in 1..=4 {
            classifier.update(&procs, &meta, generation);
        }
        assert!(classifier.fact(50, 500).is_none());
    }

    #[test]
    fn systemd_service_subtree_groups() {
        let procs = build(vec![
            row(60, 0, 600, b"daemon"),
            row(61, 60, 601, b"worker"),
        ]);
        let mut meta = Meta::default();
        for pid in 60..=61 {
            meta.cgroups
                .insert(pid, b"0::/system.slice/example.service\n".to_vec());
        }
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        let fact = classifier.fact(60, 600).expect("service grouped");
        assert_eq!(fact.rule_id(), "cgroup-systemd");
        assert_eq!(fact.label().bytes(), b"example.service");
    }

    #[test]
    fn chromium_identity_beats_generic_systemd_for_same_root() {
        let procs = build(vec![
            row(65, 0, 650, b"chrome"),
            row(66, 65, 651, b"chrome"),
            row(67, 65, 652, b"chrome"),
            row(68, 65, 653, b"chrome"),
            row(69, 65, 654, b"chrome"),
            row(70, 65, 655, b"chrome"),
        ]);
        let mut meta = Meta::default();
        for pid in 65..=70 {
            meta.cgroups.insert(
                pid,
                b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-browser@abc.service\n".to_vec(),
            );
            meta.cmdlines.insert(pid, b"/opt/browser/chrome".to_vec());
        }
        meta.cmdlines
            .insert(67, b"/opt/browser/chrome --type=renderer".to_vec());

        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        let fact = classifier.fact(65, 650).expect("app grouped");
        assert_eq!(fact.rule_id(), "chromium");
        assert_eq!(fact.label().bytes(), b"/opt/browser/chrome");
    }

    #[test]
    fn transient_terminal_scope_subtree_does_not_group() {
        let procs = build(vec![
            row(75, 0, 750, b"zsh"),
            row(76, 75, 751, b"node"),
            row(77, 75, 752, b"python"),
        ]);
        let mut meta = Meta::default();
        for pid in 75..=77 {
            meta.cgroups.insert(
                pid,
                b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/kitty-2278-3.scope\n"
                    .to_vec(),
            );
        }

        let mut classifier = GroupClassifier::new();
        for generation in 1..=4 {
            classifier.update(&procs, &meta, generation);
        }
        assert!(classifier.fact(75, 750).is_none());
    }

    #[test]
    fn mixed_systemd_process_root_does_not_group() {
        let procs = build(vec![
            row(1, 0, 1, b"systemd"),
            row(101, 1, 101, b"daemon-a"),
            row(102, 1, 102, b"daemon-b"),
            row(103, 101, 103, b"worker-a"),
        ]);
        let mut meta = Meta::default();
        meta.cgroups.insert(1, b"0::/init.scope\n".to_vec());
        meta.cgroups
            .insert(101, b"0::/system.slice/a.service\n".to_vec());
        meta.cgroups
            .insert(102, b"0::/system.slice/b.service\n".to_vec());
        meta.cgroups
            .insert(103, b"0::/system.slice/a.service\n".to_vec());

        let mut classifier = GroupClassifier::new();
        for generation in 1..=4 {
            classifier.update(&procs, &meta, generation);
        }

        assert!(classifier.fact(1, 1).is_none());
        let fact = classifier.fact(101, 101).expect("coherent service grouped");
        assert_eq!(fact.rule_id(), "cgroup-systemd");
        assert_eq!(fact.label().bytes(), b"a.service");
    }

    #[test]
    fn single_process_transient_scope_does_not_auto_group() {
        let procs = build(vec![row(70, 0, 700, b"run")]);
        let mut meta = Meta::default();
        meta.cgroups.insert(
            70,
            b"0::/user.slice/user-1000.slice/session-9.scope\n".to_vec(),
        );
        let mut classifier = GroupClassifier::new();
        for generation in 1..=4 {
            classifier.update(&procs, &meta, generation);
        }
        assert!(classifier.fact(70, 700).is_none());
    }

    #[test]
    fn kubernetes_pod_descendants_group_under_pod_label() {
        let procs = build(vec![
            row(80, 0, 800, b"pause"),
            row(81, 80, 801, b"app"),
            row(82, 80, 802, b"sidecar"),
        ]);
        let raw = b"0::/kubepods.slice/kubepods-burstable-pod12345678_90ab_cdef_1234_567890abcdef.slice/cri-containerd-aaaaaaaaaaaaaaaa.scope\n";
        let mut meta = Meta::default();
        for pid in 80..=82 {
            meta.cgroups.insert(pid, raw.to_vec());
        }
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        let fact = classifier.fact(80, 800).expect("pod grouped");
        assert_eq!(fact.rule_id(), "cgroup-container");
        assert_eq!(fact.label().bytes(), b"pod/12345678_90a");
    }

    #[test]
    fn cgroup_container_identity_beats_cmdline_shim_fallback() {
        let procs = build(vec![
            row(90, 0, 900, b"containerd-shim"),
            row(91, 90, 901, b"app"),
        ]);
        let mut meta = Meta::default();
        meta.cmdlines.insert(
            90,
            b"/usr/bin/containerd-shim-runc-v2 -namespace moby -id fallback".to_vec(),
        );
        for pid in 90..=91 {
            meta.cgroups.insert(
                pid,
                b"0::/system.slice/docker-abcdef1234567890.scope\n".to_vec(),
            );
        }
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);
        let fact = classifier.fact(90, 900).expect("container grouped");
        assert_eq!(fact.rule_id(), "cgroup-container");
        assert_eq!(fact.label().bytes(), b"container/abcdef123456");
    }

    #[test]
    fn coherent_cgroup_chain_has_one_canonical_auto_root() {
        const PROCESS_COUNT: u32 = 4_096;
        let mut rows = Vec::with_capacity(PROCESS_COUNT as usize);
        let mut meta = Meta::default();
        for pid in 1..=PROCESS_COUNT {
            rows.push(row(pid, pid.saturating_sub(1), u64::from(pid), b"worker"));
            meta.cgroups
                .insert(pid, b"0::/system.slice/example.service\n".to_vec());
        }
        let procs = build(rows);
        let mut classifier = GroupClassifier::new();
        classifier.update(&procs, &meta, 1);

        assert_eq!(classifier.auto_roots(), &[1]);
        assert!(classifier.fact(1, 1).is_some());
        assert!(
            classifier
                .fact(PROCESS_COUNT, u64::from(PROCESS_COUNT))
                .is_none()
        );
    }
}
