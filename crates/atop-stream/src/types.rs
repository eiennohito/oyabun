use std::fmt::{self, Write as _};
use std::fs;
use std::path::Path;

use crate::error::{LoadError, ParseError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawProc {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub state: ProcState,
    pub priority: i8,
    pub nice: i8,
    pub num_threads: u32,
    pub ticks: u64,
    pub mem_bytes: u64,
    pub start_time: u64,
    pub comm: Vec<u8>,
    pub is_kthread: bool,
}

impl RawProc {
    #[must_use]
    pub fn defaults(pid: u32) -> Self {
        Self {
            pid,
            ppid: 0,
            uid: 1000,
            state: ProcState::Sleeping,
            priority: 20,
            nice: 0,
            num_threads: 1,
            ticks: 0,
            mem_bytes: 0,
            start_time: u64::from(pid),
            comm: format!("p{pid}").into_bytes(),
            is_kthread: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    Running,
    Sleeping,
    DiskSleep,
    Zombie,
    Stopped,
    Idle,
}

impl ProcState {
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Running => b'R',
            Self::Sleeping => b'S',
            Self::DiskSleep => b'D',
            Self::Zombie => b'Z',
            Self::Stopped => b'T',
            Self::Idle => b'I',
        }
    }

    #[must_use]
    pub fn as_char(self) -> char {
        char::from(self.as_u8())
    }
}

impl TryFrom<u8> for ProcState {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            b'R' => Ok(Self::Running),
            b'S' => Ok(Self::Sleeping),
            b'D' => Ok(Self::DiskSleep),
            b'Z' => Ok(Self::Zombie),
            b'T' => Ok(Self::Stopped),
            b'I' => Ok(Self::Idle),
            _ => Err(()),
        }
    }
}

impl fmt::Display for ProcState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char(self.as_char())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemStats {
    pub cpu_user_bp: u32,
    pub cpu_sys_bp: u32,
    pub cpu_iowait_bp: u32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub mem_cached: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub load: [u32; 3],
    pub uptime_secs: u64,
    pub num_cores: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleEvent {
    pub wall_ns: u64,
    pub sys: SystemStats,
    pub procs: Vec<RawProc>,
    pub cmdlines: Vec<(u32, Vec<u8>)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    pub cycles: Vec<CycleEvent>,
}

impl Stream {
    /// Parse stateful atop stream DSL into fully resolved cycle snapshots.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] when syntax, time, integer, unit, or stateful process
    /// transitions are invalid.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        crate::parse::parse(input)
    }

    /// Load and parse an atop stream DSL file.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from reading `path`, or parser errors for invalid DSL.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let text = fs::read_to_string(path).map_err(LoadError::Io)?;
        Self::parse(&text).map_err(LoadError::Parse)
    }

    #[must_use]
    pub fn to_verbose_dsl(&self) -> String {
        crate::write::verbose_stream(self)
    }
}

#[must_use]
pub fn default_sys() -> SystemStats {
    SystemStats {
        mem_total: 8 * 1024 * 1024 * 1024,
        num_cores: 4,
        ..SystemStats::default()
    }
}
