//! Cgroup-derived process identity: the stable, kernel-owned boundary.
//!
//! Each process resolves its identity independently from its own cgroup line, so membership
//! follows shared identity rather than tree ancestry — an application whose helpers sit in
//! sibling scopes (a split launcher `.scope` and instantiated `.service`) still forms one group.
//! Desktop-session applications (systemd `app.slice` `app-<id>` units, including flatpak) come
//! first; containers, pods, and system services fall out of the remaining hierarchy shape.

use std::borrow::Cow;

use super::{DesktopApp, IdentityKind, ProcessIdentity};

/// Resolve a kernel-cgroup identity from a process's cgroup line (+ flatpak metadata). Returns
/// `None` for processes with no meaningful cgroup boundary (login sessions, transient terminal
/// scopes, bare `init.scope`), which are left to structural detection or stay ungrouped.
pub(super) fn extract(cgroup: &[u8], flatpak_info: &[u8]) -> Option<ProcessIdentity> {
    let path = selected_path(cgroup)?;
    if let Some(app) = desktop_app(path, flatpak_info) {
        return Some(app);
    }
    let owner_uid = session_owner(path).unwrap_or(0);
    if let Some(uid) = pod_uid(path) {
        return Some(ProcessIdentity {
            owner_uid,
            kind: IdentityKind::Pod {
                label: short_label(b"pod/", &uid),
            },
        });
    }
    if let Some(id) = container_id(path) {
        return Some(ProcessIdentity {
            owner_uid,
            kind: IdentityKind::Container {
                label: short_label(b"container/", &id),
            },
        });
    }
    if let Some(unit) = systemd_unit(path) {
        return Some(ProcessIdentity {
            owner_uid,
            kind: IdentityKind::Systemd {
                unit: String::from_utf8_lossy(&unit).into_owned(),
            },
        });
    }
    None
}

/// `<prefix><first 12 chars of id>` — a stable, human-readable container/pod label.
fn short_label(prefix: &[u8], id: &[u8]) -> String {
    let short = &id[..id.len().min(12)];
    let mut label = String::from_utf8_lossy(prefix).into_owned();
    label.push_str(&String::from_utf8_lossy(short));
    label
}

// --- desktop-session application (app.slice) ---------------------------------------------------

fn desktop_app(path: &[u8], flatpak_info: &[u8]) -> Option<ProcessIdentity> {
    let owner_uid = session_owner(path)?;
    let unit = application_unit(path)?;
    let decoded = decode_systemd(unit)?;
    let (body, launcher_scope, instantiated) = strip_unit_decorations(&decoded)?;
    let flatpak_id = flatpak_app_id(flatpak_info);
    let flatpak = flatpak_id.is_some() || body.starts_with("flatpak-");
    let app_id = flatpak_id.map_or_else(
        || body.strip_prefix("flatpak-").unwrap_or(body).to_owned(),
        str::to_owned,
    );
    valid_app_id(&app_id).then_some(ProcessIdentity {
        owner_uid,
        kind: IdentityKind::DesktopApp(DesktopApp {
            app_id,
            launcher_scope,
            instantiated,
            flatpak,
        }),
    })
}

fn application_unit(path: &[u8]) -> Option<&[u8]> {
    let mut parts = path.split(|&b| b == b'/');
    while let Some(part) = parts.next() {
        if part == b"app.slice" {
            let unit = parts.next()?;
            return (unit.starts_with(b"app-")
                && (unit.ends_with(b".scope") || unit.ends_with(b".service")))
            .then_some(unit);
        }
    }
    None
}

