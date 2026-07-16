//! The color vocabulary: what each column's color *means*, in one place.
//!
//! Two kinds of color. **Magnitude** columns (CPU%, PEAK, RSS, NI) map a value through a
//! continuous semantic gradient — the gradient's *shape* encodes what the magnitude means:
//! near-zero recedes, a meaningful level is calm, an alarming level is hot. Discrete bands
//! would lie about where "notable" begins. The one deliberate exception is **exactly-zero
//! CPU**: a truly idle process must read as obviously different from a barely-active one, so
//! zero is a hard step to a dim idle tone and *any* nonzero jumps to a visible floor — the one
//! place a discrete boundary is the honest encoding, because "used nothing" and "used a sliver"
//! are categorically different, not adjacent magnitudes. **Categorical** roles (user, state,
//! tree glyphs, deleted-binary markers) have no ordering to interpolate, so they are named
//! constants.
//!
//! All color is [`Rgb`]; a `None` returned by a per-column rule means "terminal default" (used
//! where a value is deliberately unremarkable, e.g. a multi-threaded thread count). Gradients
//! interpolate with integer math — no float accumulation, cheap enough to run per repainted
//! cell (and the renderer skips unchanged cells, so it rarely does).

use etch::Rgb;

use crate::procs::CapLevel;

// --- neutral tones -------------------------------------------------------------------

/// Near-zero magnitude: barely visible, so an idle process's CPU/RSS recedes.
const FAINT: Rgb = Rgb(72, 76, 88);
/// Muted text: path prefixes, sleeping state, non-own users, kthread names, single threads.
const DIM: Rgb = Rgb(124, 128, 140);

// --- categorical roles ---------------------------------------------------------------

/// A process holding the full capability set — root-equivalent, can do anything.
const CAP_FULL: Rgb = Rgb(205, 132, 214);
/// A process holding some (but not all) capabilities — specific elevated privileges.
const CAP_PARTIAL: Rgb = Rgb(224, 196, 96);
/// A running/active process's state char.
const STATE_ACTIVE: Rgb = Rgb(120, 200, 120);
/// A dead/stopped/blocked state (Z/X/T/t/D) — an anomaly worth a warm tint.
const STATE_STUCK: Rgb = Rgb(230, 120, 90);

/// Tree connectors (`├─ │ └─`): structural, so muted and distinct from content.
pub const TREE: Rgb = Rgb(96, 102, 122);
/// Command basename (the interesting part) — bright so it stands out from its path.
pub const BASENAME: Rgb = Rgb(234, 237, 244);
/// Command path prefix (`/usr/bin/`): context, so dim.
pub const PATH: Rgb = DIM;
/// A kernel thread's bracketed name: dim (kthreads are rarely the focus).
pub const KTHREAD: Rgb = DIM;
/// The running binary was deleted/replaced on disk — alarm (htop's red).
pub const EXE_DELETED: Rgb = Rgb(235, 90, 70);
/// A linked shared library was replaced on disk — warning (e.g. after a system update).
pub const LIB_DELETED: Rgb = Rgb(225, 200, 92);

// --- chrome (system header + footer) -------------------------------------------------

/// Bracket labels and column titles (`CPU[`, `Mem[`, the header row).
pub const LABEL: Rgb = Rgb(92, 182, 202);
/// CPU-bar user share / memory used.
pub const BAR_BUSY: Rgb = Rgb(112, 190, 112);
/// CPU-bar system share / swap used.
pub const BAR_SYS: Rgb = Rgb(216, 96, 82);
/// CPU-bar iowait share.
pub const BAR_IO: Rgb = Rgb(92, 142, 220);
/// Reclaimable page cache in the memory bar.
pub const BAR_CACHE: Rgb = Rgb(212, 192, 102);
/// The Load/Tasks/Uptime info line.
pub const INFO: Rgb = Rgb(198, 204, 214);
/// Footer chrome (keybind hints, brackets).
pub const FOOTER: Rgb = DIM;
/// Footer warning (fd-overflow).
pub const WARN: Rgb = Rgb(225, 200, 92);
/// Footer notice (short-lived count).
pub const NOTICE: Rgb = Rgb(92, 182, 202);

/// The selected row's background — the only row-level color (no state-based row tint).
pub const SELECTION_BG: Rgb = Rgb(54, 58, 70);

