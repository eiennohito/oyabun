//! Structural (session-only) process identity: heuristics over tree shape and process-controlled
//! names/argv, for multi-process applications with no stable cgroup boundary — a Chromium/Electron
//! process fan launched outside `app.slice`, a language-runtime worker pool, a container shim.
//!
//! These have no cross-run key, so their folds never persist (see [`IdentityKind::persists`]).
//! Detection is stateless within a rebuild: a root is recognized from the current tree and its
//! whole subtree is stamped as one group, skipping any member that already carries a
//! (higher-precedence) cgroup identity. Because a fresh tree deterministically yields the same
//! groups, a startup transient self-corrects on the next rebuild with no settling window.

use super::{IdentityKind, ProcMeta, ProcessIdentity, Structural, StructuralRole};
use crate::fxhash::FxMap;
use crate::procs::{NONE, ProcessEntry};

/// Real Chromium/Electron roots quickly grow zygote/GPU/utility/renderer/crashpad descendants;
/// five is a conservative fan-out floor that avoids collapsing tiny helper pairs.
const MIN_CHROMIUM_DESCENDANTS: u32 = 5;
/// Several same-`comm` immediate children before paying the argv precision check — one or two is
/// too common among generic worker pools.
const MIN_SHARED_COMM_CHILDREN: u32 = 3;
const MIN_RUNTIME_DESCENDANTS: u32 = 3;
const MIN_MATCHING_CHILDREN: usize = 2;

#[derive(Default)]
pub(super) struct StructuralDetector {
    subtree_has_type: Vec<bool>,
    runtime_ids: Vec<Option<RuntimeIdentity>>,
    runtime_positions: FxMap<RuntimeIdentity, Vec<usize>>,
}

impl StructuralDetector {
    /// Stamp structural identities into `identities` for subtrees whose root matches a heuristic
    /// and whose members have no cgroup identity. `preorder_pos[i]` is the preorder rank of
    /// process `i`; `preorder` lists process indices in that order.
    pub(super) fn detect(
        &mut self,
        procs: &[ProcessEntry],
        preorder: &[u32],
        preorder_pos: &[usize],
        meta: &dyn ProcMeta,
        identities: &mut [Option<ProcessIdentity>],
    ) {
        self.prepare_chromium(procs, preorder, meta);
        self.prepare_runtime(procs, preorder, meta);

        for &root in preorder {
            let root = root as usize;
            if identities[root].is_some() {
                continue;
            }
            let Some((role, label)) = self.detect_root(procs, preorder_pos, meta, root) else {
                continue;
            };
            let proc = &procs[root];
            let identity = ProcessIdentity {
                owner_uid: proc.uid,
                kind: IdentityKind::Structural(Structural {
                    role,
                    // A per-incarnation token: session-only, evicted when the root exits/reuses.
                    token: format!("structural\0{}\0{}", proc.pid, proc.start_time),
                    label,
                }),
            };
            stamp_subtree(procs, preorder, preorder_pos, root, &identity, identities);
        }
    }

    fn detect_root(
        &self,
        procs: &[ProcessEntry],
        preorder_pos: &[usize],
        meta: &dyn ProcMeta,
        root: usize,
    ) -> Option<(StructuralRole, String)> {
        self.chromium(procs, root, meta)
            .or_else(|| self.runtime_pool(procs, preorder_pos, root, meta))
            .or_else(|| container_shim(procs, root, meta))
    }

    fn prepare_chromium(&mut self, procs: &[ProcessEntry], preorder: &[u32], meta: &dyn ProcMeta) {
        self.subtree_has_type.clear();
        self.subtree_has_type.resize(procs.len(), false);
        for (idx, p) in procs.iter().enumerate() {
            self.subtree_has_type[idx] = contains_arg_prefix(meta.cmdline(p), b"--type=");
        }
        for &idx in preorder.iter().rev() {
            let idx = idx as usize;
            let parent = procs[idx].parent_idx;
            if parent != NONE && self.subtree_has_type[idx] {
                self.subtree_has_type[parent as usize] = true;
            }
        }
    }

    fn chromium(
        &self,
        procs: &[ProcessEntry],
        root: usize,
        meta: &dyn ProcMeta,
    ) -> Option<(StructuralRole, String)> {
        let proc = &procs[root];
        if proc.subtree_size < MIN_CHROMIUM_DESCENDANTS {
            return None;
        }
        let mut shared_comm = 0;
        let mut has_type_descendant = false;
        let mut child = proc.first_child;
        while child != NONE {
            let c = child as usize;
            if procs[c].comm() == proc.comm() {
                shared_comm += 1;
            }
            has_type_descendant |= self.subtree_has_type[c];
            child = procs[c].next_sibling;
        }
        (shared_comm >= MIN_SHARED_COMM_CHILDREN && has_type_descendant)
            .then(|| (StructuralRole::Chromium, root_label(procs, root, meta)))
    }

