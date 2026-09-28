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

use serde::{Deserialize, Deserializer};

/// A live process currently holding a Claude Code session open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveHolder {
    pub pid: u32,
    pub cwd: Option<String>,
}

/// `procStart`/`procStartFt` in the real registry are JSON **strings**
/// (`"43881434"`), not numbers - measured against 1201 live records on this
/// host, all of which a plain `Option<u64>` field rejects outright, silently
/// dropping every one of them from the scan (P1-a, the finding that made the
/// herdr-4r8 gate a no-op in production while CI's numeric fixtures stayed
/// green). Accept a number too, defensively, since nothing documents the
/// field's type is stable.
fn deserialize_flexible_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrU64 {
        String(String),
        U64(u64),
    }

    let value = Option::<StringOrU64>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        StringOrU64::String(s) => s.parse::<u64>().ok(),
        StringOrU64::U64(n) => Some(n),
    }))
}

#[derive(Debug, Deserialize)]
struct RegistryRecord {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(
        default,
        rename = "procStart",
        deserialize_with = "deserialize_flexible_u64"
    )]
    proc_start: Option<u64>,
    #[serde(
        default,
        rename = "procStartFt",
        deserialize_with = "deserialize_flexible_u64"
    )]
    proc_start_ft: Option<u64>,
    /// `linux:<machine-id>:pid:[<pid-ns-inode>]` (P1-a). A record naming a
    /// pid domain that is not OURS is a live pid in some other pid namespace
    /// or on some other machine, which pid reuse can make an entirely
    /// unrelated process under the same pid number in our own namespace -
    /// `process_exists`/`process_start_marker` cannot see that difference on
    /// their own. Absent on older Claude Code versions; a missing field
    /// keeps today's (pre-P1-a) behavior exactly, per `record_matches_live_process`.
    #[serde(default, rename = "pidDomain")]
    pid_domain: Option<String>,
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
        crate::platform::local_pid_domain,
    )
}

