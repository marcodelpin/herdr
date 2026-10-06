//! Remote-host side of the SSH stdio bridge.

use std::io;
use std::path::{Path, PathBuf};
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
    ensure_remote_server_running_with(&SYSTEM_HOST)
}

/// The host facts and actions the bridge needs to bring a server up. `SystemHost` is production;
/// tests drive `ensure_remote_server_running_with` through a fake (bd herdr-waz6).
trait RemoteHost {
    fn server_listening(&self) -> bool;
    fn running_endpoint_generation(&self) -> io::Result<Option<u32>>;
    /// The sockets this bridge will use, after `--session`, `HERDR_SESSION` and socket overrides.
    fn effective_endpoint(&self) -> Endpoint;
    /// The sockets `herdr.service` binds: the default session, no overrides.
    fn unit_endpoint(&self) -> Endpoint;
    fn unit_state(&self) -> UnitState;
    fn start_unit(&self) -> io::Result<()>;
    fn spawn_direct(&self) -> io::Result<()>;
    fn wait_for_socket(&self, client_socket: &Path, timeout: Duration) -> io::Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    api_socket: PathBuf,
    client_socket: PathBuf,
}

/// What the current user's systemd manager knows about `herdr.service`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitState {
    /// Loaded (enabled or not): a start request can run it.
    Installed,
    /// Masked: the operator blocked the unit; the bridge must not route around that.
    Masked,
    Absent,
}

/// Production host. `systemctl` is the one process seam, so tests can check the wiring of
/// `unit_state` and `start_unit` without a user manager.
struct SystemHost {
    systemctl: fn(&[&str]) -> io::Result<SystemctlOutput>,
}

const SYSTEM_HOST: SystemHost = SystemHost {
    systemctl: run_systemctl,
};

impl RemoteHost for SystemHost {
    fn server_listening(&self) -> bool {
        crate::server::autodetect::is_server_listening()
    }

    fn running_endpoint_generation(&self) -> io::Result<Option<u32>> {
        let status = crate::api::read_runtime_status_at(
            &crate::api::socket_path(),
            Duration::from_millis(500),
        )?
        .ok_or_else(|| io::Error::other("remote server status API is unavailable"))?;
        Ok(status
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.endpoint_protocol_generation))
    }

    fn effective_endpoint(&self) -> Endpoint {
        Endpoint {
            api_socket: crate::api::socket_path(),
            client_socket: crate::server::socket_paths::client_socket_path(),
        }
    }

    fn unit_endpoint(&self) -> Endpoint {
        Endpoint {
            api_socket: crate::session::api_socket_path_for(None),
            client_socket: crate::session::client_socket_path_for(None),
        }
    }

    fn unit_state(&self) -> UnitState {
        herdr_unit_state(&self.systemctl, &unit_dirs(&|name| std::env::var_os(name)))
    }

    fn start_unit(&self) -> io::Result<()> {
        systemctl_start_unit(&self.systemctl)
    }

    fn spawn_direct(&self) -> io::Result<()> {
        crate::server::autodetect::spawn_server_daemon().map(|_| ())
    }

    fn wait_for_socket(&self, client_socket: &Path, timeout: Duration) -> io::Result<()> {
        crate::server::autodetect::wait_for_server_socket(client_socket, timeout)
    }
}

fn ensure_remote_server_running_with(host: &dyn RemoteHost) -> io::Result<()> {
    if host.server_listening() {
        if host.running_endpoint_generation()?
            == Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION)
        {
            return Ok(());
        }
        return Err(io::Error::other(
            "remote herdr server needs one final update before this bridge can attach; rerun `herdr --remote` from an interactive terminal to approve it",
        ));
    }

    let effective = host.effective_endpoint();
    let target = endpoint_target(&effective, &host.unit_endpoint());
    // The unit is only asked about when the bridge targets its sockets: another session never
    // depends on systemd at all.
    let unit = match target {
        EndpointTarget::Other => UnitState::Absent,
        EndpointTarget::Unit | EndpointTarget::Mixed => host.unit_state(),
    };
    let plan = server_start_plan(unit, target)?;
    start_remote_server(
        plan,
        || host.start_unit(),
        || host.spawn_direct(),
        |timeout| host.wait_for_socket(&effective.client_socket, timeout),
    )
}

