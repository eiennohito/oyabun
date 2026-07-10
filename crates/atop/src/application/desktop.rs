use std::ffi::CStr;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::fxhash::FxMap;

/// Hard cap on `.desktop` bytes read. Real entries are well under a kilobyte; when atop runs as
/// root, any local user can plant an arbitrarily large `app-<id>.scope` and force a read of the
/// matching entry, so the read is bounded rather than trusting the file's size.
const DESKTOP_MAX_BYTES: u64 = 64 * 1024;

/// Cap on cached `(uid, app_id)` lookups. A local user can mint unbounded transient
/// `app-<id>.scope` units (each a distinct app id), so the negative/positive cache is bounded;
/// on overflow it is cleared wholesale and refills lazily.
const CACHE_MAX_ENTRIES: usize = 4096;

#[derive(Clone, Debug)]
pub(crate) struct DesktopEntry {
    pub(crate) name: String,
    pub(crate) launch: Option<String>,
    pub(crate) terminal: bool,
}

pub(crate) struct DesktopResolver {
    system_dirs: Vec<PathBuf>,
    homes: FxMap<u32, Option<PathBuf>>,
    /// `uid -> (app_id -> entry?)`, nested so the hit path looks up by a borrowed `app_id`
    /// without allocating a `(uid, String)` key or hashing twice.
    cache: FxMap<u32, FxMap<String, Option<DesktopEntry>>>,
    entries: usize,
    lookup_count: usize,
}

impl DesktopResolver {
    pub(crate) fn system() -> Self {
        Self {
            system_dirs: [
                "/usr/share/applications",
                "/usr/local/share/applications",
                "/var/lib/flatpak/exports/share/applications",
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect(),
            homes: FxMap::default(),
            cache: FxMap::default(),
            entries: 0,
            lookup_count: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_roots(
        system_dirs: Vec<PathBuf>,
        homes: FxMap<u32, Option<PathBuf>>,
    ) -> Self {
        Self {
            system_dirs,
            homes,
            cache: FxMap::default(),
            entries: 0,
            lookup_count: 0,
        }
    }

    pub(crate) fn resolve(&mut self, uid: u32, app_id: &str) -> Option<&DesktopEntry> {
        if !self
            .cache
            .get(&uid)
            .is_some_and(|apps| apps.contains_key(app_id))
        {
            self.lookup_count += 1;
            let entry = self.lookup(uid, app_id);
            self.store(uid, app_id, entry);
        }
        self.cache
            .get(&uid)
            .and_then(|apps| apps.get(app_id))
            .and_then(Option::as_ref)
    }

    /// Insert a resolved (or absent) entry, clearing the whole cache first if it would exceed
    /// [`CACHE_MAX_ENTRIES`] — a coarse but allocation-free bound on adversarial growth.
    fn store(&mut self, uid: u32, app_id: &str, entry: Option<DesktopEntry>) {
        if self.entries >= CACHE_MAX_ENTRIES {
            self.cache.clear();
            self.entries = 0;
        }
        if self
            .cache
            .entry(uid)
            .or_default()
            .insert(app_id.to_owned(), entry)
            .is_none()
        {
            self.entries += 1;
        }
    }

    fn lookup(&mut self, uid: u32, app_id: &str) -> Option<DesktopEntry> {
        let filename = format!("{app_id}.desktop");
        let home = self.home(uid);
        let user_dirs = home.iter().flat_map(|home| {
            [
                home.join(".local/share/applications"),
                home.join(".local/share/flatpak/exports/share/applications"),
            ]
        });
        user_dirs
            .chain(self.system_dirs.iter().cloned())
            .find_map(|dir| parse_desktop(&dir.join(&filename)))
    }

    fn home(&mut self, uid: u32) -> Option<PathBuf> {
        if let Some(home) = self.homes.get(&uid) {
            return home.clone();
        }
        let home = passwd_home(uid);
        self.homes.insert(uid, home.clone());
        home
    }
}

fn passwd_home(uid: u32) -> Option<PathBuf> {
    // SAFETY: getpwuid_r writes only into the supplied values/buffer. `result` either points
    // to `pwd` for the duration of this function or is null.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result = std::ptr::null_mut();
        let size =
            usize::try_from(libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX).max(4096)).unwrap_or(4096);
        let mut buf = vec![0u8; size];
        if libc::getpwuid_r(
            uid,
            &raw mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &raw mut result,
        ) != 0
            || result.is_null()
            || pwd.pw_dir.is_null()
        {
            return None;
        }
        Some(PathBuf::from(
            CStr::from_ptr(pwd.pw_dir).to_string_lossy().as_ref(),
        ))
    }
}

fn parse_desktop(path: &Path) -> Option<DesktopEntry> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(DESKTOP_MAX_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let mut main = false;
    let (mut name, mut exec, mut categories, mut terminal) = (None, None, None, false);
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.starts_with('[') {
            main = line.eq_ignore_ascii_case("[Desktop Entry]");
            continue;
        }
        if !main || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Name" => name = Some(value.trim().to_owned()),
            "Exec" => exec = Some(value.trim()),
            "Categories" => categories = Some(value),
            "Terminal" => terminal = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    Some(DesktopEntry {
        name: name.filter(|s| !s.is_empty())?,
        launch: exec.and_then(normalize_direct_launch),
        terminal: terminal
            || categories.is_some_and(|cats| {
                cats.split(';')
                    .any(|cat| cat.eq_ignore_ascii_case("TerminalEmulator"))
            }),
    })
}

fn normalize_direct_launch(exec: &str) -> Option<String> {
    let first = exec.split_ascii_whitespace().next()?;
    if first.starts_with('%') || first.contains('=') {
        return None;
    }
    let base = Path::new(first).file_name()?.to_str()?;
    if ["env", "sh", "bash", "flatpak", "gtk-launch"].contains(&base) {
        return None;
    }
    Some(base.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("atop-desktop-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn user_entry_precedes_system_and_parses_only_main_section() {
        let root = temp("precedence");
        let home = root.join("home");
        let user = home.join(".local/share/applications");
        let system = root.join("system");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir_all(&system).unwrap();
        fs::write(system.join("foo.desktop"), "[Desktop Entry]\nName=System\n").unwrap();
        fs::write(user.join("foo.desktop"), "[Other]\nName=Wrong\n[Desktop Entry]\nName=User App\nExec=/usr/bin/foo %U\nCategories=Utility;\n").unwrap();
        let mut homes = FxMap::default();
        homes.insert(1000, Some(home));
        let mut r = DesktopResolver::with_roots(vec![system], homes);
        let e = r.resolve(1000, "foo").unwrap();
        assert_eq!(e.name, "User App");
        assert_eq!(e.launch.as_deref(), Some("foo"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn positive_and_negative_results_are_cached() {
        let root = temp("cache");
        let mut r = DesktopResolver::with_roots(vec![root.clone()], FxMap::default());
        assert!(r.resolve(42, "missing").is_none());
        fs::write(root.join("missing.desktop"), "[Desktop Entry]\nName=Late\n").unwrap();
        assert!(r.resolve(42, "missing").is_none());
        assert_eq!(r.lookup_count, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detects_terminal_and_rejects_generic_launch_signature() {
        let root = temp("terminal");
        fs::write(
            root.join("kitty.desktop"),
            "[Desktop Entry]\nName=kitty\nExec=env kitty\nCategories=TerminalEmulator;\n",
        )
        .unwrap();
        let mut r = DesktopResolver::with_roots(vec![root.clone()], FxMap::default());
        let e = r.resolve(1, "kitty").unwrap();
        assert!(e.terminal);
        assert!(e.launch.is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
