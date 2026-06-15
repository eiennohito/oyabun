//! Zero-copy `/proc/<pid>/stat` parsing.
//!
//! Operates directly on the raw bytes the kernel wrote into the arena. The `comm`
//! field is recorded as a [`StringRef`] into that same buffer (no copy); numeric
//! fields are parsed from bytes without UTF-8 validation.

use crate::arena::StringRef;
use crate::snapshot::ProcessEntry;

pub struct StatFields {
    pub ppid: u32,
    pub state: u8,
    pub priority: i8,
    pub nice: i8,
    pub num_threads: u32,
    /// `utime + stime` (jiffies).
    pub ticks: u64,
    /// Start time (jiffies since boot) — stat field 22.
    pub start_time: u64,
    pub rss_pages: u64,
    pub comm: StringRef,
}

impl StatFields {
    /// Populate a process entry from these parsed fields plus the file-owner `uid`.
    /// `cpu_pct`/`cpu_peak` and the tree links are filled by later stages; `pid` was
    /// set when the tombstone was pushed.
    pub fn write_into(&self, e: &mut ProcessEntry, uid: u32, page_size: u64) {
        e.ppid = self.ppid;
        e.state = self.state;
        e.priority = self.priority;
        e.nice = self.nice;
        e.num_threads = self.num_threads;
        e.ticks = self.ticks;
        e.start_time = self.start_time;
        e.mem_bytes = self.rss_pages.saturating_mul(page_size);
        e.name = self.comm;
        e.uid = uid;
    }
}

/// Parse one stat record. `slot_offset` is the byte offset of `slot[0]` within the
/// arena, so the returned `comm` `StringRef` is absolute.
///
/// `comm` is wrapped in parens and may contain spaces/parens; we take everything
/// between the first `(` and last `)`. Numeric field indices (0-based, after `) `):
/// 0=state 1=ppid 11=utime 12=stime 15=priority 16=nice 17=`num_threads`
/// 19=starttime 21=rss(pages).
// `open`/`close` are positions within a slot bounded by SLOT_SIZE (≤ 2 KiB), so
// the offset/len casts cannot truncate.
#[allow(clippy::cast_possible_truncation)]
pub fn parse_stat(slot: &[u8], slot_offset: u32) -> Option<StatFields> {
    let open = slot.iter().position(|&b| b == b'(')?;
    let close = slot.iter().rposition(|&b| b == b')')?;
    if close <= open + 1 {
        return None;
    }
    let comm = StringRef {
        offset: slot_offset + (open + 1) as u32,
        len: (close - open - 1) as u32,
    };

    // Fields are single-space separated; ") " precedes the state char.
    let rest = slot.get(close + 2..)?;
    let mut it = rest.split(|&b| b == b' ');

    let state = *it.next()?.first()?; // 0
    let ppid = u32::try_from(parse_u64(it.next()?)?).ok()?; // 1
    skip(&mut it, 9)?; // 2..=10
    let utime = parse_u64(it.next()?)?; // 11
    let stime = parse_u64(it.next()?)?; // 12
    skip(&mut it, 2)?; // 13..=14
    let priority = parse_i8(it.next()?)?; // 15
    let nice = parse_i8(it.next()?)?; // 16
    let num_threads = u32::try_from(parse_u64(it.next()?)?).ok()?; // 17
    it.next()?; // 18 (itrealvalue)
    let start_time = parse_u64(it.next()?)?; // 19
    it.next()?; // 20 (vsize)
    let rss_pages = parse_u64(it.next()?)?; // 21

    Some(StatFields {
        ppid,
        state,
        priority,
        nice,
        num_threads,
        ticks: utime.saturating_add(stime),
        start_time,
        rss_pages,
        comm,
    })
}

fn skip<'a>(it: &mut impl Iterator<Item = &'a [u8]>, n: usize) -> Option<()> {
    for _ in 0..n {
        it.next()?;
    }
    Some(())
}

