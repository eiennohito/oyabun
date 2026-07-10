//! Stable per-process identity — the gather-layer half of process grouping.
//!
//! Grouping is split across a layer boundary: the gather layer answers *which processes belong
//! together and how trustworthy that boundary is* (this module), and the view forms, folds,
//! labels, and aggregates groups from it (`crate::application`). Identity is a pure function of
//! kernel- and process-supplied metadata and never consults user configuration, which is why it
//! lives here; folding policy is user-owned and lives in the view.
//!
//! A cgroup-derived identity is *kernel-owned* — its boundary is stable across runs, so a fold
//! keyed on it may persist; a structural heuristic is *session-only*. Cgroup identity always
//! wins over structural, so a process can never present two competing identities.
//!
//! Recomputation is change-gated (goal #1: work tracks change, not population). The gather
//! table reports a metadata epoch that moves only when a per-PID input actually changes; while
//! it holds steady the resolver reuses its cached identities and a settled desktop does no
//! identity work at all.

mod cgroup;
mod resolver;
mod structural;

pub(crate) use resolver::{
    DesktopApp, IdentityKind, IdentityResolver, ProcMeta, ProcessIdentity, Structural,
    StructuralRole,
};
