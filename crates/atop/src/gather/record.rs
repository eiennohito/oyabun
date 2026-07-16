use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::time::Instant;

use atop_stream::{
    CycleEvent, ProcMetadata, ProcSnapshot, ProcState, RawProc, Stream,
    SystemStats as StreamSystemStats,
};

use super::table::ProcTable;
use crate::identity::ProcMeta;
use crate::procs::ProcessEntry;
use crate::sys::RawSystemSnapshot;

pub(crate) struct Recorder {
    file: BufWriter<File>,
    first: Option<Instant>,
}

impl Recorder {
    pub(crate) fn from_env() -> Option<Self> {
        let path = std::env::var_os("ATOP_RECORD").map(PathBuf::from)?;
        let file = open_recording(&path)
            .unwrap_or_else(|err| panic!("open ATOP_RECORD path {}: {err}", path.display()));
        Some(Self {
            file: BufWriter::new(file),
            first: None,
        })
    }

    pub(crate) fn record(
        &mut self,
        now: Instant,
        raw_sys: &RawSystemSnapshot,
        procs: &[ProcessEntry],
        table: &ProcTable,
    ) {
        let first = *self.first.get_or_insert(now);
        let wall_ns = now.checked_duration_since(first).map_or(0, |duration| {
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
        });

        let mut cycle = CycleEvent {
            wall_ns,
            sys: convert_sys(raw_sys),
            procs: Vec::with_capacity(procs.len()),
        };
        for proc in procs {
            let cmdline = table.cmdline(proc);
            let cgroup = table.cgroup(proc);
            let flatpak = table.flatpak_info(proc);
            cycle.procs.push(ProcSnapshot {
                raw: convert_proc(proc),
                metadata: ProcMetadata {
                    cmdline: (!cmdline.is_empty()).then(|| cmdline.to_vec()),
                    cgroup: (!cgroup.is_empty()).then(|| cgroup.to_vec()),
                    flatpak: (!flatpak.is_empty()).then(|| flatpak.to_vec()),
                },
            });
        }

        let stream = Stream {
            cycles: vec![cycle],
        };
        self.file
            .write_all(stream.to_verbose_dsl().as_bytes())
            .expect("write ATOP_RECORD stream");
        self.file.flush().expect("flush ATOP_RECORD stream");
    }
}

fn open_recording(path: &std::path::Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other(
            "ATOP_RECORD path must be a regular file",
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn convert_sys(snap: &RawSystemSnapshot) -> StreamSystemStats {
    snap.to_stream(crate::sys::num_cpus())
}

fn convert_proc(proc: &ProcessEntry) -> RawProc {
    RawProc {
        pid: proc.pid,
        ppid: proc.ppid,
        uid: proc.uid,
        state: ProcState::try_from(proc.state).unwrap_or_else(|()| {
            panic!(
                "cannot record unsupported process state {:?}",
                char::from(proc.state)
            )
        }),
        priority: proc.priority,
        nice: proc.nice,
        num_threads: proc.num_threads,
        ticks: proc.ticks,
        mem_bytes: proc.mem_bytes,
        start_time: proc.start_time,
        comm: proc.comm().to_vec(),
        is_kthread: proc.is_kthread,
    }
}

#[cfg(test)]
mod tests {
    use super::open_recording;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn recording_file_is_private_even_when_it_already_exists() {
        let path = std::env::temp_dir().join(format!("atop-record-perms-{}", std::process::id()));
        std::fs::write(&path, b"").expect("create test recording");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .expect("make test recording insecure");

        let file = open_recording(&path).expect("open recording");
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);

        drop(file);
        std::fs::remove_file(path).expect("remove test recording");
    }
}
