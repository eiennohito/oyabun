#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GroupLabel {
    bytes: Vec<u8>,
    non_ascii: bool,
}

impl GroupLabel {
    pub(crate) fn new(bytes: &[u8], non_ascii: bool) -> Self {
        Self {
            bytes: bytes.to_vec(),
            non_ascii: non_ascii || bytes.iter().any(|&b| b >= 0x80),
        }
    }

    #[must_use]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub(crate) fn non_ascii(&self) -> bool {
        self.non_ascii
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GroupFact {
    rule_id: &'static str,
    label: GroupLabel,
    rank: u8,
    evidence: GroupEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupEvidence {
    /// Membership and the grouping boundary come from the kernel's cgroup hierarchy.
    KernelCgroup,
    /// The identity depends on process-controlled names, argv, or root-filesystem content.
    ProcessControlled,
}

impl GroupFact {
    pub(crate) fn new(
        rule_id: &'static str,
        label: GroupLabel,
        rank: u8,
        evidence: GroupEvidence,
    ) -> Self {
        Self {
            rule_id,
            label,
            rank,
            evidence,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn rule_id(&self) -> &'static str {
        self.rule_id
    }

    #[must_use]
    pub(crate) fn label(&self) -> &GroupLabel {
        &self.label
    }

    #[must_use]
    pub(crate) fn rank(&self) -> u8 {
        self.rank
    }

    #[must_use]
    pub(crate) fn auto_collapse(&self) -> bool {
        self.evidence == GroupEvidence::KernelCgroup
    }
}
