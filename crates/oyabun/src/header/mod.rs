//! The system header's CPU and memory lines.
//!
//! **Widgets never move except on a terminal resize.** A bar or graph whose position or length
//! follows the current numbers reads as data changing when nothing changed: a memory reading
//! crossing from `900.0M` to `1.2G` must not shove the swap bar sideways, and `wait 3%` becoming
//! `12%` must not rescale the CPU history. So the header's geometry — which slots exist, where
//! each starts, how many cells each bar and the graph get — is decided once per terminal width
//! by [`HeaderLayout`] and reused every frame. Values only ever change the *content* of a slot,
//! never the slot.
//!
//! **Structure is frozen with the layout.** What the layout may read besides the width are
//! structural facts: how many cores, whether the kernel offers pressure stalls for each
//! resource, whether swap exists, whether the machine accounts steal at all, the memory and
//! swap totals. They are sampled when the layout is built; if one changes mid-run (swap turned
//! on) it takes effect at the next resize rather than by moving widgets.
//!
//! **Conditional items own a fixed slot.** Steal, swap-in rate, Shmem, IO wait, D and Z are
//! leads worth showing only sometimes, but their place is reserved and left blank while
//! irrelevant: nothing shifts when one appears. A line that cannot hold every slot drops the
//! lowest-priority ones, decided by width and frozen structure alone. Priority, highest first:
//! memory numbers and bar; memory pressure; swap; swap-in rate; IO wait; D; Z; Shmem. On the
//! CPU line: cores and the graph; wait; sys; steal. The bars are guaranteed a share of the
//! width before the optional slots take any, so narrow terminals still get a memory bar.
//!
//! The memory line carries a memory bar and a swap bar (length = usage, color = pressure, the
//! CPU graph's grammar). The CPU line ends in a graph that takes all the width the slots leave;
//! its cell count is exposed so the history can size its visible window (and scale) to what is
//! really drawn.
//!
//! **One description, three readers.** Each line's text is written once, against a [`Sink`];
//! measuring its width, hashing it into the repaint key, and emitting it are different sinks
//! over the same description. The key therefore cannot drift from what is drawn, and laying out
//! and keying (every frame) allocate nothing.

mod cpu_line;
mod fields;
mod layout;
mod mem_line;
mod sink;
#[cfg(test)]
mod testing;

pub(crate) use cpu_line::CpuLine;
pub(crate) use layout::HeaderLayout;
pub(crate) use mem_line::MemLine;