fn find_live_holder_in_dirs(
    dirs: &[PathBuf],
    session_id: &str,
    process_exists: impl Fn(u32) -> bool,
    process_start_marker: impl Fn(u32) -> Option<u64>,
    local_pid_domain: impl Fn() -> Option<String>,
) -> Option<LiveHolder> {
    // Computed once per scan, not per record: it is a property of THIS
    // process, never of the registry entry being examined.
    let local_domain = local_pid_domain();
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
            if record_pid_domain_is_foreign(record.pid_domain.as_deref(), local_domain.as_deref()) {
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

/// P1-a: a record naming a pid domain that differs from ours is a live pid
/// in another pid namespace (or on another machine sharing this config dir)
/// and can never be a local resume target, regardless of what
/// `process_exists`/`process_start_marker` say about that same pid number
/// read in OUR namespace.
///
/// Fails open like every other comparison in this module: when either side
/// is unknown (an older Claude Code record with no `pidDomain`, or a
/// platform/host where `local_pid_domain` cannot be computed), there is
/// nothing to disagree on, so the record is never treated as foreign on that
/// basis alone - the existing pid+start-marker check still applies.
fn record_pid_domain_is_foreign(record_domain: Option<&str>, local_domain: Option<&str>) -> bool {
    match (record_domain, local_domain) {
        (Some(record_domain), Some(local_domain)) => record_domain != local_domain,
        _ => false,
    }
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
///
/// P2-e: three explicit outcomes, not two - a PROVEN mismatch requires
/// evidence on BOTH sides. Before this fix, a record with neither
/// `procStart` nor `procStartFt` at all (evidence unavailable on the RECORD
/// side) fell through the match arm below as a `None == Some(observed)`
/// comparison and answered "not live" whenever the OS *could* produce a
/// marker - the opposite of the stale-entry policy this function's own doc
/// comment already promised.
fn record_matches_live_process(
    record: &RegistryRecord,
    pid: u32,
    process_start_marker: &impl Fn(u32) -> Option<u64>,
) -> bool {
    if record.proc_start.is_none() && record.proc_start_ft.is_none() {
        // Evidence unavailable on the record side: nothing to compare, so
        // there is no proven mismatch. Fail open per the stale-entry policy.
        return true;
    }
    match process_start_marker(pid) {
        Some(observed) => {
            // Evidence on both sides: this is the only case that may prove
            // a mismatch (a reused pid) rather than fail open.
            record.proc_start == Some(observed) || record.proc_start_ft == Some(observed)
        }
        // Evidence unavailable on the OS side (platform gap, or the pid
        // vanished mid-check): fail open, unchanged from before this fix.
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
            || None,
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
            || None,
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
            || None,
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
            || None,
        );

        assert_eq!(
            holder,
            Some(LiveHolder {
                pid: 4242,
                cwd: Some("/work/project".into()),
            })
        );
    }

    // herdr-3ir/4r8/ct9 fix round, P2-e: the RECORD carries no start marker
    // at all, but the OS *can* produce one for this pid. Before this fix,
    // `None == Some(observed)` answered "not live" here - the opposite of
    // the stale-entry policy this module's own doc comment already
    // promised (a skipped resume is recoverable; a wrongly-skipped one still
    // duplicates the writer it was meant to prevent, just one restart
    // later).
    #[test]
    fn agent_restore_p2e_find_live_holder_missing_record_marker_is_live() {
        let tmp = TestDir::new("missing-record-marker");
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
            // The OS DOES have an answer here - unlike the "incomparable"
            // test above, where the OS itself has no answer.
            |pid| (pid == 4242).then_some(100),
            || None,
        );

        assert_eq!(
            holder,
            Some(LiveHolder {
                pid: 4242,
                cwd: Some("/work/project".into()),
            }),
            "a record with no start marker at all must not be able to prove a mismatch"
        );
    }

    // The mirror of the test above: BOTH sides have a marker and they
    // disagree - a PROVEN mismatch (pid reuse), the only case allowed to
    // answer "not live".
    #[test]
    fn agent_restore_p2e_find_live_holder_reused_pid_is_not_live() {
        let tmp = TestDir::new("reused-pid");
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
            |pid| (pid == 4242).then_some(999),
            || None,
        );

        assert!(
            holder.is_none(),
            "a start-marker mismatch on both sides present must be treated as pid reuse, not live"
        );
    }

    // herdr-3ir/4r8/ct9 fix round, P1-a: the real registry stores procStart
    // as a JSON STRING, not a number - every one of the 1201 live records
    // measured on this host, all silently rejected by a plain `Option<u64>`
    // field, so the herdr-4r8 gate never fired in production. The fixture
    // below is a redacted real registry record (2.1.259).
    #[test]
    fn agent_restore_p1a_find_live_holder_matches_string_proc_start() {
        let tmp = TestDir::new("string-proc-start");
        let sessions = tmp.path().join("sessions");
        write_registry_entry(
            &sessions,
            1194606,
            r#"{"pid": 1194606, "sessionId": "00000000-0000-4000-8000-000000000000", "cwd": "/work/project", "startedAt": 1788448666291, "procStart": "43881434", "version": "2.1.259", "peerProtocol": 1, "peerFeatures": ["notify_idle", "reply_across_default_dirs", "artifact_yield"], "kind": "interactive", "entrypoint": "sdk-cli", "pidDomain": "linux:884aaaeda1474ac68c9851276f5b6c04:pid:[4026535542]", "messagingSocketPath": "redacted", "name": "redacted", "nameSource": "derived", "nameSince": 1788448666291}"#,
        );

        let holder = find_live_holder_in_dirs(
            &[sessions],
            "00000000-0000-4000-8000-000000000000",
            |pid| pid == 1194606,
            |pid| (pid == 1194606).then_some(43881434),
            || Some("linux:884aaaeda1474ac68c9851276f5b6c04:pid:[4026535542]".into()),
        );

        assert_eq!(
            holder,
            Some(LiveHolder {
                pid: 1194606,
                cwd: Some("/work/project".into()),
            })
        );
    }

    #[test]
    fn agent_restore_p1a_find_live_holder_string_proc_start_mismatch_is_not_live() {
        let tmp = TestDir::new("string-proc-start-mismatch");
        let sessions = tmp.path().join("sessions");
        write_registry_entry(
            &sessions,
            1194606,
            r#"{"sessionId": "00000000-0000-4000-8000-000000000000", "cwd": "/work/project", "procStart": "43881434", "pidDomain": "linux:884aaaeda1474ac68c9851276f5b6c04:pid:[4026535542]"}"#,
        );

        // The pid is alive, but a DIFFERENT process now holds it - reused
        // since the registry entry was written.
        let holder = find_live_holder_in_dirs(
            &[sessions],
            "00000000-0000-4000-8000-000000000000",
            |pid| pid == 1194606,
            |pid| (pid == 1194606).then_some(99999999),
            || Some("linux:884aaaeda1474ac68c9851276f5b6c04:pid:[4026535542]".into()),
        );

        assert!(holder.is_none());
    }

    #[test]
    fn agent_restore_p1a_find_live_holder_ignores_foreign_pid_domain() {
        let tmp = TestDir::new("foreign-pid-domain");
        let sessions = tmp.path().join("sessions");
        write_registry_entry(
            &sessions,
            1194606,
            r#"{"sessionId": "00000000-0000-4000-8000-000000000000", "cwd": "/work/project", "procStart": "43881434", "pidDomain": "linux:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:pid:[4026531836]"}"#,
        );

        // Same pid, same procStart marker - but a DIFFERENT pid namespace
        // (or a different machine's registry, reachable through a shared
        // config dir): the pid number and its start marker say nothing
        // about the process our own kernel currently maps it to.
        let holder = find_live_holder_in_dirs(
            &[sessions],
            "00000000-0000-4000-8000-000000000000",
            |pid| pid == 1194606,
            |pid| (pid == 1194606).then_some(43881434),
            || Some("linux:884aaaeda1474ac68c9851276f5b6c04:pid:[4026535542]".into()),
        );

        assert!(
            holder.is_none(),
            "a foreign pidDomain must never be reported as a local live holder"
        );
    }

    #[test]
    fn agent_restore_p1a_find_live_holder_missing_pid_domain_keeps_current_behavior() {
        let tmp = TestDir::new("missing-pid-domain");
        let sessions = tmp.path().join("sessions");
        // An older Claude Code version's record - no pidDomain field at all.
        write_registry_entry(
            &sessions,
            1194606,
            r#"{"sessionId": "00000000-0000-4000-8000-000000000000", "cwd": "/work/project", "procStart": "43881434"}"#,
        );

        let holder = find_live_holder_in_dirs(
            &[sessions],
            "00000000-0000-4000-8000-000000000000",
            |pid| pid == 1194606,
            |pid| (pid == 1194606).then_some(43881434),
            || Some("linux:884aaaeda1474ac68c9851276f5b6c04:pid:[4026535542]".into()),
        );

        assert_eq!(
            holder,
            Some(LiveHolder {
                pid: 1194606,
                cwd: Some("/work/project".into()),
            }),
            "a record with no pidDomain must not be gated on the field at all"
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
