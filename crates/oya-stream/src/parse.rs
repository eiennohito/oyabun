use std::collections::BTreeMap;

use crate::{
    CycleEvent, ParseError, ProcMetadata, ProcSnapshot, ProcState, RawProc, Stream, SystemStats,
    default_sys,
};

pub(crate) fn parse(input: &str) -> Result<Stream, ParseError> {
    Parser::new(input).parse()
}

struct Parser<'a> {
    input: &'a str,
    state: BTreeMap<u32, ResolvedProc>,
    sys: SystemStats,
    wall_ns: u64,
    saw_cycle: bool,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            state: BTreeMap::new(),
            sys: default_sys(),
            wall_ns: 0,
            saw_cycle: false,
        }
    }

    fn parse(mut self) -> Result<Stream, ParseError> {
        let mut cycles = Vec::new();
        let mut current: Option<PendingCycle> = None;

        for (idx, raw_line) in self.input.lines().enumerate() {
            let line_no = idx + 1;
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            let tokens = tokenize(line, line_no)?;
            if tokens.first().is_some_and(|t| t == "cycle") {
                if let Some(pending) = current.take() {
                    cycles.push(self.finish_cycle(pending));
                }
                current = Some(self.start_cycle(&tokens, line_no)?);
            } else {
                let Some(pending) = current.as_mut() else {
                    return Err(ParseError::new(line_no, "process event before first cycle"));
                };
                self.apply_proc_event(pending, &tokens, line_no)?;
            }
        }

        if let Some(pending) = current.take() {
            cycles.push(self.finish_cycle(pending));
        }
        if cycles.is_empty() {
            return Err(ParseError::new(1, "stream contains no cycles"));
        }
        Ok(Stream { cycles })
    }

    fn start_cycle(
        &mut self,
        tokens: &[String],
        line_no: usize,
    ) -> Result<PendingCycle, ParseError> {
        if tokens.len() < 2 {
            return Err(ParseError::new(line_no, "cycle requires a time"));
        }
        let time = &tokens[1];
        self.wall_ns = if let Some(rest) = time.strip_prefix('+') {
            self.wall_ns
                .checked_add(parse_duration_ns(rest, line_no)?)
                .ok_or_else(|| ParseError::new(line_no, "cycle time overflow"))?
        } else {
            parse_duration_ns(time, line_no)?
        };
        for token in &tokens[2..] {
            let (key, value) = split_field(token, line_no)?;
            self.sys.apply_field(key, value, line_no)?;
        }
        let first = !self.saw_cycle;
        self.saw_cycle = true;
        Ok(PendingCycle { first })
    }

    fn apply_proc_event(
        &mut self,
        pending: &mut PendingCycle,
        tokens: &[String],
        line_no: usize,
    ) -> Result<(), ParseError> {
        let (op, pid_idx) = match tokens.first().map(String::as_str) {
            Some("+") => (ProcOp::Birth, 1),
            Some("-") => (ProcOp::Death, 1),
            Some(_) => (ProcOp::Update, 0),
            None => return Err(ParseError::new(line_no, "empty process event")),
        };
        let pid_token = tokens
            .get(pid_idx)
            .ok_or_else(|| ParseError::new(line_no, "process event requires a pid"))?;
        let pid = parse_u32(pid_token, line_no)?;

        if op == ProcOp::Death {
            if tokens.len() != pid_idx + 1 {
                return Err(ParseError::new(
                    line_no,
                    "death event does not accept fields",
                ));
            }
            self.state.remove(&pid);
            return Ok(());
        }

        let is_birth = op == ProcOp::Birth || (pending.first && !self.state.contains_key(&pid));
        if !is_birth && !self.state.contains_key(&pid) {
            return Err(ParseError::new(line_no, format!("unknown pid {pid}")));
        }
        let mut proc = if is_birth {
            ResolvedProc {
                raw: RawProc::defaults(pid),
                metadata: ProcMetadata::default(),
            }
        } else {
            self.state
                .get(&pid)
                .cloned()
                .expect("known pid checked above")
        };

        for token in &tokens[pid_idx + 1..] {
            if let Some(state) = parse_bare_state(token) {
                proc.raw.state = state;
                continue;
            }
            let (key, value) = split_field(token, line_no)?;
            proc.apply_field(key, value, line_no)?;
        }
        self.state.insert(pid, proc);
        Ok(())
    }

    fn finish_cycle(&self, _pending: PendingCycle) -> CycleEvent {
        let mut procs = Vec::with_capacity(self.state.len());
        for state in self.state.values() {
            procs.push(ProcSnapshot {
                raw: state.raw.clone(),
                metadata: state.metadata.clone(),
            });
        }
        CycleEvent {
            wall_ns: self.wall_ns,
            sys: self.sys,
            procs,
        }
    }
}

