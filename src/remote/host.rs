//! Remote-host side of the SSH stdio bridge.

use std::io;
use std::time::Duration;

pub(crate) fn run_remote_client_bridge(args: &[String]) -> io::Result<()> {
    let idle_timeout = match args {
        [] => false,
        [option]
            if option == "--idle-timeout-v1"
                && crate::platform::REMOTE_BRIDGE_IDLE_TIMEOUT_SUPPORTED =>
        {
            true
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported remote client bridge option",
            ))
        }
    };
    ensure_remote_server_running()?;
    #[cfg(unix)]
    let _ssh_agent = super::ssh_agent::Registration::start();

    let socket_path = crate::server::socket_paths::client_socket_path();
    let stream = crate::ipc::connect_local_stream(&socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to connect to remote Herdr client socket {}: {err}",
                socket_path.display()
            ),
        )
    })?;

    crate::platform::forward_remote_bridge_stdio(stream, idle_timeout)
}

fn ensure_remote_server_running() -> io::Result<()> {
    let socket_path = crate::server::socket_paths::client_socket_path();
    if crate::server::autodetect::is_server_listening() {
        let status = crate::api::read_runtime_status_at(
            &crate::api::socket_path(),
            Duration::from_millis(500),
        )?
        .ok_or_else(|| io::Error::other("remote server status API is unavailable"))?;
        if status
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.endpoint_protocol_generation)
            == Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION)
        {
            return Ok(());
        }
        return Err(io::Error::other(
            "remote herdr server needs one final update before this bridge can attach; rerun `herdr --remote` from an interactive terminal to approve it",
        ));
    }

    let unit_installed = herdr_unit_installed();
    let plan = server_start_plan(
        unit_installed,
        crate::session::explicit_session_requested(),
        std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR).is_some(),
    );
    start_remote_server(
        plan,
        systemctl_start_unit,
        || crate::server::autodetect::spawn_server_daemon().map(|_| ()),
        |timeout| crate::server::autodetect::wait_for_server_socket(&socket_path, timeout),
    )
}

/// How the bridge starts a server when none is listening (bd herdr-waz6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerStartPlan {
    /// The host has the `herdr.service` systemd --user unit: start THAT unit.
    SystemdUnit,
    /// No unit (or a non-default session/socket): spawn a detached `herdr server`.
    DirectSpawn,
}

const HERDR_UNIT: &str = "herdr.service";
const UNIT_START_WAIT: Duration = Duration::from_secs(30);
const DIRECT_START_WAIT: Duration = Duration::from_secs(5);

/// The unit runs the DEFAULT session on the default socket, so it only stands in for the bridge's
/// server when the bridge targets that same session. A server spawned over ssh lives in the ssh
/// session's cgroup, outside the unit; the unit's socket-holder wait then refuses to start and
/// retries forever (dcc 2026-10-02..05, 11,346 restarts).
fn server_start_plan(
    unit_installed: bool,
    explicit_session: bool,
    socket_override: bool,
) -> ServerStartPlan {
    if unit_installed && !explicit_session && !socket_override {
        ServerStartPlan::SystemdUnit
    } else {
        ServerStartPlan::DirectSpawn
    }
}

/// Runs the chosen plan. A failed unit start is an error: it never falls back to an unmanaged
/// server, because that is exactly the process the unit then cannot replace.
fn start_remote_server(
    plan: ServerStartPlan,
    start_unit: impl FnOnce() -> io::Result<()>,
    spawn_direct: impl FnOnce() -> io::Result<()>,
    wait_for_socket: impl FnOnce(Duration) -> io::Result<()>,
) -> io::Result<()> {
    match plan {
        ServerStartPlan::SystemdUnit => {
            start_unit().map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "no herdr server is running and `systemctl --user start {HERDR_UNIT}` failed: {err}; not starting an unmanaged server beside the unit"
                    ),
                )
            })?;
            wait_for_socket(UNIT_START_WAIT)
        }
        ServerStartPlan::DirectSpawn => {
            spawn_direct()?;
            wait_for_socket(DIRECT_START_WAIT)
        }
    }
}

fn unit_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        dirs.push(std::path::PathBuf::from(config).join("systemd/user"));
    } else if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        dirs.push(std::path::PathBuf::from(home).join(".config/systemd/user"));
    }
    dirs.push("/etc/systemd/user".into());
    dirs.push("/usr/lib/systemd/user".into());
    dirs.push("/lib/systemd/user".into());
    dirs.push("/usr/local/lib/systemd/user".into());
    dirs
}

