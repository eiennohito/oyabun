//! Persisted fold preferences, keyed by an application's stable identity.
//!
//! A fold that survives a restart cannot key on process identifiers (they change every launch);
//! it keys on the group's cross-run-stable identity — a desktop application or a systemd unit.
//! Container ids, pod uids, and structural (heuristic) tokens are ephemeral, so their folds stay
//! session-only. The default is folded, so what is persisted is the user's *expansion* override:
//! a group whose key is recorded here starts expanded next run.

use std::collections::HashSet;
use std::path::PathBuf;

pub(crate) struct FoldPreferences {
    path: Option<PathBuf>,
    expanded: HashSet<String>,
}

impl FoldPreferences {
    /// Load preferences from the per-user config file (best-effort — a missing or unreadable file
    /// yields empty preferences, and persistence is silently disabled if no config dir resolves).
    pub(crate) fn system() -> Self {
        let path = config_path();
        let expanded = path.as_deref().map(load).unwrap_or_default();
        Self { path, expanded }
    }

    /// Preferences that never touch disk — for tests.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            path: None,
            expanded: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        let expanded = load(&path);
        Self {
            path: Some(path),
            expanded,
        }
    }

    /// Whether the user has recorded this identity as expanded (overriding the folded default).
    pub(crate) fn is_expanded(&self, key: &str) -> bool {
        self.expanded.contains(key)
    }

    /// Record (or clear) an expansion override and write it through immediately. Persistence is a
    /// convenience, so I/O errors are ignored rather than surfaced into the render loop.
    pub(crate) fn set_expanded(&mut self, key: &str, expanded: bool) {
        let changed = if expanded {
            self.expanded.insert(key.to_owned())
        } else {
            self.expanded.remove(key)
        };
        if changed && let Some(path) = &self.path {
            let _ = save(path, &self.expanded);
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("oya").join("folds"))
}

fn load(path: &std::path::Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn save(path: &std::path::Path, expanded: &HashSet<String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Sorted for a stable, diff-friendly file. Keys are opaque and machine-managed.
    let mut keys: Vec<&String> = expanded.iter().collect();
    keys.sort_unstable();
    let mut body = keys.into_iter().fold(String::new(), |mut acc, key| {
        acc.push_str(key);
        acc.push('\n');
        acc
    });
    body.truncate(body.trim_end_matches('\n').len());
    std::fs::write(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        std::env::temp_dir()
            .join(format!("oya-folds-{}", std::process::id()))
            .join("folds")
    }

    #[test]
    fn overrides_persist_across_reloads() {
        let path = temp();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut prefs = FoldPreferences::at(path.clone());
        assert!(!prefs.is_expanded("1000\0desktop\0Zed"));
        prefs.set_expanded("1000\0desktop\0Zed", true);
        prefs.set_expanded("0\0systemd\0sshd.service", true);

        // A fresh load sees the recorded overrides.
        let reloaded = FoldPreferences::at(path.clone());
        assert!(reloaded.is_expanded("1000\0desktop\0Zed"));
        assert!(reloaded.is_expanded("0\0systemd\0sshd.service"));

        // Clearing one removes only it, and that too round-trips.
        let mut prefs = FoldPreferences::at(path.clone());
        prefs.set_expanded("1000\0desktop\0Zed", false);
        let reloaded = FoldPreferences::at(path.clone());
        assert!(!reloaded.is_expanded("1000\0desktop\0Zed"));
        assert!(reloaded.is_expanded("0\0systemd\0sshd.service"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn disabled_preferences_never_persist() {
        let mut prefs = FoldPreferences::disabled();
        prefs.set_expanded("anything", true);
        assert!(prefs.is_expanded("anything"), "in-memory only");
    }
}
