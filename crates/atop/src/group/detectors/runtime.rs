use std::collections::HashMap;

use crate::group::{GroupEvidence, GroupFact, GroupLabel, GroupMetaView, GroupRule, TreeView};

const MIN_RUNTIME_DESCENDANTS: u32 = 3;
const MIN_MATCHING_CHILDREN: usize = 2;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Runtime {
    Python,
    Node,
    Java,
    Dotnet,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct RuntimeIdentity {
    runtime: Runtime,
    entrypoint: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct RuntimePoolRule {
    identities: Vec<Option<RuntimeIdentity>>,
    positions: HashMap<RuntimeIdentity, Vec<usize>>,
}

impl GroupRule for RuntimePoolRule {
    fn prepare(&mut self, tree: &TreeView<'_>, meta: &GroupMetaView<'_>) {
        self.identities.clear();
        self.identities.extend(tree.procs().iter().map(|proc| {
            let runtime = Runtime::from_comm(proc.comm())?;
            let entrypoint = runtime.entrypoint(meta.cmdline(proc))?;
            Some(RuntimeIdentity {
                runtime,
                entrypoint,
            })
        }));
        self.positions.clear();
        for (position, &idx) in tree.preorder().iter().enumerate() {
            if let Some(identity) = &self.identities[idx as usize] {
                self.positions
                    .entry(identity.clone())
                    .or_default()
                    .push(position);
            }
        }
    }

    fn detect(
        &self,
        tree: &TreeView<'_>,
        meta: &GroupMetaView<'_>,
        pid_idx: usize,
    ) -> Option<GroupFact> {
        let root = tree.proc(pid_idx);
        if root.subtree_size < MIN_RUNTIME_DESCENDANTS {
            return None;
        }
        let Some(runtime) = Runtime::from_comm(root.comm()) else {
            return None;
        };

        let matching_children = tree
            .children(pid_idx)
            .filter(|&child_idx| Runtime::from_comm(tree.proc(child_idx).comm()) == Some(runtime))
            .take(MIN_MATCHING_CHILDREN)
            .count();
        if matching_children >= MIN_MATCHING_CHILDREN {
            let identity = self.identities[pid_idx].as_ref()?;
            let positions = self.positions.get(identity)?;
            let start = tree.preorder_pos(pid_idx);
            let end = start + root.subtree_size as usize + 1;
            let matching_descendants = positions.partition_point(|&pos| pos < end)
                - positions.partition_point(|&pos| pos <= start);
            if matching_descendants >= MIN_MATCHING_CHILDREN {
                return Some(GroupFact::new(
                    "runtime-pool",
                    GroupLabel::new(meta.cmdline(root), root.non_ascii),
                    70,
                    GroupEvidence::ProcessControlled,
                ));
            }
        }
        None
    }
}

impl Runtime {
    fn from_comm(comm: &[u8]) -> Option<Self> {
        if comm.starts_with(b"python") {
            Some(Self::Python)
        } else if matches!(comm, b"node" | b"nodejs") {
            Some(Self::Node)
        } else if comm == b"java" {
            Some(Self::Java)
        } else if comm == b"dotnet" {
            Some(Self::Dotnet)
        } else {
            None
        }
    }

    fn entrypoint(self, cmdline: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::Python => python_entrypoint(cmdline),
            Self::Node => script_entrypoint(cmdline, b"node"),
            Self::Java => java_entrypoint(cmdline),
            Self::Dotnet => script_entrypoint(cmdline, b"dotnet"),
        }
    }
}

fn python_entrypoint(cmdline: &[u8]) -> Option<Vec<u8>> {
    let args = args_after_binary(cmdline, b"python")?;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == b"-m" {
            return args.get(i + 1).map(|module| keyed(b"module:", module));
        }
        if !arg.starts_with(b"-") {
            return Some(arg.to_vec());
        }
        i += option_arity(arg);
    }
    None
}

fn java_entrypoint(cmdline: &[u8]) -> Option<Vec<u8>> {
    let args = args_after_binary(cmdline, b"java")?;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == b"-jar" {
            return args.get(i + 1).map(|jar| keyed(b"jar:", jar));
        }
        if !arg.starts_with(b"-") {
            return Some(keyed(b"class:", arg));
        }
        i += option_arity(arg);
    }
    None
}

fn script_entrypoint(cmdline: &[u8], binary: &[u8]) -> Option<Vec<u8>> {
    let args = args_after_binary(cmdline, binary)?;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if !arg.starts_with(b"-") {
            return Some(arg.to_vec());
        }
        i += option_arity(arg);
    }
    None
}

fn args_after_binary<'a>(cmdline: &'a [u8], binary: &[u8]) -> Option<Vec<&'a [u8]>> {
    let mut parts = cmdline.split(|&b| b == b' ').filter(|arg| !arg.is_empty());
    let exe = parts.next()?;
    let base = exe.rsplit(|&b| b == b'/').next().unwrap_or(exe);
    if !base.starts_with(binary) {
        return None;
    }
    Some(parts.collect())
}

fn option_arity(arg: &[u8]) -> usize {
    if matches!(
        arg,
        b"-m" | b"-c" | b"-cp" | b"-classpath" | b"--class-path" | b"-jar" | b"-e" | b"--eval"
    ) {
        2
    } else {
        1
    }
}

fn keyed(prefix: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + value.len());
    out.extend_from_slice(prefix);
    out.extend_from_slice(value);
    out
}