/// Parse a signed decimal field (e.g. nice: −20…19) into an `i8`.
fn parse_i8(b: &[u8]) -> Option<i8> {
    if b.is_empty() {
        return None;
    }
    let (neg, digits) = if b[0] == b'-' {
        (true, &b[1..])
    } else {
        (false, b)
    };
    let mag = parse_u64(digits)?;
    let val = if neg {
        i8::try_from(mag).ok().and_then(i8::checked_neg)?
    } else {
        i8::try_from(mag).ok()?
    };
    Some(val)
}

/// Process raw `/proc/<pid>/cmdline` bytes in-place: replace NUL separators with
/// spaces and trim trailing whitespace/NULs. Returns the usable byte length.
pub fn clean_cmdline(buf: &mut [u8]) -> u32 {
    // Strip trailing NULs.
    let mut end = buf.len();
    while end > 0 && buf[end - 1] == 0 {
        end -= 1;
    }
    // Replace internal NULs with spaces.
    for b in &mut buf[..end] {
        if *b == 0 {
            *b = b' ';
        }
    }
    u32::try_from(end).unwrap_or(u32::MAX)
}

fn parse_u64(b: &[u8]) -> Option<u64> {
    if b.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_record() {
        let raw = b"1234 (bash) S 1000 1234 1234 0 -1 4194560 100 200 0 0 7 8 0 0 20 0 1 0 999 12345 678 18446744073709551615 1 1 0 0\n";
        let f = parse_stat(raw, 0).expect("parse");
        assert_eq!(f.ppid, 1000);
        assert_eq!(f.state, b'S');
        assert_eq!(f.priority, 20);
        assert_eq!(f.nice, 0);
        assert_eq!(f.num_threads, 1);
        assert_eq!(f.ticks, 7 + 8);
        assert_eq!(f.start_time, 999);
        assert_eq!(f.rss_pages, 678);
        // comm offset points at 'b' in "(bash)" → index 6.
        assert_eq!(
            &raw[f.comm.offset as usize..(f.comm.offset + f.comm.len) as usize],
            b"bash"
        );
    }

    #[test]
    fn parses_negative_nice() {
        // nice = -10, priority = 10, num_threads = 4
        let raw = b"99 (cc1) R 50 99 99 0 -1 0 0 0 0 0 100 50 0 0 10 -10 4 0 500 0 1024 0 0\n";
        let f = parse_stat(raw, 0).expect("parse");
        assert_eq!(f.priority, 10);
        assert_eq!(f.nice, -10);
        assert_eq!(f.num_threads, 4);
        assert_eq!(f.ticks, 150);
    }

    #[test]
    fn comm_with_spaces_and_parens() {
        // comm = "Web Content (tab)"
        let raw =
            b"42 (Web Content (tab)) R 7 42 42 0 -1 0 0 0 0 0 11 22 0 0 20 0 1 0 0 0 4096 0 0\n";
        let f = parse_stat(raw, 0).expect("parse");
        assert_eq!(f.ppid, 7);
        assert_eq!(f.state, b'R');
        assert_eq!(f.ticks, 33);
        let name = &raw[f.comm.offset as usize..(f.comm.offset + f.comm.len) as usize];
        assert_eq!(name, b"Web Content (tab)");
    }

    #[test]
    fn slot_offset_makes_comm_absolute() {
        let raw = b"5 (x) S 1 5 5 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 4096 0 0\n";
        let f = parse_stat(raw, 1000).expect("parse");
        assert_eq!(f.comm.offset, 1000 + 3); // '(' at index 2, comm 'x' at 3
        assert_eq!(f.comm.len, 1);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_stat(b"not a stat line", 0).is_none());
        assert!(parse_stat(b"", 0).is_none());
    }

    #[test]
    fn clean_cmdline_replaces_nuls_and_trims() {
        let mut buf = *b"/usr/bin/foo\0--bar\0--baz\0";
        let len = clean_cmdline(&mut buf);
        assert_eq!(&buf[..len as usize], b"/usr/bin/foo --bar --baz");
    }

    #[test]
    fn clean_cmdline_empty() {
        let mut buf = [0u8; 4];
        assert_eq!(clean_cmdline(&mut buf), 0);
        assert_eq!(clean_cmdline(&mut []), 0);
    }
}
