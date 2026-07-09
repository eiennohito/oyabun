use super::detectors;
use super::{GroupCandidate, GroupFact, GroupMetaView, GroupRule, TreeView};
use crate::fxhash::PidMap;
use crate::procs::ProcessEntry;

const GROUP_SETTLE_GENS: u64 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
enum GroupState {
    Settling {
        start_time: u64,
        first_seen_gen: u64,
        last_seen_gen: u64,
    },
    NoGroup {
        start_time: u64,
        last_seen_gen: u64,
    },
    Group {
        start_time: u64,
        last_seen_gen: u64,
        fact: GroupFact,
    },
}

pub(crate) trait GroupMetaSource {
    fn cmdline(&self, e: &ProcessEntry) -> &[u8];
}

pub(crate) struct GroupClassifier {
    states: PidMap<GroupState>,
    rules: Vec<Box<dyn GroupRule>>,
    candidates: Vec<GroupCandidate>,
}

impl GroupClassifier {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            states: PidMap::default(),
            rules: detectors::default_rules(),
            candidates: Vec::new(),
        }
    }

    pub(crate) fn update(
        &mut self,
        procs: &[ProcessEntry],
        meta: &dyn GroupMetaSource,
        cur_gen: u64,
    ) {
        let tree = TreeView::new(procs);
        let meta = GroupMetaView::new(procs, meta);

        for (pid_idx, p) in procs.iter().enumerate() {
            let state = self
                .states
                .entry(p.pid)
                .and_modify(|state| {
                    if state.start_time() == p.start_time {
                        state.set_last_seen_gen(cur_gen);
                    } else {
                        *state = GroupState::Settling {
                            start_time: p.start_time,
                            first_seen_gen: cur_gen,
                            last_seen_gen: cur_gen,
                        };
                    }
                })
                .or_insert(GroupState::Settling {
                    start_time: p.start_time,
                    first_seen_gen: cur_gen,
                    last_seen_gen: cur_gen,
                });

            let GroupState::Settling {
                start_time,
                first_seen_gen,
                ..
            } = state
            else {
                continue;
            };

            let start_time = *start_time;
            let first_seen_gen = *first_seen_gen;
            self.candidates.clear();
            for rule in &self.rules {
                rule.nominate(&tree, pid_idx, &mut self.candidates);
            }
            let mut fact = None;
            for candidate in &self.candidates {
                if let Some(resolved) = self
                    .rules
                    .iter()
                    .find(|rule| rule.id() == candidate.rule_id())
                    .and_then(|rule| rule.resolve(&meta, candidate))
                {
                    fact = Some(resolved);
                    break;
                }
            }

            *state = if let Some(fact) = fact {
                GroupState::Group {
                    start_time,
                    last_seen_gen: cur_gen,
                    fact,
                }
            } else if cur_gen.wrapping_sub(first_seen_gen) >= GROUP_SETTLE_GENS {
                GroupState::NoGroup {
                    start_time,
                    last_seen_gen: cur_gen,
                }
            } else {
                GroupState::Settling {
                    start_time,
                    first_seen_gen,
                    last_seen_gen: cur_gen,
                }
            };
        }

        self.states
            .retain(|_, state| state.last_seen_gen() == cur_gen);
    }

    #[must_use]
    pub(crate) fn fact(&self, pid: u32, start_time: u64) -> Option<&GroupFact> {
        match self.states.get(&pid) {
            Some(GroupState::Group {
                start_time: state_start,
                fact,
                ..
            }) if *state_start == start_time => Some(fact),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn contains_pid(&self, pid: u32) -> bool {
        self.states.contains_key(&pid)
    }
}

impl GroupState {
    fn start_time(&self) -> u64 {
        match self {
            Self::Settling { start_time, .. }
            | Self::NoGroup { start_time, .. }
            | Self::Group { start_time, .. } => *start_time,
        }
    }

    fn last_seen_gen(&self) -> u64 {
        match self {
            Self::Settling { last_seen_gen, .. }
            | Self::NoGroup { last_seen_gen, .. }
            | Self::Group { last_seen_gen, .. } => *last_seen_gen,
        }
    }

    fn set_last_seen_gen(&mut self, cur_gen: u64) {
        match self {
            Self::Settling { last_seen_gen, .. }
            | Self::NoGroup { last_seen_gen, .. }
            | Self::Group { last_seen_gen, .. } => *last_seen_gen = cur_gen,
        }
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
    }

    impl GroupMetaSource for Meta {
        fn cmdline(&self, e: &ProcessEntry) -> &[u8] {
            self.cmdlines.get(&e.pid).map_or(&[], Vec::as_slice)
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
}