fn unit_file_in(dirs: &[std::path::PathBuf]) -> bool {
    dirs.iter().any(|dir| dir.join(HERDR_UNIT).is_file())
}

#[cfg(target_os = "linux")]
fn herdr_unit_installed() -> bool {
    unit_file_in(&unit_dirs())
}

#[cfg(not(target_os = "linux"))]
fn herdr_unit_installed() -> bool {
    false
}

fn systemctl_start_unit() -> io::Result<()> {
    let output = std::process::Command::new("systemctl")
        .args(["--user", "start", HERDR_UNIT])
        .stdin(std::process::Stdio::null())
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn plan_uses_the_unit_when_installed_on_the_default_session() {
        assert_eq!(
            server_start_plan(true, false, false),
            ServerStartPlan::SystemdUnit
        );
    }

    #[test]
    fn plan_spawns_directly_without_a_unit() {
        assert_eq!(
            server_start_plan(false, false, false),
            ServerStartPlan::DirectSpawn
        );
    }

    #[test]
    fn plan_spawns_directly_for_an_explicit_session() {
        assert_eq!(
            server_start_plan(true, true, false),
            ServerStartPlan::DirectSpawn
        );
    }

    #[test]
    fn plan_spawns_directly_for_a_socket_override() {
        assert_eq!(
            server_start_plan(true, false, true),
            ServerStartPlan::DirectSpawn
        );
    }

    #[test]
    fn unit_plan_starts_the_unit_and_never_spawns() {
        let started = Cell::new(0);
        let spawned = Cell::new(0);
        let waited = Cell::new(None);
        start_remote_server(
            ServerStartPlan::SystemdUnit,
            || {
                started.set(started.get() + 1);
                Ok(())
            },
            || {
                spawned.set(spawned.get() + 1);
                Ok(())
            },
            |timeout| {
                waited.set(Some(timeout));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((started.get(), spawned.get()), (1, 0));
        assert_eq!(waited.get(), Some(UNIT_START_WAIT));
    }

    #[test]
    fn direct_plan_spawns_and_never_touches_the_unit() {
        let started = Cell::new(0);
        let spawned = Cell::new(0);
        let waited = Cell::new(None);
        start_remote_server(
            ServerStartPlan::DirectSpawn,
            || {
                started.set(started.get() + 1);
                Ok(())
            },
            || {
                spawned.set(spawned.get() + 1);
                Ok(())
            },
            |timeout| {
                waited.set(Some(timeout));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((started.get(), spawned.get()), (0, 1));
        assert_eq!(waited.get(), Some(DIRECT_START_WAIT));
    }

    #[test]
    fn failed_unit_start_is_a_clear_error_without_fallback() {
        let spawned = Cell::new(0);
        let waited = Cell::new(0);
        let err = start_remote_server(
            ServerStartPlan::SystemdUnit,
            || Err(io::Error::other("exit status: 1: Failed to connect to bus")),
            || {
                spawned.set(spawned.get() + 1);
                Ok(())
            },
            |_| {
                waited.set(waited.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("systemctl --user start herdr.service"), "{message}");
        assert!(message.contains("Failed to connect to bus"), "{message}");
        assert!(message.contains("not starting an unmanaged server"), "{message}");
        assert_eq!((spawned.get(), waited.get()), (0, 0));
    }

    #[test]
    fn unit_socket_wait_failure_propagates_without_fallback() {
        let spawned = Cell::new(0);
        let err = start_remote_server(
            ServerStartPlan::SystemdUnit,
            || Ok(()),
            || {
                spawned.set(spawned.get() + 1);
                Ok(())
            },
            |_| Err(io::Error::new(io::ErrorKind::TimedOut, "socket not ready")),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(spawned.get(), 0);
    }

    #[test]
    fn unit_file_detection_finds_the_unit_in_any_listed_dir() {
        let root = std::env::temp_dir().join(format!("herdr-waz6-units-{}", std::process::id()));
        let empty = root.join("empty");
        let with_unit = root.join("with");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&with_unit).unwrap();
        assert!(!unit_file_in(&[empty.clone()]));
        std::fs::write(with_unit.join(HERDR_UNIT), "[Service]\n").unwrap();
        assert!(unit_file_in(&[empty, with_unit]));
        let _ = std::fs::remove_dir_all(&root);
    }
}