/// How the bridge's sockets relate to the ones `herdr.service` binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointTarget {
    /// Both sockets are the unit's: the bridge wants the unit's server.
    Unit,
    /// Neither socket is the unit's: a separate server the unit never competes with.
    Other,
    /// One socket is the unit's and the other is not (e.g. `HERDR_SESSION=work` plus a client
    /// socket override pointing at the default session).
    Mixed,
}

fn endpoint_target(effective: &Endpoint, unit: &Endpoint) -> EndpointTarget {
    match (
        same_path(&effective.api_socket, &unit.api_socket),
        same_path(&effective.client_socket, &unit.client_socket),
    ) {
        (true, true) => EndpointTarget::Unit,
        (false, false) => EndpointTarget::Other,
        _ => EndpointTarget::Mixed,
    }
}

/// Compares socket paths through their (canonicalized when possible) parent directory, so a
/// symlinked or `..`-spelled override of the default socket still counts as the default socket.
fn same_path(a: &Path, b: &Path) -> bool {
    fn normalized(path: &Path) -> PathBuf {
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                .map(|parent| parent.join(name))
                .unwrap_or_else(|_| path.to_path_buf()),
            _ => path.to_path_buf(),
        }
    }
    a == b || normalized(a) == normalized(b)
}

/// How the bridge starts a server when none is listening (bd herdr-waz6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerStartPlan {
    /// The host has the `herdr.service` systemd --user unit: start THAT unit.
    SystemdUnit,
    /// No unit, or the bridge targets another session/socket: spawn a detached `herdr server`.
    DirectSpawn,
}

const HERDR_UNIT: &str = "herdr.service";
const UNIT_START_WAIT: Duration = Duration::from_secs(30);
const DIRECT_START_WAIT: Duration = Duration::from_secs(5);
const UNIT_REMEDY: &str = "check `systemctl --user status herdr.service` and `journalctl --user -u herdr.service`; if the unit hit its start limit, run `systemctl --user reset-failed herdr.service` (or wait out StartLimitIntervalSec) and retry";

/// The unit runs the DEFAULT session on the default sockets, so it stands in for the bridge's
/// server exactly when the bridge targets those sockets, whatever spelling selected them
/// (`--session default`, `HERDR_SOCKET_PATH` naming the default socket, or nothing). A server
/// spawned over ssh lives in the ssh session's cgroup, outside the unit; the unit's socket-holder
/// wait then refuses to start and retried every ~16 s (dcc 2026-10-02..05, 11,346 restarts).
fn server_start_plan(unit: UnitState, target: EndpointTarget) -> io::Result<ServerStartPlan> {
    match (target, unit) {
        (EndpointTarget::Other, _) | (_, UnitState::Absent) => Ok(ServerStartPlan::DirectSpawn),
        (EndpointTarget::Unit, UnitState::Installed) => Ok(ServerStartPlan::SystemdUnit),
        (_, UnitState::Masked) => Err(io::Error::other(format!(
            "no herdr server is running and {HERDR_UNIT} is masked; not starting an unmanaged server beside the unit. Unmask it (`systemctl --user unmask {HERDR_UNIT}`) or attach to a non-default session"
        ))),
        (EndpointTarget::Mixed, UnitState::Installed) => Err(io::Error::other(format!(
            "no herdr server is running and the requested sockets mix {HERDR_UNIT}'s default socket with another session's; not starting an unmanaged server on the unit's socket. Point HERDR_SOCKET_PATH/HERDR_CLIENT_SOCKET_PATH and the session at one endpoint"
        ))),
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
                        "no herdr server is running and `systemctl --user start {HERDR_UNIT}` failed: {err}; not starting an unmanaged server beside the unit; {UNIT_REMEDY}"
                    ),
                )
            })?;
            wait_for_socket(UNIT_START_WAIT).map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "{HERDR_UNIT} was started but its socket did not come up within {}s: {err}; not starting an unmanaged server beside the unit; {UNIT_REMEDY}",
                        UNIT_START_WAIT.as_secs()
                    ),
                )
            })
        }
        ServerStartPlan::DirectSpawn => {
            spawn_direct()?;
            wait_for_socket(DIRECT_START_WAIT)
        }
    }
}

