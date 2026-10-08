//! The view-side half of process grouping: forms, folds, labels, and aggregates groups from the
//! stable per-process identities produced by [`crate::identity`]. "Application" is one presentation
//! kind over the single underlying notion of a process group; the label, aggregated numbers, fold
//! state, and kind are all presentation and live here.

mod desktop;
mod folds;
mod memory;
mod model;

pub(crate) use desktop::DesktopResolver;
pub(crate) use folds::FoldPreferences;
pub(crate) use memory::MemorySampler;
pub(crate) use model::{AppGroup, AppGroupKey, ApplicationGroups};