#[derive(Clone)]
struct ResolvedProc {
    raw: RawProc,
    metadata: ProcMetadata,
}

impl ResolvedProc {
    fn apply_field(&mut self, key: &str, value: &str, line_no: usize) -> Result<(), ParseError> {
        match key {
            "ppid" => self.raw.ppid = parse_u32(value, line_no)?,
            "uid" => self.raw.uid = parse_u32(value, line_no)?,
            "state" => self.raw.state = parse_state(value, line_no)?,
            "priority" | "prio" => self.raw.priority = parse_i8(value, line_no)?,
            "nice" => self.raw.nice = parse_i8(value, line_no)?,
            "threads" | "num_threads" => self.raw.num_threads = parse_u32(value, line_no)?,
            "ticks" => {
                if let Some(delta) = value.strip_prefix('+') {
                    self.raw.ticks = self
                        .raw
                        .ticks
                        .checked_add(parse_u64(delta, line_no)?)
                        .ok_or_else(|| ParseError::new(line_no, "ticks overflow"))?;
                } else {
                    self.raw.ticks = parse_u64(value, line_no)?;
                }
            }
            // Naming a wait makes it measured (counting from zero); `none` makes it unmeasured
            // again (values carry across cycles, so omission alone could not say so).
            "wait" => self.raw.wait = apply_delta_present(self.raw.wait, value, line_no)?,
            "mem" | "mem_bytes" => self.raw.mem_bytes = parse_bytes(value, line_no)?,
            "start" | "start_time" => self.raw.start_time = parse_u64(value, line_no)?,
            "comm" => self.raw.comm = value.as_bytes().to_vec(),
            "cmd" | "cmdline" => self.metadata.cmdline = Some(value.as_bytes().to_vec()),
            "cgroup" => self.metadata.cgroup = Some(value.as_bytes().to_vec()),
            "flatpak" => self.metadata.flatpak = Some(value.as_bytes().to_vec()),
            "kthread" | "is_kthread" => self.raw.is_kthread = parse_bool(value, line_no)?,
            _ => {
                return Err(ParseError::new(
                    line_no,
                    format!("unknown process field {key:?}"),
                ));
            }
        }
        Ok(())
    }
}

impl SystemStats {
    fn apply_field(&mut self, key: &str, value: &str, line_no: usize) -> Result<(), ParseError> {
        match key {
            "cores" | "num_cores" => self.num_cores = parse_u32(value, line_no)?,
            // Raw cumulative /proc/stat CPU jiffies — support +delta syntax for readability.
            "stat_user" => self.stat_user = apply_delta_u64(self.stat_user, value, line_no)?,
            "stat_nice" => self.stat_nice = apply_delta_u64(self.stat_nice, value, line_no)?,
            "stat_system" => {
                self.stat_system = apply_delta_u64(self.stat_system, value, line_no)?;
            }
            "stat_idle" => self.stat_idle = apply_delta_u64(self.stat_idle, value, line_no)?,
            "stat_iowait" => {
                self.stat_iowait = apply_delta_u64(self.stat_iowait, value, line_no)?;
            }
            "stat_irq" => self.stat_irq = apply_delta_u64(self.stat_irq, value, line_no)?,
            "stat_softirq" => {
                self.stat_softirq = apply_delta_u64(self.stat_softirq, value, line_no)?;
            }
            "stat_steal" => self.stat_steal = apply_delta_u64(self.stat_steal, value, line_no)?,
            // Raw /proc/meminfo fields (bytes).
            "mem" | "mem_total" => self.mem_total = parse_bytes(value, line_no)?,
            "mem_available" => self.mem_available = parse_bytes(value, line_no)?,
            "mem_free" => self.mem_free = parse_bytes(value, line_no)?,
            "mem_shmem" => self.mem_shmem = parse_bytes(value, line_no)?,
            "swap_total" => self.swap_total = parse_bytes(value, line_no)?,
            "swap_free" => self.swap_free = parse_bytes(value, line_no)?,
            "runnable" => self.runnable = parse_u32(value, line_no)?,
            // Naming a pressure file makes it present (counting from zero); unnamed = absent.
            "psi_cpu" => self.psi_cpu = apply_delta_present(self.psi_cpu, value, line_no)?,
            "psi_memory" => self.psi_memory = apply_delta_present(self.psi_memory, value, line_no)?,
            "psi_io" => self.psi_io = apply_delta_present(self.psi_io, value, line_no)?,
            "pswpin" => self.pswpin = apply_delta_present(self.pswpin, value, line_no)?,
            // Fields older recordings carried and the header no longer shows: accepted so those
            // recordings still replay, and ignored.
            "mem_buffers" | "mem_cached" | "load" | "load1" | "load5" | "load15" | "uptime"
            | "uptime_secs" => {}
            _ => {
                return Err(ParseError::new(
                    line_no,
                    format!("unknown system field {key:?}"),
                ));
            }
        }
        Ok(())
    }
}

