//! Memory regions for raw `/proc` text and string fields.
//!
//! [`MmapRegion`] is the low-level owned-`mmap` primitive. [`HugePageBuf`] layers a
//! bump cursor on top of it to store string fields (process names point into it via
//! [`StringRef`]). It resets in O(1) each refresh — no per-cycle allocation after
//! warmup. The gatherer's `io_uring` landing pad is a second, small, fixed
//! `MmapRegion`; future work routes the gatherer's persistent huge-page-backed
//! structures (CPU history, PID caches) through the same primitive.

use std::ptr::NonNull;

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

/// 2 MiB — transparent-huge-page size on x86-64/aarch64. THP-eligible allocations
/// round up to this.
const HUGE_PAGE: usize = 2 * 1024 * 1024;

fn round_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// An owned anonymous `mmap` region with a stable base pointer, unmapped on drop.
///
/// The shared memory primitive: [`HugePageBuf`] (the string store) and the gatherer's
/// `io_uring` landing pad both build on it. It rounds up to a 2 MiB huge page and hints
/// THP, cutting TLB misses on the large, randomly-accessed structures it backs. When only
/// part of a region should be *pinned*, the caller registers a sub-range rather than the
/// whole mapping (the landing pad registers just its read-slot prefix and uses the free
/// huge-page tail for unpinned scratch).
pub struct MmapRegion {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: a region is a plain owned allocation exposing only a raw pointer + length.
// Aliasing/threading discipline (mutation under unique access, shared access
// read-only) is upheld by the owner — the same contract `HugePageBuf` documents.
unsafe impl Send for MmapRegion {}
unsafe impl Sync for MmapRegion {}

impl MmapRegion {
    /// Map at least `min_len` bytes, rounded up to a 2 MiB huge page, and hint THP.
    pub fn huge(min_len: usize) -> Self {
        let len = round_up(min_len.max(HUGE_PAGE), HUGE_PAGE);
        // SAFETY: standard anonymous private mapping; null hint, valid flags, len > 0.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(ptr != libc::MAP_FAILED, "mmap failed for {len} bytes");

        #[cfg(target_os = "linux")]
        // SAFETY: ptr/len come from the successful mmap above.
        // Best-effort: ignore failure (e.g. THP disabled by policy).
        unsafe {
            libc::madvise(ptr, len, libc::MADV_HUGEPAGE);
        }

        Self {
            ptr: NonNull::new(ptr.cast()).expect("mmap returned non-null"),
            len,
        }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Raw write pointer at `offset`. Caller guarantees the subsequent write stays within
    /// `len()`. A runtime `assert` (not `debug_assert`) — this pointer becomes an
    /// `io_uring ReadFixed` destination and an arena memcpy target, so a bad offset would be
    /// silent out-of-bounds writes; the one compare is negligible next to the I/O it guards.
    pub fn write_ptr(&self, offset: usize) -> *mut u8 {
        assert!(offset <= self.len, "MmapRegion write_ptr out of bounds");
        // SAFETY: offset bounded by len (checked above); caller bounds the write length.
        unsafe { self.ptr.as_ptr().add(offset) }
    }

    /// Read `len` bytes at `offset` as a slice.
    pub fn bytes(&self, offset: usize, len: usize) -> &[u8] {
        assert!(offset + len <= self.len, "MmapRegion read out of bounds");
        // SAFETY: range bounded by len (checked above).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) }
    }

    /// Mutable slice of `len` bytes at `offset` (e.g. an `io_uring` read destination,
    /// or transient read scratch).
    pub fn slice_mut(&mut self, offset: usize, len: usize) -> &mut [u8] {
        assert!(offset + len <= self.len, "MmapRegion write out of bounds");
        // SAFETY: range bounded by len (checked above); &mut self gives unique access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(offset), len) }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // SAFETY: ptr/len from our own mmap; dropped exactly once.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
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

    #[test]
    fn region_rounding() {
        // `huge` rounds up to a whole huge page so the mapping is THP-eligible.
        assert_eq!(MmapRegion::huge(0).len(), HUGE_PAGE);
        assert_eq!(MmapRegion::huge(512 * 1024).len(), HUGE_PAGE);
        assert_eq!(MmapRegion::huge(HUGE_PAGE + 1).len(), HUGE_PAGE * 2);
    }

    #[test]
    fn region_slice_mut_round_trips() {
        let mut r = MmapRegion::huge(4096);
        r.slice_mut(0, 5).copy_from_slice(b"hello");
        assert_eq!(r.bytes(0, 5), b"hello");
    }
}