// --- per-column color rules ----------------------------------------------------------

/// USER: colored by effective-capability level, not ownership — a process's *privilege* is the
/// interesting axis (a root process that dropped its caps is harmless; a non-root process with
/// `CAP_SYS_ADMIN` is not). Unprivileged is dim, a partial set is a warning, the full
/// (root-equivalent) set is highlighted. The username still shows as the cell's text.
#[must_use]
pub fn caps(level: CapLevel) -> Rgb {
    match level {
        CapLevel::None => DIM,
        CapLevel::Partial => CAP_PARTIAL,
        CapLevel::Full => CAP_FULL,
    }
}

/// S (state) column, colored by the *display* state: R (active) green; D/Z/T/t/X (dead,
/// stopped, blocked) warm; sleeping/idle dim.
#[must_use]
pub fn state(display_state: u8) -> Rgb {
    match display_state {
        b'R' => STATE_ACTIVE,
        b'D' | b'Z' | b'X' | b'T' | b't' => STATE_STUCK,
        _ => DIM,
    }
}

/// THR: a single-threaded process is unremarkable (dim); multi-threaded stands out (default).
#[must_use]
pub fn threads(n: u32) -> Option<Rgb> {
    if n <= 1 { Some(DIM) } else { None }
}

/// NI: diverging from neutral zero. Negative nice (elevated priority) warms toward red;
/// positive nice (yielded priority) cools toward green.
#[must_use]
pub fn nice(ni: i8) -> Rgb {
    interp(NICE_STOPS, i64::from(ni))
}

/// CPU% / PEAK (basis points, 10000 = one full core). Exactly zero (a genuinely idle process)
/// recedes to a dim tone; **any** nonzero value — however tiny — steps hard to a clearly-visible
/// floor, then the gradient warms cool→hot as it climbs (one full core is hot, multi-core is
/// alarm). The zero↔nonzero boundary is a discrete step, *not* a gradient endpoint: "used a
/// sliver" must never blend into "used nothing".
#[must_use]
pub fn cpu(bp: u32) -> Rgb {
    if bp == 0 {
        CPU_IDLE
    } else {
        interp(CPU_STOPS, i64::from(bp))
    }
}

/// RSS (bytes): log-scale, so each perceptual step is roughly a doubling. KB is nothing,
/// MB normal, GB notable, tens of GB heavy — a linear ramp would make MB and GB
/// indistinguishable.
#[must_use]
pub fn rss(bytes: u64) -> Rgb {
    // Position = floor(log2(bytes)); the gradient stops are expressed in the same bit units.
    interp(RSS_STOPS, i64::from(bytes.max(1).ilog2()))
}

// --- gradient machinery --------------------------------------------------------------

/// NI diverging stops (domain −20…19).
const NICE_STOPS: &[(i64, Rgb)] = &[
    (-20, Rgb(232, 110, 90)),
    (0, Rgb(96, 100, 112)),
    (19, Rgb(120, 190, 140)),
];

/// Exactly-zero CPU: a genuinely idle process, dim so it recedes. Deliberately far from the
/// nonzero floor ([`CPU_STOPS`]'s first stop) so 0% and the smallest sliver read as obviously
/// different colors — a hard step, handled in [`cpu`], never interpolated.
const CPU_IDLE: Rgb = Rgb(68, 72, 84);

/// CPU% stops in basis points, for **nonzero** usage only (0 is [`CPU_IDLE`], stepped in
/// [`cpu`]). The first stop is a clearly-visible green so even 0.01% pops against idle; the
/// ramp then warms through yellow to a red alarm past one full core.
const CPU_STOPS: &[(i64, Rgb)] = &[
    (1, Rgb(120, 200, 130)),    // any activity: clearly visible (green)
    (2000, Rgb(160, 205, 115)), // 20% yellow-green
    (6000, Rgb(210, 198, 108)), // 60% yellow
    (10000, Rgb(234, 176, 92)), // 100% (one core) amber
    (20000, Rgb(238, 120, 70)), // 200% orange
    (40000, Rgb(240, 80, 68)),  // ≥400% red alarm
];

