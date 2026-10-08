//! Lazy debug log: created on first write, no-op in release builds.
//!
//! Replaces `eprintln!` diagnostics that corrupt the TUI in debug builds. The file is
//! created only when something actually writes, so a clean run leaves no trace. The
//! `debug_log!` macro compiles to nothing in release.

use std::path::Path;

#[cfg(debug_assertions)]
pub(crate) mod inner {
    use std::fs::{self, File};
    use std::io::{BufWriter, Write};
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    /// `None` = creation failed (degrade to silent no-op, never panic the TUI).
    static WRITER: OnceLock<Option<Mutex<BufWriter<File>>>> = OnceLock::new();
    static PATH: OnceLock<PathBuf> = OnceLock::new();

    /// Per-user log directory: `$XDG_RUNTIME_DIR/oya/` or `$TMPDIR/oya-<uid>/`.
    /// Created on demand with mode 0700.
    fn log_dir() -> Option<PathBuf> {
        let dir = if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(xdg).join("oya")
        } else {
            let tmp = std::env::temp_dir();
            // SAFETY: getuid is always safe.
            let uid = unsafe { libc::getuid() };
            tmp.join(format!("oya-{uid}"))
        };
        fs::create_dir_all(&dir).ok()?;
        // Ensure the directory is ours and mode 0700 (defends against a pre-planted
        // directory with lax permissions).
        let meta = fs::metadata(&dir).ok()?;
        if !meta.is_dir() {
            return None;
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).ok()?;
        Some(dir)
    }

    /// `YYYYMMDDHHMM` in local time, no external crate.
    #[allow(clippy::cast_possible_wrap)] // time_t is i64; u64 epoch seconds won't wrap before 2262
    fn local_timestamp() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: valid pointers, localtime_r is reentrant.
        unsafe { libc::localtime_r(&raw const secs, &raw mut tm) };
        format!(
            "{:04}{:02}{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
        )
    }

    fn init() -> Option<&'static Mutex<BufWriter<File>>> {
        WRITER
            .get_or_init(|| {
                let dir = log_dir()?;
                let pid = std::process::id();
                let stamp = local_timestamp();
                let path = dir.join(format!("log-{stamp}-{pid}.log"));
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true) // O_EXCL: fails on existing path / symlink
                    .mode(0o600)
                    .open(&path)
                    .ok()?;
                PATH.get_or_init(|| path);
                Some(Mutex::new(BufWriter::new(file)))
            })
            .as_ref()
    }

    #[doc(hidden)]
    pub fn write_line(args: std::fmt::Arguments<'_>) {
        let Some(mtx) = init() else { return };
        let Ok(mut w) = mtx.lock() else { return };
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = elapsed.as_secs();
        let millis = elapsed.subsec_millis();
        let _ = write!(w, "[{secs}.{millis:03}] ");
        let _ = w.write_fmt(args);
        let _ = writeln!(w);
        let _ = w.flush();
    }

    pub fn log_path() -> Option<&'static std::path::Path> {
        PATH.get().map(PathBuf::as_path)
    }
}

/// The log file path, if any write has occurred.
pub fn log_path() -> Option<&'static Path> {
    #[cfg(debug_assertions)]
    {
        inner::log_path()
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

#[cfg(debug_assertions)]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        $crate::log::inner::write_line(format_args!($($arg)*))
    };
}

#[cfg(not(debug_assertions))]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if false { _ = format_args!($($arg)*); }
    };
}

pub(crate) use debug_log;
