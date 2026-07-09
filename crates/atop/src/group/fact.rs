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
}

impl GroupFact {
    pub(crate) fn new(rule_id: &'static str, label: GroupLabel) -> Self {
        Self { rule_id, label }
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GroupCandidate {
    pid_idx: usize,
    rule_id: &'static str,
}

impl GroupCandidate {
    pub(crate) fn new(pid_idx: usize, rule_id: &'static str) -> Self {
        Self { pid_idx, rule_id }
    }

    #[must_use]
    pub(crate) fn pid_idx(&self) -> usize {
        self.pid_idx
    }

    #[must_use]
    pub(crate) fn rule_id(&self) -> &'static str {
        self.rule_id
    }
}