fn decode_systemd(unit: &[u8]) -> Option<Cow<'_, str>> {
    let text = std::str::from_utf8(unit).ok()?;
    if !text.as_bytes().contains(&b'\\') {
        return Some(Cow::Borrowed(text));
    }
    let mut out = Vec::with_capacity(unit.len());
    let mut i = 0;
    while i < unit.len() {
        if unit[i] == b'\\' {
            if i + 3 >= unit.len() || unit[i + 1] != b'x' {
                return None;
            }
            let hi = hex(unit[i + 2])?;
            let lo = hex(unit[i + 3])?;
            out.push((hi << 4) | lo);
            i += 4;
        } else {
            out.push(unit[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok().map(Cow::Owned)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn strip_unit_decorations(unit: &str) -> Option<(&str, bool, bool)> {
    let body = unit.strip_prefix("app-")?;
    let (body, launcher_scope) = body
        .strip_suffix(".scope")
        .map(|b| (b, true))
        .or_else(|| body.strip_suffix(".service").map(|b| (b, false)))?;
    if body.starts_with("dbus-") || body.starts_with("session-") {
        return None;
    }
    if let Some((id, instance)) = body.rsplit_once('@') {
        return (!id.is_empty() && !instance.is_empty()).then_some((id, launcher_scope, true));
    }
    if launcher_scope
        && let Some((id, suffix)) = body.rsplit_once('-')
        && !id.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit())
    {
        if id
            .rsplit_once('-')
            .is_some_and(|(_, tab)| !tab.is_empty() && tab.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        return Some((id, true, false));
    }
    Some((body, launcher_scope, false))
}

fn flatpak_app_id(info: &[u8]) -> Option<&str> {
    let mut application = false;
    for raw in info.split(|&b| b == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.starts_with(b"[") {
            application = line.eq_ignore_ascii_case(b"[Application]");
            continue;
        }
        if application {
            let Some(split) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let (key, value) = (&line[..split], &line[split + 1..]);
            if key.eq_ignore_ascii_case(b"name") || key.eq_ignore_ascii_case(b"app") {
                return std::str::from_utf8(value).ok().filter(|s| valid_app_id(s));
            }
        }
    }
    None
}

fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

// --- shared cgroup-path parsing ---------------------------------------------------------------

fn session_owner(path: &[u8]) -> Option<u32> {
    path.split(|&b| b == b'/').find_map(|part| {
        part.strip_prefix(b"user-")?
            .strip_suffix(b".slice")
            .and_then(|uid| std::str::from_utf8(uid).ok())?
            .parse()
            .ok()
    })
}

/// The v2 unified path (`0::`), else the longest useful v1 controller path.
pub(super) fn selected_path(raw: &[u8]) -> Option<&[u8]> {
    let mut fallback = None;
    for line in raw.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix(b"0::") {
            return useful_path(path);
        }
        let Some(first) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let Some(second_rel) = line[first + 1..].iter().position(|&b| b == b':') else {
            continue;
        };
        let path = &line[first + 1 + second_rel + 1..];
        if let Some(path) = useful_path(path)
            && fallback.is_none_or(|old: &[u8]| path.len() > old.len())
        {
            fallback = Some(path);
        }
    }
    fallback
}

fn useful_path(path: &[u8]) -> Option<&[u8]> {
    (!path.is_empty() && path != b"/").then_some(path)
}

fn systemd_unit(path: &[u8]) -> Option<Vec<u8>> {
    path.split(|&b| b == b'/')
        .rev()
        .find(|part| useful_systemd_unit(part))
        .map(Vec::from)
}

fn useful_systemd_unit(unit: &[u8]) -> bool {
    unit.ends_with(b".service") && !unit.starts_with(b"user@") && !unit.starts_with(b"app-")
}

fn pod_uid(path: &[u8]) -> Option<Vec<u8>> {
    path.split(|&b| b == b'/').find_map(|part| {
        let tail = if let Some(rest) = part.strip_prefix(b"pod") {
            rest
        } else {
            let idx = rfind_bytes(part, b"-pod")?;
            &part[idx + 4..]
        };
        let tail = tail
            .strip_prefix(b"-")
            .or_else(|| tail.strip_prefix(b"_"))
            .unwrap_or(tail);
        let end = tail
            .iter()
            .position(|&b| !(b.is_ascii_hexdigit() || b == b'_' || b == b'-'))
            .unwrap_or(tail.len());
        let uid = tail[..end].to_vec();
        (uid.iter().filter(|&&b| b.is_ascii_hexdigit()).count() >= 12).then_some(uid)
    })
}

fn container_id(path: &[u8]) -> Option<Vec<u8>> {
    let mut previous_runtime_dir = false;
    for part in path.split(|&b| b == b'/') {
        let part = part.strip_suffix(b".scope").unwrap_or(part);
        if previous_runtime_dir {
            let id = hex_prefix(part);
            if id.len() >= 12 {
                return Some(id.to_vec());
            }
        }
        previous_runtime_dir = matches!(part, b"docker" | b"containerd" | b"crio" | b"libpod");
        for prefix in [
            b"docker-".as_slice(),
            b"containerd-",
            b"cri-containerd-",
            b"crio-",
            b"libpod-",
        ] {
            if let Some(rest) = part.strip_prefix(prefix) {
                let id = hex_prefix(rest);
                if id.len() >= 12 {
                    return Some(id.to_vec());
                }
            }
        }
        let id = hex_prefix(part);
        if id.len() >= 32 {
            return Some(id.to_vec());
        }
    }
    None
}

fn hex_prefix(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|b| !b.is_ascii_hexdigit())
        .unwrap_or(bytes.len());
    &bytes[..end]
}

fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(unit: &str) -> Option<ProcessIdentity> {
        let raw = format!("0::/user.slice/user-1000.slice/user@1000.service/app.slice/{unit}\n");
        extract(raw.as_bytes(), b"")
    }

    fn desktop(id: &ProcessIdentity) -> &DesktopApp {
        match &id.kind {
            IdentityKind::DesktopApp(app) => app,
            other => panic!("expected DesktopApp, got {other:?}"),
        }
    }

    #[test]
    fn extracts_and_decodes_application_units() {
        let chrome = app(r"app-google\x2dchrome@489abc.service").unwrap();
        assert_eq!(desktop(&chrome).app_id, "google-chrome");
        assert_eq!(chrome.owner_uid, 1000);
        assert!(desktop(&chrome).instantiated);
        assert_eq!(
            desktop(&app("app-com.google.Chrome-1684.scope").unwrap()).app_id,
            "com.google.Chrome"
        );
    }

    #[test]
    fn flatpak_metadata_wins_over_unit_instance() {
        let raw = b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-com.slack.Slack-99.scope\n";
        let got = extract(raw, b"[Application]\nname=com.slack.Slack\n").unwrap();
        assert_eq!(desktop(&got).app_id, "com.slack.Slack");
        assert!(desktop(&got).flatpak);
    }

    #[test]
    fn rejects_non_application_and_malformed_units() {
        for raw in [
            b"0::/user.slice/user-1000.slice/session.slice/session-2.scope\n".as_slice(),
            b"0::/user.slice/user-1000.slice/background.slice/app-foo.scope\n",
            b"0::/user.slice/user-1000.slice/app.slice/dbus-foo.service\n",
            br"0::/user.slice/user-1000.slice/app.slice/app-bad\xZZ.scope\n",
            b"0::/user.slice/user-1000.slice/app.slice/app-kitty-2278-0.scope\n",
        ] {
            assert!(
                extract(raw, b"").is_none_or(|id| !matches!(id.kind, IdentityKind::DesktopApp(_))),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn owner_comes_from_session_not_process() {
        assert_eq!(app("app-foo@abc.service").unwrap().owner_uid, 1000);
    }

    #[test]
    fn system_service_is_a_systemd_identity() {
        let id = extract(b"0::/system.slice/sshd.service\n", b"").unwrap();
        assert!(matches!(id.kind, IdentityKind::Systemd { .. }));
        assert_eq!(id.kind.label(), Some("sshd.service"));
        assert_eq!(id.owner_uid, 0);
        assert!(id.kind.persists());
    }

    #[test]
    fn user_app_is_not_a_systemd_identity() {
        let raw = b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.mozilla.firefox-123.scope\n";
        assert!(matches!(
            extract(raw, b"").unwrap().kind,
            IdentityKind::DesktopApp(_)
        ));
    }

    #[test]
    fn parses_docker_containerd_and_libpod_scopes() {
        for (raw, label) in [
            (
                b"0::/system.slice/docker-abcdef1234567890.scope\n".as_slice(),
                "container/abcdef123456",
            ),
            (
                b"0::/system.slice/cri-containerd-fedcba9876543210.scope\n",
                "container/fedcba987654",
            ),
            (
                b"0::/machine.slice/libpod-1111222233334444.scope\n",
                "container/111122223333",
            ),
        ] {
            let got = extract(raw, b"").unwrap();
            assert!(matches!(got.kind, IdentityKind::Container { .. }));
            assert_eq!(got.kind.label(), Some(label));
        }
    }

    #[test]
    fn parses_kubernetes_pod_path() {
        let raw = b"0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod12345678_90ab_cdef_1234_567890abcdef.slice/cri-containerd-aaaaaaaaaaaa.scope\n";
        let got = extract(raw, b"").unwrap();
        assert!(matches!(got.kind, IdentityKind::Pod { .. }));
        assert_eq!(got.kind.label(), Some("pod/12345678_90a"));
    }

    #[test]
    fn skips_broad_and_transient_scopes() {
        for raw in [
            b"0::/user.slice/user-1000.slice/user@1000.service/init.scope\n".as_slice(),
            b"0::/user.slice/user-1000.slice/session-2.scope\n",
            b"0::/user.slice/user-1000.slice/user@1000.service/app.slice/kitty-2278-3.scope\n",
        ] {
            assert!(extract(raw, b"").is_none(), "{raw:?}");
        }
    }

    #[test]
    fn parses_v1_multi_controller_input() {
        let got = extract(
            b"12:cpu,cpuacct:/docker/abcdef1234567890\n11:memory:/\n",
            b"",
        )
        .unwrap();
        assert!(matches!(got.kind, IdentityKind::Container { .. }));
        assert_eq!(got.kind.label(), Some("container/abcdef123456"));
    }
}