/// The result of one `systemctl` run (the seam tests replace).
struct SystemctlOutput {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

fn run_systemctl(args: &[&str]) -> io::Result<SystemctlOutput> {
    let output = std::process::Command::new("systemctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(SystemctlOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// Every systemd --user unit search directory (systemd.unit(5) "User Unit Search Path"); only the
/// fallback when the user manager cannot be asked.
fn unit_dirs(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    let home = var("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    let config_home = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    let config_dirs = var("XDG_CONFIG_DIRS")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_else(|| vec![PathBuf::from("/etc/xdg")]);
    let data_home = var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".local/share")));
    let data_dirs = var("XDG_DATA_DIRS")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_else(|| {
            vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        });
    let runtime = var("XDG_RUNTIME_DIR").map(PathBuf::from);

    if let Some(config_home) = &config_home {
        dirs.push(config_home.join("systemd/user.control"));
    }
    if let Some(runtime) = &runtime {
        dirs.push(runtime.join("systemd/user.control"));
        dirs.push(runtime.join("systemd/transient"));
        dirs.push(runtime.join("systemd/generator.early"));
    }
    if let Some(config_home) = &config_home {
        dirs.push(config_home.join("systemd/user"));
    }
    dirs.extend(config_dirs.iter().map(|dir| dir.join("systemd/user")));
    dirs.push("/etc/systemd/user".into());
    if let Some(runtime) = &runtime {
        dirs.push(runtime.join("systemd/user"));
    }
    dirs.push("/run/systemd/user".into());
    if let Some(runtime) = &runtime {
        dirs.push(runtime.join("systemd/generator"));
    }
    if let Some(data_home) = &data_home {
        dirs.push(data_home.join("systemd/user"));
    }
    dirs.extend(data_dirs.iter().map(|dir| dir.join("systemd/user")));
    dirs.push("/usr/local/lib/systemd/user".into());
    dirs.push("/usr/lib/systemd/user".into());
    dirs.push("/lib/systemd/user".into());
    if let Some(runtime) = &runtime {
        dirs.push(runtime.join("systemd/generator.late"));
    }
    dirs
}

/// Asks the user manager (`LoadState`) first: it knows every search path, drop-in and mask. Only
/// when it cannot be asked (no systemctl, no user bus) are the search directories scanned, and
/// then any `herdr.service` entry, symlinks included, counts: a false "installed" costs a clear
/// start error, a false "absent" costs an unmanaged server beside the unit.
fn herdr_unit_state(
    systemctl: &dyn Fn(&[&str]) -> io::Result<SystemctlOutput>,
    dirs: &[PathBuf],
) -> UnitState {
    if !cfg!(target_os = "linux") {
        return UnitState::Absent;
    }
    match systemctl(&[
        "--user",
        "show",
        "--property=LoadState",
        "--value",
        HERDR_UNIT,
    ]) {
        Ok(output) if output.success => match output.stdout.as_str() {
            "not-found" => UnitState::Absent,
            "masked" => UnitState::Masked,
            "" => unit_state_in(dirs),
            // loaded, bad-setting, error: the unit exists; a start reports what is wrong with it.
            _ => UnitState::Installed,
        },
        _ => unit_state_in(dirs),
    }
}

fn unit_state_in(dirs: &[PathBuf]) -> UnitState {
    for dir in dirs {
        let path = dir.join(HERDR_UNIT);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            let masked = metadata.file_type().is_symlink()
                && std::fs::read_link(&path).is_ok_and(|target| target == Path::new("/dev/null"));
            if masked || (metadata.is_file() && metadata.len() == 0) {
                return UnitState::Masked;
            }
            return UnitState::Installed;
        }
    }
    UnitState::Absent
}

