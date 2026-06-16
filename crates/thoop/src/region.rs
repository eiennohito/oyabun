//! The owned-`mmap` primitive every other abstraction builds on.

use std::ptr::NonNull;

/// 2 MiB — the transparent-huge-page size on x86-64/aarch64. THP-eligible allocations
/// round up to this.
pub const HUGE_PAGE: usize = 2 * 1024 * 1024;

fn round_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// An owned anonymous `mmap` region with a stable base pointer, unmapped on drop.
///
/// Rounds up to a 2 MiB huge page and hints THP, cutting TLB misses on the large,
/// randomly-accessed structures it backs. When only part of a region should be *pinned*,
/// the caller registers a sub-range rather than the whole mapping.
///
/// The base pointer is stable for the region's life — a region never relocates — so
/// callers may hand out byte offsets into it as durable handles.
pub struct MmapRegion {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: a region is a plain owned allocation exposing only a raw pointer + length.
// Aliasing/threading discipline (mutation under unique access, shared access read-only)
// is upheld by the owner.
unsafe impl Send for MmapRegion {}
unsafe impl Sync for MmapRegion {}

impl MmapRegion {
    /// Map at least `min_len` bytes, rounded up to a 2 MiB huge page, and hint THP.
    ///
    /// # Panics
    /// If the underlying `mmap` fails (e.g. the address space is exhausted).
    #[must_use]
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

    #[must_use]
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Raw write pointer at `offset`. Caller guarantees the subsequent write stays within
    /// `len()`. A runtime `assert` (not `debug_assert`) — this pointer becomes an
    /// `io_uring ReadFixed` destination and a memcpy target, so a bad offset would be a
    /// silent out-of-bounds write; the one compare is negligible next to the I/O it guards.
    ///
    /// # Panics
    /// If `offset` exceeds the region length.
    #[must_use]
    pub fn write_ptr(&self, offset: usize) -> *mut u8 {
        assert!(offset <= self.len, "MmapRegion write_ptr out of bounds");
        // SAFETY: offset bounded by len (checked above); caller bounds the write length.
        unsafe { self.ptr.as_ptr().add(offset) }
    }

    /// Read `len` bytes at `offset` as a slice.
    ///
    /// # Panics
    /// If `offset + len` exceeds the region length.
    #[must_use]
    pub fn bytes(&self, offset: usize, len: usize) -> &[u8] {
        assert!(offset + len <= self.len, "MmapRegion read out of bounds");
        // SAFETY: range bounded by len (checked above).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) }
    }

    /// Mutable slice of `len` bytes at `offset` (e.g. an `io_uring` read destination,
    /// or transient read scratch).
    ///
    /// # Panics
    /// If `offset + len` exceeds the region length.
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

#[cfg(test)]
mod tests {
    use super::*;

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