/// RSS stops in log2(bytes) positions (bit index of the high bit).
const RSS_STOPS: &[(i64, Rgb)] = &[
    (0, FAINT),
    (18, FAINT),              // ≤256 KiB recedes
    (20, Rgb(96, 136, 110)),  // 1 MiB muted green
    (26, Rgb(120, 185, 120)), // 64 MiB green
    (30, Rgb(215, 195, 110)), // 1 GiB yellow
    (34, Rgb(232, 140, 80)),  // 16 GiB orange
    (37, Rgb(238, 90, 70)),   // ≥128 GiB red
];

/// Interpolate an ascending-by-position stop table at `pos`, clamping past the ends.
fn interp(stops: &[(i64, Rgb)], pos: i64) -> Rgb {
    let first = stops[0];
    if pos <= first.0 {
        return first.1;
    }
    let last = stops[stops.len() - 1];
    if pos >= last.0 {
        return last.1;
    }
    for w in stops.windows(2) {
        let (lo, hi) = (w[0], w[1]);
        if pos <= hi.0 {
            // `pos ∈ (lo.0, hi.0]` here, so `num ∈ [0, den]` and both are non-negative.
            return lerp(lo.1, hi.1, pos - lo.0, (hi.0 - lo.0).max(1));
        }
    }
    last.1
}

/// Linear blend `a → b` at fraction `num/den` (`0 ≤ num ≤ den`), per channel, integer math.
fn lerp(a: Rgb, b: Rgb, num: i64, den: i64) -> Rgb {
    // Each blended channel lands in `[0, 255]` by construction; the clamp is belt-and-braces.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mix = |x: u8, y: u8| -> u8 {
        let (x, y) = (i64::from(x), i64::from(y));
        (x + (y - x) * num / den).clamp(0, 255) as u8
    };
    Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_clamps_and_hits_anchors() {
        assert_eq!(cpu(0), CPU_IDLE, "exactly zero recedes");
        assert_eq!(
            cpu(10000),
            Rgb(234, 176, 92),
            "one core hits the amber anchor"
        );
        assert_eq!(
            cpu(u32::MAX),
            Rgb(240, 80, 68),
            "beyond the top clamps to alarm"
        );
    }

    #[test]
    fn zero_cpu_is_a_hard_step_from_the_smallest_sliver() {
        // The whole point: 0% and any nonzero must be obviously different colors, not adjacent
        // points on a gradient. The smallest representable nonzero (0.01%) already jumps to the
        // visible floor.
        let idle = cpu(0);
        let sliver = cpu(1);
        assert_ne!(idle, sliver, "0% and 0.01% must not share a color");
        // A large per-channel distance, not the ~1-unit step a gradient would give.
        let dist = i32::from(idle.0.abs_diff(sliver.0))
            + i32::from(idle.1.abs_diff(sliver.1))
            + i32::from(idle.2.abs_diff(sliver.2));
        assert!(
            dist > 120,
            "zero↔sliver must be a bold step, got L1 distance {dist}"
        );
    }

    #[test]
    fn gradient_is_monotone_between_anchors() {
        // Rising CPU% must not step backwards on the red channel across the warming half.
        let r = |bp| cpu(bp).0;
        assert!(r(6000) <= r(10000) && r(10000) <= r(20000));
    }

    #[test]
    fn nice_diverges_from_zero() {
        assert_eq!(nice(0), Rgb(96, 100, 112), "zero is neutral");
        assert!(
            nice(-20).0 > nice(0).0,
            "negative nice is warmer (more red)"
        );
        assert!(
            nice(19).1 > nice(0).1,
            "positive nice is cooler (more green)"
        );
    }

    #[test]
    fn rss_is_log_scale() {
        assert_eq!(rss(0), FAINT);
        assert_eq!(rss(4096), FAINT, "a few pages still recede");
        // 1 GiB must be visibly warmer than 1 MiB (yellow vs muted green).
        assert!(rss(1 << 30).0 > rss(1 << 20).0);
    }

    #[test]
    fn cap_roles() {
        assert_eq!(caps(CapLevel::None), DIM, "unprivileged is dim");
        assert_eq!(caps(CapLevel::Partial), CAP_PARTIAL, "partial is a warning");
        assert_eq!(caps(CapLevel::Full), CAP_FULL, "full caps highlighted");
    }

    #[test]
    fn state_colors() {
        assert_eq!(state(b'R'), STATE_ACTIVE);
        assert_eq!(state(b'Z'), STATE_STUCK);
        assert_eq!(state(b'S'), DIM);
    }
}