/// [`apply_delta_u64`] for a counter that may be absent: naming it makes it present, and the
/// literal `none` makes it absent again (values carry across cycles, so omission alone could
/// not express "present before, gone now").
fn apply_delta_present(
    current: Option<u64>,
    value: &str,
    line_no: usize,
) -> Result<Option<u64>, ParseError> {
    if value == "none" {
        return Ok(None);
    }
    apply_delta_u64(current.unwrap_or(0), value, line_no).map(Some)
}

/// Parse `value` as either an absolute u64 or `+delta` added to `current`.
fn apply_delta_u64(current: u64, value: &str, line_no: usize) -> Result<u64, ParseError> {
    if let Some(delta) = value.strip_prefix('+') {
        current
            .checked_add(parse_u64(delta, line_no)?)
            .ok_or_else(|| ParseError::new(line_no, "stat counter overflow"))
    } else {
        parse_u64(value, line_no)
    }
}

struct PendingCycle {
    first: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcOp {
    Birth,
    Death,
    Update,
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn tokenize(line: &str, line_no: usize) -> Result<Vec<String>, ParseError> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if quoted {
            match ch {
                '"' => quoted = false,
                '\\' => cur.push(parse_escape(&mut chars, line_no)?),
                _ => cur.push(ch),
            }
        } else {
            match ch {
                '"' => quoted = true,
                ch if ch.is_whitespace() => {
                    if !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(ch),
            }
        }
    }
    if quoted {
        return Err(ParseError::new(line_no, "unterminated quoted value"));
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    Ok(tokens)
}

fn parse_escape(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    line_no: usize,
) -> Result<char, ParseError> {
    let Some(ch) = chars.next() else {
        return Err(ParseError::new(line_no, "trailing escape"));
    };
    match ch {
        'n' => Ok('\n'),
        'r' => Ok('\r'),
        't' => Ok('\t'),
        '\\' | '"' => Ok(ch),
        'x' => {
            let hi = chars
                .next()
                .ok_or_else(|| ParseError::new(line_no, "short hex escape"))?;
            let lo = chars
                .next()
                .ok_or_else(|| ParseError::new(line_no, "short hex escape"))?;
            let hex = format!("{hi}{lo}");
            let byte = u8::from_str_radix(&hex, 16)
                .map_err(|_| ParseError::new(line_no, "invalid hex escape"))?;
            Ok(char::from(byte))
        }
        _ => Err(ParseError::new(line_no, format!("unknown escape \\{ch}"))),
    }
}

fn split_field(token: &str, line_no: usize) -> Result<(&str, &str), ParseError> {
    token
        .split_once('=')
        .ok_or_else(|| ParseError::new(line_no, format!("expected key=value, got {token:?}")))
}

fn parse_bare_state(token: &str) -> Option<ProcState> {
    (token.len() == 1)
        .then(|| ProcState::try_from(token.as_bytes()[0]).ok())
        .flatten()
}

fn parse_state(value: &str, line_no: usize) -> Result<ProcState, ParseError> {
    parse_bare_state(value)
        .ok_or_else(|| ParseError::new(line_no, format!("invalid state {value:?}")))
}

fn parse_bool(value: &str, line_no: usize) -> Result<bool, ParseError> {
    match value {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(ParseError::new(line_no, format!("invalid bool {value:?}"))),
    }
}

fn parse_duration_ns(value: &str, line_no: usize) -> Result<u64, ParseError> {
    let (num, scale) = split_unit(value);
    let n = parse_u64(num, line_no)?;
    let scale = match scale {
        "" | "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        _ => {
            return Err(ParseError::new(
                line_no,
                format!("invalid time unit {scale:?}"),
            ));
        }
    };
    n.checked_mul(scale)
        .ok_or_else(|| ParseError::new(line_no, "time overflow"))
}

fn parse_bytes(value: &str, line_no: usize) -> Result<u64, ParseError> {
    let (num, scale) = split_unit(value);
    let n = parse_u64(num, line_no)?;
    let scale = match scale {
        "" => 1,
        "K" | "k" => 1024,
        "M" | "m" => 1024 * 1024,
        "G" | "g" => 1024 * 1024 * 1024,
        _ => {
            return Err(ParseError::new(
                line_no,
                format!("invalid memory unit {scale:?}"),
            ));
        }
    };
    n.checked_mul(scale)
        .ok_or_else(|| ParseError::new(line_no, "memory overflow"))
}

fn split_unit(value: &str) -> (&str, &str) {
    let idx = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    (&value[..idx], &value[idx..])
}

fn parse_u64(value: &str, line_no: usize) -> Result<u64, ParseError> {
    value
        .parse()
        .map_err(|_| ParseError::new(line_no, format!("invalid unsigned integer {value:?}")))
}

fn parse_u32(value: &str, line_no: usize) -> Result<u32, ParseError> {
    value
        .parse()
        .map_err(|_| ParseError::new(line_no, format!("invalid u32 {value:?}")))
}

fn parse_i8(value: &str, line_no: usize) -> Result<i8, ParseError> {
    value
        .parse()
        .map_err(|_| ParseError::new(line_no, format!("invalid i8 {value:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inheritance_birth_death_and_relative_ticks() {
        let stream = Stream::parse(
            r"
            # initial snapshot
            cycle 0 cores=4 mem=8G
              1 ppid=0 uid=0 S comm=init cmd=/sbin/init ticks=100 mem=12M
              42 ppid=1 S comm=bash cmd=/bin/bash mem=3M

            cycle +1s
              42 ticks=+50
              + 43 ppid=1 R comm=cc1

            cycle +500ms
              - 43
            ",
        )
        .expect("parse");

        assert_eq!(stream.cycles.len(), 3);
        assert_eq!(stream.cycles[1].wall_ns, 1_000_000_000);
        assert_eq!(stream.cycles[2].wall_ns, 1_500_000_000);
        assert_eq!(stream.cycles[1].procs[1].raw.ticks, 50);
        assert_eq!(stream.cycles[1].procs[2].raw.pid, 43);
        assert_eq!(stream.cycles[2].procs.len(), 2);
    }

    #[test]
    fn parses_quoted_values_and_writer_round_trips_verbose_state() {
        let stream = Stream::parse(
            r#"
            cycle 0 runnable=3 psi_cpu=100
              7 comm="space name" cmd="/usr/bin/space name --flag" mem=1K
            "#,
        )
        .expect("parse");

        let text = stream.to_verbose_dsl();
        assert!(text.contains("cmd=\"/usr/bin/space name --flag\""));
        let reparsed = Stream::parse(&text).expect("reparse");
        assert_eq!(stream, reparsed);
    }

    #[test]
    fn legacy_system_fields_are_accepted_and_ignored() {
        let stream = Stream::parse(
            "cycle 0 mem=8G mem_buffers=1M mem_cached=2M load=125,75,50 load1=1 load5=2 \
             load15=3 uptime=3600 uptime_secs=1\n",
        )
        .expect("old recordings still parse");
        assert_eq!(stream.cycles[0].sys.mem_total, 8 << 30);
    }

    #[test]
    fn absence_replays_as_absence() {
        let mut stream = Stream::parse("cycle 0 psi_cpu=5 pswpin=9\ncycle +1s\n").unwrap();
        assert_eq!(stream.cycles[1].sys.psi_cpu, Some(5), "omitted = carried");
        stream.cycles[1].sys.psi_cpu = None;
        stream.cycles[1].sys.pswpin = None;
        let again = Stream::parse(&stream.to_verbose_dsl()).unwrap();
        assert_eq!(again, stream, "a value that disappears stays gone");
        assert_eq!(again.cycles[1].sys.pswpin, None);
    }

    #[test]
    fn wait_is_cumulative_carried_and_can_become_unmeasured() {
        let mut stream = Stream::parse(
            "cycle 0\n  7 ticks=10 wait=5\n  8 ticks=1\ncycle +1s\n  7 wait=+3\n  8 wait=+2\n\
             cycle +1s\n  7 wait=none\n",
        )
        .unwrap();
        let wait = |c: usize, i: usize| stream.cycles[c].procs[i].raw.wait;
        assert_eq!(
            (wait(0, 0), wait(0, 1)),
            (Some(5), None),
            "unnamed = unmeasured"
        );
        assert_eq!(
            (wait(1, 0), wait(1, 1)),
            (Some(8), Some(2)),
            "+delta; naming starts at 0"
        );
        assert_eq!(wait(2, 0), None, "`none` ends measurement");
        assert_eq!(wait(2, 1), Some(2), "carried when omitted");
        let again = Stream::parse(&stream.to_verbose_dsl()).unwrap();
        assert_eq!(
            again, stream,
            "a wait that stops being measured stays unmeasured"
        );
        stream.cycles[2].procs[1].raw.wait = None;
        let again = Stream::parse(&stream.to_verbose_dsl()).unwrap();
        assert_eq!(again, stream);
    }

    #[test]
    fn rejects_unknown_update() {
        let err = Stream::parse("cycle 0\n  7 ticks=1\ncycle +1s\n  8 ticks=2\n")
            .expect_err("unknown pid must fail");
        assert!(err.to_string().contains("unknown pid 8"));
    }
}
