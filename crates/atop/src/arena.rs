//! Arena-style backing store for raw `/proc` text and string fields.
//!
//! A single mmap-backed buffer is both the I/O target (the gatherer reads `/proc`
//! files directly into it) and the storage for string fields (process names point
//! into it via [`StringRef`]). It resets in O(1) each refresh — no per-cycle
//! allocation after warmup.

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

/// 2 MiB — transparent-huge-page size on x86-64/aarch64. Allocations round up to
/// this so the buffer is THP-eligible.
const HUGE_PAGE: usize = 2 * 1024 * 1024;

/// mmap-backed, huge-page-hinted byte arena with a bump cursor.
///
/// Mutated only by the gatherer while it holds unique access to the owning
/// snapshot; shared access (UI render) is read-only. Hence the `Send`/`Sync`
/// impls below are sound.
pub struct HugePageBuf {
    ptr: NonNull<u8>,
    capacity: usize,
    cursor: usize,
}

// SAFETY: the buffer is owned exclusively by its `Snapshot`. The gatherer mutates
// it only while holding `Arc::get_mut` (strong count 1); once published via ArcSwap
// it is read-only for all observers. No aliased mutation crosses threads.
unsafe impl Send for HugePageBuf {}
unsafe impl Sync for HugePageBuf {}

fn round_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

fn map(capacity: usize) -> NonNull<u8> {
    // SAFETY: standard anonymous private mapping; null hint, valid flags.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            capacity,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(ptr != libc::MAP_FAILED, "mmap failed for {capacity} bytes");

    #[cfg(target_os = "linux")]
    // SAFETY: ptr/len come from the successful mmap above.
    unsafe {
        // Best-effort: ignore failure (e.g. THP disabled by sysctl).
        libc::madvise(ptr, capacity, libc::MADV_HUGEPAGE);
    }

    NonNull::new(ptr.cast()).expect("mmap returned non-null")
}

impl HugePageBuf {
    pub fn new(min_capacity: usize) -> Self {
        let capacity = round_up(min_capacity.max(HUGE_PAGE), HUGE_PAGE);
        Self {
            ptr: map(capacity),
            capacity,
            cursor: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Drop all stored data, retaining the mapping. O(1).
    pub fn reset(&mut self) {
        self.cursor = 0;
    }

    /// Ensure room for `needed` total bytes, growing (re-mmap + copy) if short.
    /// Returns `true` if the base pointer moved — callers holding the old base
    /// (e.g. an `io_uring` registered buffer) must re-register.
    ///
    /// Call before filling, never while reads are in flight against the buffer.
    pub fn reserve(&mut self, needed: usize) -> bool {
        if needed <= self.capacity {
            return false;
        }
        let mut new_cap = self.capacity;
        while new_cap < needed {
            new_cap *= 2;
        }
        new_cap = round_up(new_cap, HUGE_PAGE);

        let new_ptr = map(new_cap);
        // SAFETY: both mappings are valid for `cursor` bytes; regions don't overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), new_ptr.as_ptr(), self.cursor);
            libc::munmap(self.ptr.as_ptr().cast(), self.capacity);
        }
        self.ptr = new_ptr;
        self.capacity = new_cap;
        true
    }

    /// Bump-allocate `len` bytes, returning the starting offset. Grows if needed —
    /// safe only when no external reads target the buffer (use [`reserve`] up front
    /// for the `io_uring` path).
    ///
    /// [`reserve`]: Self::reserve
    pub fn alloc(&mut self, len: usize) -> usize {
        // The gatherer `reserve()`s the whole cycle up front, so alloc must never
        // grow here — an io_uring read may be targeting this mapping. The runtime
        // grow stays as a release-mode safety net.
        debug_assert!(
            self.cursor + len <= self.capacity,
            "alloc grew mid-fill; reserve() the cycle up front"
        );
        if self.cursor + len > self.capacity {
            self.reserve(self.cursor + len);
        }
        let off = self.cursor;
        self.cursor += len;
        off
    }

    /// Raw write pointer at `offset`. Caller guarantees `offset + len <= capacity`.
    pub fn write_ptr(&mut self, offset: usize) -> *mut u8 {
        debug_assert!(offset <= self.capacity);
        // SAFETY: offset bounded by capacity per the debug_assert / caller contract.
        unsafe { self.ptr.as_ptr().add(offset) }
    }

    pub fn bytes(&self, offset: u32, len: u32) -> &[u8] {
        let (offset, len) = (offset as usize, len as usize);
        assert!(offset + len <= self.capacity, "StringRef out of bounds");
        // SAFETY: range bounded by capacity (checked above); bytes were written before publish.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) }
    }

    /// Mutable byte slice — only valid while the gatherer holds unique access.
    pub fn bytes_mut(&mut self, offset: u32, len: u32) -> &mut [u8] {
        let (offset, len) = (offset as usize, len as usize);
        assert!(offset + len <= self.capacity, "StringRef out of bounds");
        // SAFETY: range bounded by capacity (checked above); exclusive access via &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(offset), len) }
    }

    pub fn get(&self, sref: StringRef) -> &[u8] {
        self.bytes(sref.offset, sref.len)
    }
}

impl Drop for HugePageBuf {
    fn drop(&mut self) {
        // SAFETY: ptr/capacity from our own mmap; dropped exactly once.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.capacity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_and_read_back() {
        let mut buf = HugePageBuf::new(0);
        assert!(buf.capacity() >= HUGE_PAGE);

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
        assert!(grew);
        assert!(buf.capacity() >= HUGE_PAGE * 3);
        assert_eq!(
            buf.get(StringRef {
                offset: u32::try_from(off).unwrap(),
                len: 4
            }),
            b"keep",
            "grow must preserve already-written bytes"
        );
    }
}