fn systemctl_start_unit(
    systemctl: &dyn Fn(&[&str]) -> io::Result<SystemctlOutput>,
) -> io::Result<()> {
    let output = systemctl(&["--user", "start", HERDR_UNIT])?;
    if output.success {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{}: {}",
        output.status, output.stderr
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn default_endpoint() -> Endpoint {
        Endpoint {
            api_socket: PathBuf::from("/home/u/.config/herdr/herdr.sock"),
            client_socket: PathBuf::from("/home/u/.config/herdr/herdr-client.sock"),
        }
    }

    fn work_endpoint() -> Endpoint {
        Endpoint {
            api_socket: PathBuf::from("/home/u/.config/herdr/sessions/work/herdr.sock"),
            client_socket: PathBuf::from("/home/u/.config/herdr/sessions/work/herdr-client.sock"),
        }
    }

    struct FakeHost {
        listening: bool,
        generation: Option<u32>,
        effective: Endpoint,
        unit: UnitState,
        start_result: RefCell<Option<io::Error>>,
        wait_result: RefCell<Option<io::Error>>,
        unit_queries: Cell<u32>,
        unit_starts: Cell<u32>,
        spawns: Cell<u32>,
        waits: RefCell<Vec<(PathBuf, Duration)>>,
    }

    impl FakeHost {
        fn new(effective: Endpoint, unit: UnitState) -> Self {
            Self {
                listening: false,
                generation: Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION),
                effective,
                unit,
                start_result: RefCell::new(None),
                wait_result: RefCell::new(None),
                unit_queries: Cell::new(0),
                unit_starts: Cell::new(0),
                spawns: Cell::new(0),
                waits: RefCell::new(Vec::new()),
            }
        }
    }

    impl RemoteHost for FakeHost {
        fn server_listening(&self) -> bool {
            self.listening
        }
        fn running_endpoint_generation(&self) -> io::Result<Option<u32>> {
            Ok(self.generation)
        }
        fn effective_endpoint(&self) -> Endpoint {
            self.effective.clone()
        }
        fn unit_endpoint(&self) -> Endpoint {
            default_endpoint()
        }
        fn unit_state(&self) -> UnitState {
            self.unit_queries.set(self.unit_queries.get() + 1);
            self.unit
        }
        fn start_unit(&self) -> io::Result<()> {
            self.unit_starts.set(self.unit_starts.get() + 1);
            self.start_result.borrow_mut().take().map_or(Ok(()), Err)
        }
        fn spawn_direct(&self) -> io::Result<()> {
            self.spawns.set(self.spawns.get() + 1);
            Ok(())
        }
        fn wait_for_socket(&self, client_socket: &Path, timeout: Duration) -> io::Result<()> {
            self.waits
                .borrow_mut()
                .push((client_socket.to_path_buf(), timeout));
            self.wait_result.borrow_mut().take().map_or(Ok(()), Err)
        }
    }

    fn ok(stdout: &str) -> io::Result<SystemctlOutput> {
        Ok(SystemctlOutput {
            success: true,
            status: "exit status: 0".into(),
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-waz6-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    // --- the entry point, through the RemoteHost seam ---

    #[test]
    fn entry_starts_the_unit_for_the_default_endpoint_and_never_spawns() {
        let host = FakeHost::new(default_endpoint(), UnitState::Installed);
        ensure_remote_server_running_with(&host).unwrap();
        assert_eq!((host.unit_starts.get(), host.spawns.get()), (1, 0));
        assert_eq!(
            host.waits.borrow().as_slice(),
            &[(default_endpoint().client_socket, UNIT_START_WAIT)]
        );
    }

    #[test]
    fn entry_spawns_directly_when_no_unit_is_installed() {
        let host = FakeHost::new(default_endpoint(), UnitState::Absent);
        ensure_remote_server_running_with(&host).unwrap();
        assert_eq!((host.unit_starts.get(), host.spawns.get()), (0, 1));
        assert_eq!(host.waits.borrow()[0].1, DIRECT_START_WAIT);
    }

    #[test]
    fn entry_spawns_directly_for_another_session_without_asking_systemd() {
        let host = FakeHost::new(work_endpoint(), UnitState::Installed);
        ensure_remote_server_running_with(&host).unwrap();
        assert_eq!(
            (
                host.unit_queries.get(),
                host.unit_starts.get(),
                host.spawns.get()
            ),
            (0, 0, 1)
        );
        assert_eq!(host.waits.borrow()[0].0, work_endpoint().client_socket);
    }

    #[test]
    fn entry_refuses_a_mixed_endpoint_when_the_unit_is_installed() {
        let mut mixed = work_endpoint();
        mixed.client_socket = default_endpoint().client_socket;
        let host = FakeHost::new(mixed, UnitState::Installed);
        let err = ensure_remote_server_running_with(&host).unwrap_err();
        assert!(
            err.to_string().contains("not starting an unmanaged server"),
            "{err}"
        );
        assert_eq!((host.unit_starts.get(), host.spawns.get()), (0, 0));
    }

    #[test]
    fn entry_refuses_when_the_unit_is_masked() {
        let host = FakeHost::new(default_endpoint(), UnitState::Masked);
        let err = ensure_remote_server_running_with(&host).unwrap_err();
        assert!(
            err.to_string()
                .contains("systemctl --user unmask herdr.service"),
            "{err}"
        );
        assert_eq!((host.unit_starts.get(), host.spawns.get()), (0, 0));
    }

    #[test]
    fn entry_reuses_a_listening_compatible_server() {
        let mut host = FakeHost::new(default_endpoint(), UnitState::Installed);
        host.listening = true;
        ensure_remote_server_running_with(&host).unwrap();
        assert_eq!((host.unit_starts.get(), host.spawns.get()), (0, 0));
        assert!(host.waits.borrow().is_empty());
    }

    #[test]
    fn entry_unit_start_failure_names_the_remedy_and_never_spawns() {
        let host = FakeHost::new(default_endpoint(), UnitState::Installed);
        *host.start_result.borrow_mut() = Some(io::Error::other(
            "exit status: 1: Job for herdr.service failed. Start request repeated too quickly.",
        ));
        let err = ensure_remote_server_running_with(&host).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("systemctl --user start herdr.service"),
            "{message}"
        );
        assert!(message.contains("repeated too quickly"), "{message}");
        assert!(
            message.contains("not starting an unmanaged server"),
            "{message}"
        );
        assert!(
            message.contains("systemctl --user reset-failed herdr.service"),
            "{message}"
        );
        assert_eq!(host.spawns.get(), 0);
        assert!(host.waits.borrow().is_empty());
    }

    #[test]
    fn entry_unit_socket_wait_failure_is_unit_specific_without_fallback() {
        let host = FakeHost::new(default_endpoint(), UnitState::Installed);
        *host.wait_result.borrow_mut() =
            Some(io::Error::new(io::ErrorKind::TimedOut, "socket not ready"));
        let err = ensure_remote_server_running_with(&host).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let message = err.to_string();
        assert!(message.contains("herdr.service was started"), "{message}");
        assert!(
            message.contains("systemctl --user reset-failed herdr.service"),
            "{message}"
        );
        assert_eq!(host.spawns.get(), 0);
    }

    // --- endpoint selection: the effective sockets decide, not how they were spelled ---

    /// Runs the CLI session parsing for `args` and returns where the bridge's sockets land.
    fn target_for_cli(args: &[&str], env: &[(&str, &str)]) -> EndpointTarget {
        // Same env vars as the session and update tests: hold both of their locks.
        let _session_guard = crate::session::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        #[cfg(unix)]
        let _update_guard = crate::update::test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for name in [
            crate::session::SESSION_ENV_VAR,
            crate::api::SOCKET_PATH_ENV_VAR,
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
        ] {
            std::env::remove_var(name);
        }
        for (name, value) in env {
            std::env::set_var(name, value);
        }
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        crate::session::configure_from_args(&args).unwrap();
        let target = endpoint_target(
            &SYSTEM_HOST.effective_endpoint(),
            &SYSTEM_HOST.unit_endpoint(),
        );
        for (name, _) in env {
            std::env::remove_var(name);
        }
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        target
    }

    #[test]
    fn cli_session_selection_decides_by_effective_sockets() {
        let bridge = "remote-client-bridge";
        // `--session default` is an explicit session that IS the unit's endpoint (codex hwaz6-r1 P1).
        assert_eq!(
            target_for_cli(&["herdr", "--session", "default", bridge], &[]),
            EndpointTarget::Unit
        );
        assert_eq!(
            target_for_cli(&["herdr", bridge], &[]),
            EndpointTarget::Unit
        );
        assert_eq!(
            target_for_cli(&["herdr", "--session", "work", bridge], &[]),
            EndpointTarget::Other
        );
        assert_eq!(
            target_for_cli(&["herdr", bridge], &[("HERDR_SESSION", "work")]),
            EndpointTarget::Other
        );
        // HERDR_SESSION=work plus a client-only override naming the default client socket.
        let default_client = crate::session::client_socket_path_for(None);
        assert_eq!(
            target_for_cli(
                &["herdr", bridge],
                &[
                    ("HERDR_SESSION", "work"),
                    ("HERDR_CLIENT_SOCKET_PATH", default_client.to_str().unwrap()),
                ],
            ),
            EndpointTarget::Mixed
        );
        // HERDR_SOCKET_PATH naming the default api socket is the unit's endpoint too.
        let default_api = crate::session::api_socket_path_for(None);
        assert_eq!(
            target_for_cli(
                &["herdr", bridge],
                &[("HERDR_SOCKET_PATH", default_api.to_str().unwrap())],
            ),
            EndpointTarget::Unit
        );
    }

    #[test]
    fn socket_override_naming_the_default_socket_targets_the_unit() {
        let root = temp_root("override");
        let dir = root.join("herdr");
        std::fs::create_dir_all(&dir).unwrap();
        let unit = Endpoint {
            api_socket: dir.join("herdr.sock"),
            client_socket: dir.join("herdr-client.sock"),
        };
        let api_override = root.join("herdr/../herdr/herdr.sock");
        let effective = Endpoint {
            client_socket: crate::server::socket_paths::derive_client_socket_from_api_socket(
                &api_override,
            ),
            api_socket: api_override,
        };
        assert_eq!(endpoint_target(&effective, &unit), EndpointTarget::Unit);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn endpoint_target_classifies_other_and_mixed() {
        assert_eq!(
            endpoint_target(&work_endpoint(), &default_endpoint()),
            EndpointTarget::Other
        );
        let mut mixed = work_endpoint();
        mixed.client_socket = default_endpoint().client_socket;
        assert_eq!(
            endpoint_target(&mixed, &default_endpoint()),
            EndpointTarget::Mixed
        );
    }

    #[test]
    fn plan_table() {
        use EndpointTarget::*;
        use UnitState::*;
        assert_eq!(
            server_start_plan(Installed, Unit).unwrap(),
            ServerStartPlan::SystemdUnit
        );
        assert_eq!(
            server_start_plan(Absent, Unit).unwrap(),
            ServerStartPlan::DirectSpawn
        );
        assert_eq!(
            server_start_plan(Installed, Other).unwrap(),
            ServerStartPlan::DirectSpawn
        );
        assert_eq!(
            server_start_plan(Absent, Mixed).unwrap(),
            ServerStartPlan::DirectSpawn
        );
        assert!(server_start_plan(Masked, Unit).is_err());
        assert!(server_start_plan(Installed, Mixed).is_err());
    }

    // --- unit discovery: ask systemd, scan every search dir only as the fallback ---

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_state_follows_the_user_manager_load_state() {
        let calls = RefCell::new(Vec::new());
        let answer = Cell::new("loaded");
        let ask = |args: &[&str]| {
            calls
                .borrow_mut()
                .push(args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
            ok(answer.get())
        };
        for (load_state, expected) in [
            ("loaded", UnitState::Installed),
            ("masked", UnitState::Masked),
            ("not-found", UnitState::Absent),
            ("bad-setting", UnitState::Installed),
        ] {
            answer.set(load_state);
            assert_eq!(herdr_unit_state(&ask, &[]), expected, "{load_state}");
        }
        assert_eq!(
            calls.borrow()[0],
            [
                "--user",
                "show",
                "--property=LoadState",
                "--value",
                "herdr.service"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unit_state_scans_the_dirs_when_systemd_cannot_be_asked() {
        let root = temp_root("scan");
        let empty = root.join("empty");
        let data = root.join("data/systemd/user");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let no_bus = |_: &[&str]| -> io::Result<SystemctlOutput> {
            Ok(SystemctlOutput {
                success: false,
                status: "exit status: 1".into(),
                stdout: String::new(),
                stderr: "Failed to connect to bus".into(),
            })
        };
        let missing = |_: &[&str]| -> io::Result<SystemctlOutput> {
            Err(io::Error::new(io::ErrorKind::NotFound, "systemctl"))
        };
        let dirs = [empty.clone(), data.clone()];
        assert_eq!(herdr_unit_state(&no_bus, &dirs), UnitState::Absent);
        std::fs::write(data.join(HERDR_UNIT), "[Service]\n").unwrap();
        assert_eq!(herdr_unit_state(&no_bus, &dirs), UnitState::Installed);
        assert_eq!(herdr_unit_state(&missing, &dirs), UnitState::Installed);
        std::fs::remove_file(data.join(HERDR_UNIT)).unwrap();
        std::os::unix::fs::symlink("/dev/null", empty.join(HERDR_UNIT)).unwrap();
        assert_eq!(herdr_unit_state(&no_bus, &dirs), UnitState::Masked);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unit_dirs_cover_every_user_search_path() {
        let env = |name: &str| -> Option<std::ffi::OsString> {
            match name {
                "HOME" => Some("/home/u".into()),
                "XDG_RUNTIME_DIR" => Some("/run/user/1000".into()),
                "XDG_DATA_DIRS" => Some("/opt/share:/usr/share".into()),
                _ => None,
            }
        };
        let dirs = unit_dirs(&env);
        for expected in [
            "/home/u/.config/systemd/user",
            "/etc/xdg/systemd/user",
            "/etc/systemd/user",
            "/run/user/1000/systemd/user",
            "/run/systemd/user",
            "/home/u/.local/share/systemd/user",
            "/opt/share/systemd/user",
            "/usr/share/systemd/user",
            "/usr/local/lib/systemd/user",
            "/usr/lib/systemd/user",
        ] {
            assert!(
                dirs.contains(&PathBuf::from(expected)),
                "{expected} missing from {dirs:?}"
            );
        }
        let xdg = |name: &str| -> Option<std::ffi::OsString> {
            match name {
                "HOME" => Some("/home/u".into()),
                "XDG_CONFIG_HOME" => Some("/cfg".into()),
                "XDG_DATA_HOME" => Some("/data".into()),
                _ => None,
            }
        };
        let dirs = unit_dirs(&xdg);
        assert!(dirs.contains(&PathBuf::from("/cfg/systemd/user")));
        assert!(dirs.contains(&PathBuf::from("/data/systemd/user")));
    }

    #[test]
    fn start_unit_runs_systemctl_user_start_herdr_service() {
        let calls = RefCell::new(Vec::new());
        let run = |args: &[&str]| {
            calls
                .borrow_mut()
                .push(args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
            ok("")
        };
        systemctl_start_unit(&run).unwrap();
        assert_eq!(calls.borrow()[0], ["--user", "start", "herdr.service"]);
        let refused = |_: &[&str]| -> io::Result<SystemctlOutput> {
            Ok(SystemctlOutput {
                success: false,
                status: "exit status: 1".into(),
                stdout: String::new(),
                stderr: "Start request repeated too quickly.".into(),
            })
        };
        let err = systemctl_start_unit(&refused).unwrap_err();
        assert!(err.to_string().contains("repeated too quickly"), "{err}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_host_wires_unit_state_and_start_through_systemctl() {
        thread_local! {
            static CALLS: RefCell<Vec<Vec<String>>> = const { RefCell::new(Vec::new()) };
        }
        fn fake(args: &[&str]) -> io::Result<SystemctlOutput> {
            CALLS.with(|calls| {
                calls
                    .borrow_mut()
                    .push(args.iter().map(|a| a.to_string()).collect())
            });
            ok(if args.contains(&"show") { "loaded" } else { "" })
        }
        let host = SystemHost { systemctl: fake };
        assert_eq!(host.unit_state(), UnitState::Installed);
        host.start_unit().unwrap();
        CALLS.with(|calls| {
            let calls = calls.borrow();
            assert_eq!(
                calls[0],
                [
                    "--user",
                    "show",
                    "--property=LoadState",
                    "--value",
                    "herdr.service"
                ]
            );
            assert_eq!(calls[1], ["--user", "start", "herdr.service"]);
        });
    }
}
