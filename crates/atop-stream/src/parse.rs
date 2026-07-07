use std::collections::BTreeMap;

use crate::{CycleEvent, ParseError, ProcState, RawProc, Stream, SystemStats, default_sys};

pub(crate) fn parse(input: &str) -> Result<Stream, ParseError> {
    Parser::new(input).parse()
}

struct Parser<'a> {
    input: &'a str,
    state: BTreeMap<u32, ProcSnapshot>,
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
        Ok(PendingCycle {
            first,
            cmdline_updates: BTreeMap::new(),
        })
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
            pending.cmdline_updates.remove(&pid);
            return Ok(());
        }

        let is_birth = op == ProcOp::Birth || (pending.first && !self.state.contains_key(&pid));
        if !is_birth && !self.state.contains_key(&pid) {
            return Err(ParseError::new(line_no, format!("unknown pid {pid}")));
        }
        let mut proc = if is_birth {
            ProcSnapshot {
                raw: RawProc::defaults(pid),
                cmdline: None,
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
        if let Some(cmdline) = &proc.cmdline {
            pending.cmdline_updates.insert(pid, cmdline.clone());
        }
        self.state.insert(pid, proc);
        Ok(())
    }

    fn finish_cycle(&self, pending: PendingCycle) -> CycleEvent {
        let mut procs = Vec::with_capacity(self.state.len());
        let mut cmdlines = Vec::new();
        for (pid, state) in &self.state {
            procs.push(state.raw.clone());
            // CycleEvent is a fully resolved snapshot. Replay clears its per-cycle cmdline
            // map before loading each event, so unchanged cmdlines must be forward-filled.
            if let Some(cmdline) = pending.cmdline_updates.get(pid).or(state.cmdline.as_ref()) {
                cmdlines.push((*pid, cmdline.clone()));
            }
        }
        CycleEvent {
            wall_ns: self.wall_ns,
            sys: self.sys,
            procs,
            cmdlines,
        }
    }
}

#[derive(Clone)]
struct ProcSnapshot {
    raw: RawProc,
    cmdline: Option<Vec<u8>>,
}

impl ProcSnapshot {
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
            "mem" | "mem_bytes" => self.raw.mem_bytes = parse_bytes(value, line_no)?,
            "start" | "start_time" => self.raw.start_time = parse_u64(value, line_no)?,
            "comm" => self.raw.comm = value.as_bytes().to_vec(),
            "cmd" | "cmdline" => self.cmdline = Some(value.as_bytes().to_vec()),
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
            "mem" | "mem_total" => self.mem_total = parse_bytes(value, line_no)?,
            "mem_used" => self.mem_used = parse_bytes(value, line_no)?,
            "mem_cached" => self.mem_cached = parse_bytes(value, line_no)?,
            "swap_total" => self.swap_total = parse_bytes(value, line_no)?,
            "swap_used" => self.swap_used = parse_bytes(value, line_no)?,
            "cpu_user" | "cpu_user_bp" => self.cpu_user_bp = parse_u32(value, line_no)?,
            "cpu_sys" | "cpu_sys_bp" => self.cpu_sys_bp = parse_u32(value, line_no)?,
            "cpu_iowait" | "cpu_iowait_bp" => {
                self.cpu_iowait_bp = parse_u32(value, line_no)?;
            }
            "load" => self.load = parse_load(value, line_no)?,
            "load1" => self.load[0] = parse_u32(value, line_no)?,
            "load5" => self.load[1] = parse_u32(value, line_no)?,
            "load15" => self.load[2] = parse_u32(value, line_no)?,
            "uptime" | "uptime_secs" => self.uptime_secs = parse_u64(value, line_no)?,
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

struct PendingCycle {
    first: bool,
    cmdline_updates: BTreeMap<u32, Vec<u8>>,
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

fn split_field<'a>(token: &'a str, line_no: usize) -> Result<(&'a str, &'a str), ParseError> {
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

fn parse_load(value: &str, line_no: usize) -> Result<[u32; 3], ParseError> {
    let mut out = [0; 3];
    let parts = value.split(',').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(ParseError::new(
            line_no,
            "load requires three comma-separated values",
        ));
    }
    for (dst, part) in out.iter_mut().zip(parts) {
        *dst = parse_u32(part, line_no)?;
    }
    Ok(out)
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
            r#"
            # initial snapshot
            cycle 0 cores=4 mem=8G
              1 ppid=0 uid=0 S comm=init cmd=/sbin/init ticks=100 mem=12M
              42 ppid=1 S comm=bash cmd=/bin/bash mem=3M

            cycle +1s
              42 ticks=+50
              + 43 ppid=1 R comm=cc1

            cycle +500ms
              - 43
            "#,
        )
        .expect("parse");

        assert_eq!(stream.cycles.len(), 3);
        assert_eq!(stream.cycles[1].wall_ns, 1_000_000_000);
        assert_eq!(stream.cycles[2].wall_ns, 1_500_000_000);
        assert_eq!(stream.cycles[1].procs[1].ticks, 50);
        assert_eq!(stream.cycles[1].procs[2].pid, 43);
        assert_eq!(stream.cycles[2].procs.len(), 2);
    }

    #[test]
    fn parses_quoted_values_and_writer_round_trips_verbose_state() {
        let stream = Stream::parse(
            r#"
            cycle 0 load=125,75,50 uptime=3600
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
    fn rejects_unknown_update() {
        let err = Stream::parse("cycle 0\n  7 ticks=1\ncycle +1s\n  8 ticks=2\n")
            .expect_err("unknown pid must fail");
        assert!(err.to_string().contains("unknown pid 8"));
    }
}
