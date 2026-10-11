//! The rows the flatten pass emits.

use std::ops::Range;

/// What a display row stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A process on its own, or a natively collapsed process (see [`RowShape::Collapsed`]).
    Process { proc_idx: usize },
    /// A folded application group, at its anchor's native position.
    Group { group_idx: usize },
}

/// How much of the tree a row stands for. One type, so a collapsed row always names what it
/// hides and a row that hides nothing cannot claim to be collapsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RowShape {
    /// A process with no children.
    Leaf,
    /// A process whose children follow as their own rows.
    Expanded,
    /// A row standing for whole native subtrees: a folded process (itself the only hidden root)
    /// or a folded group (its visible top members). `hidden` is a slice of the tree's root pool.
    Collapsed { hidden: Range<u32> },
}

pub struct ViewRow {
    pub kind: RowKind,
    /// One more than the nearest emitted ancestor row — assigned by the flatten pass and
    /// nowhere else.
    pub depth: u16,
    pub(super) shape: RowShape,
    pub has_next: bool,
}

impl ViewRow {
    /// Whether the row stands for subtrees it does not show.
    pub fn is_collapsed(&self) -> bool {
        matches!(self.shape, RowShape::Collapsed { .. })
    }

    /// Whether there is anything to expand or collapse: children shown, or hidden.
    pub fn has_children(&self) -> bool {
        !matches!(self.shape, RowShape::Leaf)
    }

    /// The row's slice of the tree's root pool; empty unless collapsed.
    pub(super) fn hidden(&self) -> Range<u32> {
        match &self.shape {
            RowShape::Collapsed { hidden } => hidden.clone(),
            RowShape::Leaf | RowShape::Expanded => 0..0,
        }
    }
}