    fn prepare_runtime(&mut self, procs: &[ProcessEntry], preorder: &[u32], meta: &dyn ProcMeta) {
        self.runtime_ids.clear();
        self.runtime_ids.extend(procs.iter().map(|proc| {
            let runtime = Runtime::from_comm(proc.comm())?;
            Some(RuntimeIdentity {
                runtime,
                entrypoint: runtime.entrypoint(meta.cmdline(proc))?,
            })
        }));
        self.runtime_positions.clear();
        for (position, &idx) in preorder.iter().enumerate() {
            if let Some(identity) = &self.runtime_ids[idx as usize] {
                self.runtime_positions
                    .entry(identity.clone())
                    .or_default()
                    .push(position);
            }
        }
    }

    fn runtime_pool(
        &self,
        procs: &[ProcessEntry],
        preorder_pos: &[usize],
        root: usize,
        meta: &dyn ProcMeta,
    ) -> Option<(StructuralRole, String)> {
        let proc = &procs[root];
        if proc.subtree_size < MIN_RUNTIME_DESCENDANTS {
            return None;
        }
        let runtime = Runtime::from_comm(proc.comm())?;
        let matching_children = children(procs, root)
            .filter(|&c| Runtime::from_comm(procs[c].comm()) == Some(runtime))
            .take(MIN_MATCHING_CHILDREN)
            .count();
        if matching_children < MIN_MATCHING_CHILDREN {
            return None;
        }
        let identity = self.runtime_ids[root].as_ref()?;
        let positions = self.runtime_positions.get(identity)?;
        let start = preorder_pos[root];
        let end = start + proc.subtree_size as usize + 1;
        let matching_descendants =
            positions.partition_point(|&p| p < end) - positions.partition_point(|&p| p <= start);
        (matching_descendants >= MIN_MATCHING_CHILDREN)
            .then(|| (StructuralRole::RuntimePool, root_label(procs, root, meta)))
    }
}

/// A structural group's label is its root command (or `comm` when the command is unavailable).
fn root_label(procs: &[ProcessEntry], root: usize, meta: &dyn ProcMeta) -> String {
    let proc = &procs[root];
    let cmdline = meta.cmdline(proc);
    let bytes = if cmdline.is_empty() {
        proc.comm()
    } else {
        cmdline
    };
    String::from_utf8_lossy(bytes).into_owned()
}

fn container_shim(
    procs: &[ProcessEntry],
    root: usize,
    meta: &dyn ProcMeta,
) -> Option<(StructuralRole, String)> {
    let proc = &procs[root];
    if proc.subtree_size == 0 || !is_container_shim_comm(proc.comm()) {
        return None;
    }
    let cmdline = meta.cmdline(proc);
    let has_id = [
        b"-id".as_slice(),
        b"--id",
        b"-container-id",
        b"--container-id",
    ]
    .iter()
    .any(|needle| cmdline.split(|&b| b == b' ').any(|arg| arg == *needle));
    (!cmdline.is_empty() && has_id).then(|| {
        (
            StructuralRole::ContainerShim,
            String::from_utf8_lossy(cmdline).into_owned(),
        )
    })
}

fn is_container_shim_comm(comm: &[u8]) -> bool {
    matches!(comm, b"containerd-shim" | b"conmon" | b"docker-init")
}

/// A structural group is the root's whole subtree — it spans `start ..= start + subtree_size` in
/// preorder. Stamp every member that has no (higher-precedence) cgroup identity.
fn stamp_subtree(
    procs: &[ProcessEntry],
    preorder: &[u32],
    preorder_pos: &[usize],
    root: usize,
    identity: &ProcessIdentity,
    identities: &mut [Option<ProcessIdentity>],
) {
    let start = preorder_pos[root];
    let last = start + procs[root].subtree_size as usize;
    for &member in &preorder[start..=last] {
        let member = member as usize;
        if identities[member].is_none() {
            identities[member] = Some(identity.clone());
        }
    }
}

fn children(procs: &[ProcessEntry], parent: usize) -> impl Iterator<Item = usize> + '_ {
    let mut next = procs[parent].first_child;
    std::iter::from_fn(move || {
        if next == NONE {
            return None;
        }
        let idx = next as usize;
        next = procs[idx].next_sibling;
        Some(idx)
    })
}

fn contains_arg_prefix(cmdline: &[u8], prefix: &[u8]) -> bool {
    cmdline
        .split(|&b| b == b' ')
        .any(|arg| arg.starts_with(prefix))
}

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
