//! A fixed-capacity wraparound ring — the storage and index arithmetic shared by both CPU
//! samplers, so the "overwrite the oldest, track head/len, find the k-th-recent or the oldest"
//! bookkeeping is written (and tested) once instead of open-coded twice.
//!
//! It is deliberately just storage: it holds no notion of *what* the samples mean. Each user
//! layers its own aggregation on top — [`CpuRing`](super::cpu) keeps running sums plus an
//! incremental peak, [`SystemSampler`](super::sysstat) differences the two window endpoints.
//! Physical slot indices (`0..N`) are exposed on purpose: `CpuRing` caches the index of its
//! peak sample, and that must stay stable as the head advances, so the ring never renumbers
//! live slots — a push overwrites exactly the slot the previous `head` pointed at.

/// A ring of at most `N` `T`s. `Copy` so it embeds in a `Copy` owner (e.g. a huge-page-resident
/// `CpuRing`); `T: Default` seeds the unused slots.
#[derive(Clone, Copy)]
pub(super) struct Ring<T, const N: usize> {
    slots: [T; N],
    /// Physical index the next push overwrites (and, when full, the current oldest slot).
    head: usize,
    /// Live element count (`< N` only while the ring is still filling after construction).
    len: usize,
}

impl<T: Copy + Default, const N: usize> Ring<T, N> {
    pub(super) fn new() -> Self {
        Self {
            slots: [T::default(); N],
            head: 0,
            len: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// The physical slot the next [`push`](Self::push) will overwrite. `CpuRing` reads this
    /// *before* pushing to decide whether the sample being evicted was its cached peak.
    pub(super) fn write_pos(&self) -> usize {
        self.head
    }

    /// Value at a physical slot index (`0..N`). Callers pass indices obtained from
    /// [`write_pos`](Self::write_pos), [`recent_index`](Self::recent_index), or a `0..len` scan.
    pub(super) fn slot(&self, i: usize) -> T {
        self.slots[i]
    }

    /// Push `v`, returning the evicted oldest value when the ring was already full (so the
    /// caller can back it out of a running aggregate), else `None`.
    pub(super) fn push(&mut self, v: T) -> Option<T> {
        let evicted = if self.len == N {
            Some(self.slots[self.head])
        } else {
            self.len += 1;
            None
        };
        self.slots[self.head] = v;
        self.head = (self.head + 1) % N;
        evicted
    }

    /// Physical index of the `k`-th most recent element (`k = 1` is newest), or `None` when
    /// `k` exceeds the live count.
    pub(super) fn recent_index(&self, k: usize) -> Option<usize> {
        (1..=self.len).contains(&k).then(|| (self.head + N - k) % N)
    }

    /// The oldest live value (the far endpoint of the window), or `None` when empty.
    pub(super) fn oldest(&self) -> Option<T> {
        (self.len > 0).then(|| self.slots[(self.head + N - self.len) % N])
    }
}

#[cfg(test)]
mod tests {
    use super::Ring;

    #[test]
    fn fills_then_evicts_oldest() {
        let mut r: Ring<u32, 3> = Ring::new();
        assert_eq!(r.len(), 0);
        assert_eq!(r.oldest(), None);
        assert_eq!(r.push(10), None);
        assert_eq!(r.push(20), None);
        assert_eq!(r.push(30), None); // now full: [10,20,30]
        assert_eq!(r.len(), 3);
        assert_eq!(r.oldest(), Some(10));
        assert_eq!(r.push(40), Some(10)); // evicts the oldest
        assert_eq!(r.oldest(), Some(20));
        assert_eq!(r.push(50), Some(20));
        assert_eq!(r.oldest(), Some(30));
    }

    #[test]
    fn recent_index_counts_back_from_newest() {
        let mut r: Ring<u32, 4> = Ring::new();
        for v in [1, 2, 3] {
            r.push(v);
        }
        // newest is 3, then 2, then 1; nothing older.
        assert_eq!(r.recent_index(1).map(|i| r.slot(i)), Some(3));
        assert_eq!(r.recent_index(2).map(|i| r.slot(i)), Some(2));
        assert_eq!(r.recent_index(3).map(|i| r.slot(i)), Some(1));
        assert_eq!(r.recent_index(4), None);
    }

    #[test]
    fn write_pos_is_the_slot_push_overwrites() {
        let mut r: Ring<u32, 2> = Ring::new();
        let p0 = r.write_pos();
        r.push(7);
        assert_eq!(r.slot(p0), 7, "push wrote exactly the reported slot");
        let p1 = r.write_pos();
        r.push(8);
        assert_eq!(r.slot(p1), 8);
        // Full now; the next write_pos wraps back to p0 (the oldest).
        assert_eq!(r.write_pos(), p0);
    }
}
