//! The snapshot's bump-cursor string arena (`comm` + cmdline bytes).
//!
//! [`HugePageBuf`] layers a bump cursor on [`thoop::MmapRegion`] to store string fields
//! (process names point into it via [`StringRef`]). It resets in O(1) each refresh — no
//! per-cycle allocation after warmup.
//!
//! This is the pre-generational string store: every cycle re-materializes each PID's
//! strings into a freshly-reset arena. The THP-arena work replaces it with generational
//! `GenStore` string slots that survive across cycles (no per-cycle re-copy); until then
//! `HugePageBuf` remains the snapshot's `strings` field.

use thoop::MmapRegion;

/// A slice of the [`HugePageBuf`], identified by byte offset and length.
///
/// Stored on [`ProcessEntry`](crate::snapshot::ProcessEntry) instead of a `String`
/// so process records stay POD (no per-field heap allocation, no drop on reset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringRef {
    pub offset: u32,
    pub len: u32,
}

impl StringRef {
    pub const EMPTY: StringRef = StringRef { offset: 0, len: 0 };
}

/// Huge-page-hinted byte arena with a bump cursor — the snapshot's string store.
///
/// Mutated only by the gatherer while it holds unique access to the owning snapshot;
/// shared access (UI render) is read-only. As of the landing-pad split it is **not**
/// an `io_uring` target, so a mid-cycle grow no longer races in-flight reads — the
/// reserve-up-front rule the gatherer follows is now a performance choice, not a
/// correctness requirement.
pub struct HugePageBuf {
    region: MmapRegion,
    cursor: usize,
}

impl HugePageBuf {
    pub fn new(min_capacity: usize) -> Self {
        Self {
            region: MmapRegion::huge(min_capacity),
            cursor: 0,
        }
    }

    /// Drop all stored data, retaining the mapping. O(1).
    pub fn reset(&mut self) {
        self.cursor = 0;
    }

    /// Ensure room for `needed` total bytes, growing (re-mmap + copy) if short.
    /// Returns `true` if the base pointer moved.
    pub fn reserve(&mut self, needed: usize) -> bool {
        if needed <= self.region.len() {
            return false;
        }
        // The doubling terminates because `region.len() >= HUGE_PAGE > 0` by construction
        // (`MmapRegion::huge` floors at one huge page) — so `new_cap` is never 0 and each
        // step makes progress. A future change that lets `region.len()` reach 0 would hang.
        let mut new_cap = self.region.len();
        while new_cap < needed {
            new_cap *= 2;
        }
        let new = MmapRegion::huge(new_cap);
        // SAFETY: both mappings are valid for `cursor` bytes; regions don't overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(self.region.as_ptr(), new.as_ptr(), self.cursor);
        }
        self.region = new; // old region drops here → munmap
        true
    }

    /// Bump-allocate `len` bytes, returning the starting offset.
    pub fn alloc(&mut self, len: usize) -> usize {
        if self.cursor + len > self.region.len() {
            self.reserve(self.cursor + len);
        }
        let off = self.cursor;
        self.cursor += len;
        off
    }

    /// Raw write pointer at `offset`. Caller guarantees `offset + len <= capacity`.
    pub fn write_ptr(&mut self, offset: usize) -> *mut u8 {
        self.region.write_ptr(offset)
    }

    pub fn bytes(&self, offset: u32, len: u32) -> &[u8] {
        self.region.bytes(offset as usize, len as usize)
    }

    pub fn get(&self, sref: StringRef) -> &[u8] {
        self.bytes(sref.offset, sref.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thoop::HUGE_PAGE;

    #[test]
    fn alloc_and_read_back() {
        let mut buf = HugePageBuf::new(0);

        let off = buf.alloc(5);
        let p = buf.write_ptr(off);
        // SAFETY: 5 bytes allocated at off.
        unsafe { std::ptr::copy_nonoverlapping(b"hello".as_ptr(), p, 5) };

        let sref = StringRef {
            offset: u32::try_from(off).unwrap(),
            len: 5,
        };
        assert_eq!(buf.get(sref), b"hello");
    }

    #[test]
    fn reset_rewinds_cursor() {
        let mut buf = HugePageBuf::new(0);
        let a = buf.alloc(100);
        buf.reset();
        let b = buf.alloc(100);
        assert_eq!(a, b, "reset should rewind the bump cursor");
    }

    #[test]
    fn reserve_grows_and_preserves_data() {
        let mut buf = HugePageBuf::new(HUGE_PAGE);
        let off = buf.alloc(4);
        // SAFETY: 4 bytes allocated.
        unsafe { std::ptr::copy_nonoverlapping(b"keep".as_ptr(), buf.write_ptr(off), 4) };

        let grew = buf.reserve(HUGE_PAGE * 3);
        assert!(
            grew,
            "reserve past capacity must grow (and move) the mapping"
        );
        // Data preserved across the grow, and the new offset is still addressable.
        assert_eq!(
            buf.get(StringRef {
                offset: u32::try_from(off).unwrap(),
                len: 4
            }),
            b"keep",
            "grow must preserve already-written bytes"
        );
        let tail = buf.alloc(HUGE_PAGE * 2); // fits only if the mapping actually grew
        assert!(tail >= 4);
    }
}
