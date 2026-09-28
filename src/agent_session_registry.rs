//! Scan the local Claude Code session registries to check whether a session
//! id is currently held open by a live process, so herdr does not spawn a
//! second `--resume`/`-c` writer onto a conversation another process already
//! has open (ADR-0002, herdr-4r8).
//!
//! Filesystem walking and JSON parsing are the same shape on every OS; only
//! "is this pid alive, and is it *this* process" is platform-specific, so
//! this module carries no `#[cfg(target_os)]` of its own - it calls the
//! per-platform primitives in `crate::platform`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A live process currently holding a Claude Code session open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveHolder {
    pub pid: u32,
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistryRecord {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "procStart")]
    proc_start: Option<u64>,
    #[serde(default, rename = "procStartFt")]
    proc_start_ft: Option<u64>,
}

/// Scan the local Claude Code session registries (`<claude_dir>/sessions`,
/// `<claude_dir>/<worker-profile>/sessions`, `<claude_dir>/profiles/*/sessions`)
/// for a live process holding `session_id`.
///
/// Fails open (returns `None`) on any I/O error, unreadable registry, or a
/// platform with no start-marker primitive - the caller must treat `None` as
/// "unknown", never as "not live".
pub fn find_live_holder(session_id: &str) -> Option<LiveHolder> {
    let root = crate::integration::claude_dir().ok()?;
    find_live_holder_in_dirs(
        &registry_session_dirs(&root),
        session_id,
        crate::platform::process_exists,
        crate::platform::process_start_marker,
    )
}

fn find_live_holder_in_dirs(
    dirs: &[PathBuf],
    session_id: &str,
    process_exists: impl Fn(u32) -> bool,
    process_start_marker: impl Fn(u32) -> Option<u64>,
) -> Option<LiveHolder> {
    for sessions_dir in dirs {
        let Ok(entries) = std::fs::read_dir(sessions_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(pid) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u32>().ok())
            else {
                continue;
            };
            // Cheap pre-filter before any file read: most registry entries
            // are stale, and a dead pid can never be the live holder we are
            // looking for regardless of what its JSON contains.
            if !process_exists(pid) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<RegistryRecord>(&bytes) else {
                continue;
            };
            if record.session_id != session_id {
                continue;
            }
            if !record_matches_live_process(&record, pid, &process_start_marker) {
                continue;
            }
            return Some(LiveHolder {
                pid,
                cwd: record.cwd,
            });
        }
    }
    None
}

/// The registry roots to scan under a Claude config dir: `sessions/` itself,
/// every immediate child's own `sessions/` (worker/profile-carrying dirs
/// such as `worker-mid-*`), and every `profiles/*/sessions/` beneath it.
fn registry_session_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.join("sessions")];
    let Ok(entries) = std::fs::read_dir(root) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if !child.is_dir() {
            continue;
        }
        let child_name = child.file_name().and_then(|name| name.to_str());
        if child_name == Some("sessions") {
            // Already covered by the unconditional push above - without this,
            // <root>/sessions is itself a child of <root> and would also
            // contribute the nonsensical <root>/sessions/sessions.
            continue;
        }
        dirs.push(child.join("sessions"));
        if child_name == Some("profiles") {
            let Ok(profiles) = std::fs::read_dir(&child) else {
                continue;
            };
            for profile in profiles.flatten() {
                dirs.push(profile.path().join("sessions"));
            }
        }
    }
    dirs
}

/// Stale-entry policy: when the start marker cannot be compared (platform
/// gap, or the field is simply absent from this registry's records), treat
/// the pid as live. A skipped resume is recoverable on the next restart or
/// manual retry; a duplicate writer on one session is not.
fn record_matches_live_process(
    record: &RegistryRecord,
    pid: u32,
    process_start_marker: &impl Fn(u32) -> Option<u64>,
) -> bool {
    match process_start_marker(pid) {
        Some(observed) => {
            record.proc_start == Some(observed) || record.proc_start_ft == Some(observed)
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory under the OS temp dir, removed when the
    /// guard drops - the same manual pattern `integration::tests::unique_base`
    /// uses, so this module adds no new dev-dependency for fixtures.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "herdr-agent-session-registry-test-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_registry_entry(dir: &Path, pid: u32, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("{pid}.json")), json).unwrap();
    }

    #[test]
    fn agent_restore_4r8_find_live_holder_matches_session_id_with_matching_start_marker() {
        let tmp = TestDir::new("matching-marker");
        let sessions = tmp.path().join("sessions");
        write_registry_entry(
            &sessions,
            4242,
            r#"{"sessionId":"target-session","cwd":"/work/project","procStart":100}"#,
        );

        let holder = find_live_holder_in_dirs(
            &[sessions],
            "target-session",
            |pid| pid == 4242,
            |pid| (pid == 4242).then_some(100),
        );

        assert_eq!(
            holder,
            Some(LiveHolder {
                pid: 4242,
                cwd: Some("/work/project".into()),
            })
        );
    }

    #[test]
    fn agent_restore_4r8_find_live_holder_ignores_dead_pid() {
        let tmp = TestDir::new("dead-pid");
        let sessions = tmp.path().join("sessions");
        write_registry_entry(
            &sessions,
            4242,
            r#"{"sessionId":"target-session","procStart":100}"#,
        );

        let holder = find_live_holder_in_dirs(
            &[sessions],
            "target-session",
            |_pid| false,
            |pid| (pid == 4242).then_some(100),
        );

        assert!(holder.is_none());
    }

    #[test]
    fn agent_restore_4r8_find_live_holder_ignores_mismatched_session_id() {
        let tmp = TestDir::new("mismatched-session-id");
        let sessions = tmp.path().join("sessions");
        write_registry_entry(
            &sessions,
            4242,
            r#"{"sessionId":"other-session","procStart":100}"#,
        );

        let holder = find_live_holder_in_dirs(
            &[sessions],
            "target-session",
            |pid| pid == 4242,
            |pid| (pid == 4242).then_some(100),
        );

        assert!(holder.is_none());
    }

    #[test]
    fn agent_restore_4r8_find_live_holder_treats_incomparable_start_marker_as_live() {
        let tmp = TestDir::new("incomparable-marker");
        let sessions = tmp.path().join("sessions");
        write_registry_entry(
            &sessions,
            4242,
            r#"{"sessionId":"target-session","cwd":"/work/project"}"#,
        );

        let holder = find_live_holder_in_dirs(
            &[sessions],
            "target-session",
            |pid| pid == 4242,
            |_pid| None,
        );

        assert_eq!(
            holder,
            Some(LiveHolder {
                pid: 4242,
                cwd: Some("/work/project".into()),
            })
        );
    }

    #[test]
    fn agent_restore_4r8_registry_session_dirs_covers_three_roots() {
        let tmp = TestDir::new("three-roots");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::fs::create_dir_all(root.join("worker-mid-x").join("sessions")).unwrap();
        std::fs::create_dir_all(root.join("profiles").join("y").join("sessions")).unwrap();

        let mut dirs = registry_session_dirs(root);
        dirs.sort();
        let mut expected = vec![
            root.join("sessions"),
            root.join("worker-mid-x").join("sessions"),
            root.join("profiles").join("y").join("sessions"),
            // registry_session_dirs also pushes <profiles-dir>/sessions itself,
            // as a plain immediate child, before descending into it.
            root.join("profiles").join("sessions"),
        ];
        expected.sort();

        assert_eq!(dirs, expected);
    }
}
