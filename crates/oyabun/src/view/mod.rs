//! The display tree: one model for "collapsed", flattened to rows.
//!
//! A collapsed row (a folded process or a folded application group) stands for a set of whole
//! native subtrees; its numbers are exactly the sum over what it hides, and every process is
//! either its own row or hidden in exactly one collapsed row. The native tree comes from the
//! gatherer, grouping from [`crate::application`], and the user's collapse choices from
//! [`Collapse`]; this module turns them into display rows, per-row totals, and the history
//! series of collapsed rows.
//!
//! - [`collapse`] — user collapse/fold state, with a version for rebuild gating.
//! - [`row`] — the row kinds the flatten pass emits (depth is assigned there, only there).
//! - [`tree`] — [`ViewTree`]: flatten pass, sibling order, per-cycle refresh.
//! - [`totals`] — per-process totals and what a row adds up.
//! - [`series`] — CPU history of collapsed rows, kept equal to the sum of their members'.

mod collapse;
mod row;
mod series;
mod totals;
mod tree;

pub(crate) use collapse::Collapse;
pub use row::{RowKind, ViewRow};
pub(crate) use totals::Totals;
pub(crate) use tree::ViewTree;
