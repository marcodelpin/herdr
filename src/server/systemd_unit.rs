//! Whether this server leads a systemd service, and the main-pid hand-over a
//! live handoff needs under one (bd herdr-t6kr; ADR-0018 in herdr-AGENT).
//!
//! Under a `Type=simple` unit the server that runs a live handoff is the
//! unit's main process. When it exits, systemd stops the unit and kills the
//! import server and every pane left in its cgroup. So before the source
//! commits, it asks systemd to take the import server as main process
//! (`MAINPID=` on `NOTIFY_SOCKET`, honoured with `NotifyAccess=main`) and reads
//! the unit's `MainPID` back: the handoff commits only when systemd names the
//! import server. A barrier orders a message but never says whether systemd
//! accepted it, so the read-back alone decides.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixDatagram;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Environment variable systemd sets to the unit's notification socket.
pub(crate) const NOTIFY_SOCKET_ENV_VAR: &str = "NOTIFY_SOCKET";
/// Test stand-in for `/proc/self/cgroup`: integration tests do not run in a unit.
pub(crate) const TEST_CGROUP_ENV_VAR: &str = "HERDR_TEST_SYSTEMD_CGROUP";
/// Test stand-in for `systemctl show`: a file holding its `Key=Value` output.
pub(crate) const TEST_SHOW_FILE_ENV_VAR: &str = "HERDR_TEST_SYSTEMD_SHOW_FILE";
/// Test knob: milliseconds to wait between the hand-over and its read-back.
pub(crate) const TEST_DECISION_DELAY_ENV_VAR: &str = "HERDR_TEST_HANDOFF_DECISION_DELAY_MS";

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const SHOW_TIMEOUT: Duration = Duration::from_secs(5);
const READ_BACK_ATTEMPTS: u32 = 3;
const READ_BACK_RETRY_DELAY: Duration = Duration::from_millis(200);
const SHOW_PROPERTIES: [&str; 4] = ["LoadState", "ControlGroup", "MainPID", "NotifyAccess"];

/// What the supervision probe found before a handoff pauses anything.
pub(crate) enum Supervision {
    /// Not the main process of a systemd service: hand off as upstream does.
    Unsupervised,
    /// The main process of a unit that accepts notifications.
    Supervised(UnitLead),
}

/// The outcome of a main-pid hand-over, as systemd reports it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandOver {
    /// systemd names the import server as main process: commit.
    Accepted,
    /// systemd processed the request and still names this server: abort.
    Refused,
    /// systemd's answer is unknown: roll back through the import server.
    Unknown(String),
}

/// A service unit as systemd names it, and the cgroup systemd gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnitRef {
    name: String,
    /// The uid of the user manager that owns the unit; `None` for the system manager.
    user_manager_uid: Option<u32>,
    control_group: String,
}

#[derive(Debug, PartialEq, Eq)]
struct UnitState {
    main_pid: u32,
    notify_access: String,
}

/// A server that is the main process of its unit, with a notification socket
/// that answered a barrier.
pub(crate) struct UnitLead {
    unit: UnitRef,
    notifier: Notifier,
}

/// ADR-0018 D1. `Ok(Unsupervised)` when this server does not lead a service
/// unit; an error when it does but a handoff could not move the main pid (the
/// unit would end every pane with this server), or when systemd cannot say.
pub(crate) fn probe() -> io::Result<Supervision> {
    let cgroup = match std::env::var(TEST_CGROUP_ENV_VAR) {
        Ok(text) if !text.is_empty() => text,
        _ => match std::fs::read_to_string("/proc/self/cgroup") {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Supervision::Unsupervised);
            }
            Err(err) => return Err(err),
        },
    };
    let Some(unit) = unit_of_cgroup(&cgroup) else {
        return Ok(Supervision::Unsupervised);
    };
    let state = unit.state().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "cannot tell whether this server leads {}: {err}",
                unit.describe()
            ),
        )
    })?;
    if state.main_pid != std::process::id() {
        tracing::info!(
            unit = %unit.describe(),
            main_pid = state.main_pid,
            "this server is not the main process of its unit; handing off unsupervised"
        );
        return Ok(Supervision::Unsupervised);
    }
    if !matches!(state.notify_access.as_str(), "main" | "all") {
        return Err(io::Error::other(format!(
            "this server is the main process of {}, which has NotifyAccess={}: the unit would end every pane together with this server; install the live-handoff drop-in (NotifyAccess=main) and restart the unit once",
            unit.describe(),
            if state.notify_access.is_empty() {
                "<unknown>"
            } else {
                state.notify_access.as_str()
            }
        )));
    }
    let socket = std::env::var_os(NOTIFY_SOCKET_ENV_VAR)
        .filter(|socket| !socket.is_empty())
        .ok_or_else(|| {
            io::Error::other(format!(
                "this server is the main process of {} but has no {NOTIFY_SOCKET_ENV_VAR}: it started before the unit accepted notifications; restart the unit once",
                unit.describe()
            ))
        })?;
    let notifier = Notifier::connect(&socket).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "the systemd notification socket of {} does not answer: {err}",
                unit.describe()
            ),
        )
    })?;
    Ok(Supervision::Supervised(UnitLead { unit, notifier }))
}

impl UnitLead {
    pub(crate) fn describe(&self) -> String {
        self.unit.describe()
    }

    /// ADR-0018 D2 steps 3-4: asks systemd to take `pid` as the unit's main
    /// process and reads the answer back.
    pub(crate) fn hand_main_pid_to(&self, pid: u32) -> HandOver {
        let sent = self.notifier.send_main_pid(pid);
        if let Err(err) = &sent {
            tracing::warn!(
                unit = %self.describe(),
                pid,
                err = %err,
                "the MAINPID hand-over was not confirmed by a barrier; reading the main pid back"
            );
        }
        test_decision_delay();
        let own = std::process::id();
        let mut last = String::new();
        for attempt in 1..=READ_BACK_ATTEMPTS {
            match self.unit.state() {
                Ok(state) if state.main_pid == pid => return HandOver::Accepted,
                // Without a completed barrier the request may still be queued, so
                // this server being named is not yet a refusal.
                Ok(state) if state.main_pid == own && sent.is_ok() => return HandOver::Refused,
                Ok(state) => {
                    last = format!(
                        "systemd names pid {} as the main process of {}",
                        state.main_pid,
                        self.describe()
                    );
                }
                Err(err) => last = err.to_string(),
            }
            if attempt < READ_BACK_ATTEMPTS {
                std::thread::sleep(READ_BACK_RETRY_DELAY);
            }
        }
        match sent {
            Ok(()) => HandOver::Unknown(last),
            Err(err) => HandOver::Unknown(format!("{last}; the hand-over barrier failed: {err}")),
        }
    }
}

/// Sent by an import server that its source rolls back (ADR-0018 D2 step 4):
/// if systemd took this process as main, only it can give the main pid back
/// (`NotifyAccess=main`); if systemd did not, systemd drops the message. Either
/// way `pid` is the main process once the barrier completes.
pub(crate) fn send_main_pid_rollback(pid: u32) -> io::Result<()> {
    let socket = std::env::var_os(NOTIFY_SOCKET_ENV_VAR)
        .filter(|socket| !socket.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{NOTIFY_SOCKET_ENV_VAR} is not set"),
            )
        })?;
    Notifier::connect(&socket)?.send_main_pid(pid)
}

fn test_decision_delay() {
    let Some(millis) = std::env::var(TEST_DECISION_DELAY_ENV_VAR)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    std::thread::sleep(Duration::from_millis(millis));
}

impl UnitRef {
    fn describe(&self) -> String {
        match self.user_manager_uid {
            Some(uid) => format!("{} (user manager of uid {uid})", self.name),
            None => self.name.clone(),
        }
    }

    fn state(&self) -> io::Result<UnitState> {
        let text = match std::env::var_os(TEST_SHOW_FILE_ENV_VAR) {
            Some(path) if !path.is_empty() => std::fs::read_to_string(path)?,
            _ => systemctl_show(self)?,
        };
        self.verify(&parse_show(&text))
    }

    /// The unit's state, accepted only from the unit that owns this server's
    /// cgroup: a wrong unit name reads as `LoadState=not-found` and `MainPID=0`,
    /// which must never look like "another process is main" (codex t6kr-r3 #4).
    fn verify(&self, show: &HashMap<String, String>) -> io::Result<UnitState> {
        let field = |key: &str| show.get(key).map(String::as_str).unwrap_or("");
        if field("LoadState") != "loaded" || field("ControlGroup") != self.control_group {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "systemd's {} is not the unit of this server's cgroup {} (LoadState={:?}, ControlGroup={:?})",
                    self.describe(),
                    self.control_group,
                    field("LoadState"),
                    field("ControlGroup")
                ),
            ));
        }
        let main_pid = field("MainPID").parse::<u32>().map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unexpected MainPID {:?} for {}: {err}",
                    field("MainPID"),
                    self.describe()
                ),
            )
        })?;
        Ok(UnitState {
            main_pid,
            notify_access: field("NotifyAccess").to_string(),
        })
    }
}

fn parse_show(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// The service unit named by a cgroup v2 `/proc/<pid>/cgroup` text.
fn unit_of_cgroup(cgroup: &str) -> Option<UnitRef> {
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let unit_index = segments
        .iter()
        .rposition(|segment| segment.ends_with(".service"))?;
    let user_manager_uid = segments[..unit_index].iter().find_map(|segment| {
        cg_unescape(segment)
            .strip_prefix("user@")
            .and_then(|rest| rest.strip_suffix(".service"))
            .and_then(|uid| uid.parse::<u32>().ok())
    });
    Some(UnitRef {
        name: cg_unescape(segments[unit_index]).to_string(),
        user_manager_uid,
        control_group: format!("/{}", segments[..=unit_index].join("/")),
    })
}

/// systemd's `cg_unescape`: a cgroup name that could clash with a kernel file
/// (a leading `_` or `.`, a controller prefix, `tasks`) is written with one
/// extra leading `_`, so the unit `_herdr.service` lives in `__herdr.service`.
fn cg_unescape(component: &str) -> &str {
    component.strip_prefix('_').unwrap_or(component)
}

/// `systemctl [--user] show <unit> -p ...`, bounded by [`SHOW_TIMEOUT`]. A user
/// unit is asked of the manager of the uid in its cgroup path, through that
/// user's runtime dir, whatever this process's own environment says.
fn systemctl_show(unit: &UnitRef) -> io::Result<String> {
    let mut command = Command::new("systemctl");
    if let Some(uid) = unit.user_manager_uid {
        command
            .arg("--user")
            .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
            .env_remove("DBUS_SESSION_BUS_ADDRESS");
    }
    command.arg("show").arg(&unit.name);
    for property in SHOW_PROPERTIES {
        command.arg("-p").arg(property);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("cannot run systemctl: {err}")))?;
    let deadline = Instant::now() + SHOW_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "systemctl show {} did not answer within {} ms",
                    unit.name,
                    SHOW_TIMEOUT.as_millis()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut stdout)?;
    }
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut stderr);
        }
        return Err(io::Error::other(format!(
            "systemctl show {} failed ({status}): {}",
            unit.name,
            stderr.trim()
        )));
    }
    Ok(stdout)
}

/// A datagram connection to the unit's notification socket.
struct Notifier {
    socket: UnixDatagram,
}

impl Notifier {
    /// Connects and proves with a barrier that the manager reads the socket.
    fn connect(address: &OsStr) -> io::Result<Self> {
        let bytes = address.as_bytes();
        if bytes.first() != Some(&b'/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported {NOTIFY_SOCKET_ENV_VAR} address {address:?} (only a socket path is supported)"),
            ));
        }
        let socket = UnixDatagram::unbound()?;
        socket.connect(std::path::Path::new(address))?;
        let notifier = Self { socket };
        notifier.barrier()?;
        Ok(notifier)
    }

    fn send_main_pid(&self, pid: u32) -> io::Result<()> {
        self.socket.send(format!("MAINPID={pid}\n").as_bytes())?;
        self.barrier()
    }

    /// `BARRIER=1` with the write end of a pipe: the manager closes it once it
    /// has processed every message it received before.
    fn barrier(&self) -> io::Result<()> {
        let (read_end, write_end) = cloexec_pipe()?;
        send_with_fd(&self.socket, b"BARRIER=1\n", write_end.as_raw_fd())?;
        drop(write_end);
        wait_for_hangup(&read_end, BARRIER_TIMEOUT)
    }
}

fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe() just returned these two descriptors and nothing else owns them.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read_end, &write_end] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read_end, write_end))
}

fn send_with_fd(socket: &UnixDatagram, payload: &[u8], fd: libc::c_int) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut libc::c_void,
        iov_len: payload.len(),
    };
    let fd_size = std::mem::size_of::<libc::c_int>() as u32;
    let space = unsafe { libc::CMSG_SPACE(fd_size) } as usize;
    // u64 cells keep the control buffer aligned for `cmsghdr`.
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: an all-zero msghdr is a valid empty message header.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer is CMSG_SPACE(sizeof(int)) bytes, enough for one header and one fd.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("cannot build the barrier control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut libc::c_int, fd);
    }
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait_for_hangup(read_end: &OwnedFd, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut poll_fd = libc::pollfd {
            fd: read_end.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        let ready = unsafe { libc::poll(&mut poll_fd, 1, millis) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ready > 0 && poll_fd.revents & (libc::POLLHUP | libc::POLLIN | libc::POLLERR) != 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the systemd manager did not answer a notification barrier within {} ms",
                    timeout.as_millis()
                ),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(
        load: &str,
        control_group: &str,
        main_pid: &str,
        access: &str,
    ) -> HashMap<String, String> {
        parse_show(&format!(
            "LoadState={load}\nControlGroup={control_group}\nMainPID={main_pid}\nNotifyAccess={access}\n"
        ))
    }

    #[test]
    fn a_user_unit_cgroup_names_its_unit_and_user_manager() {
        let unit = unit_of_cgroup(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/herdr.service\n",
        )
        .expect("unit");
        assert_eq!(unit.name, "herdr.service");
        assert_eq!(unit.user_manager_uid, Some(1000));
        assert_eq!(
            unit.control_group,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/herdr.service"
        );
    }

    #[test]
    fn an_escaped_cgroup_component_names_the_unescaped_unit() {
        // codex t6kr-r3 #4: systemd writes the unit _herdr.service as the cgroup
        // __herdr.service; asking systemd about __herdr.service reads MainPID 0.
        let unit = unit_of_cgroup(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/__herdr.service\n",
        )
        .expect("unit");
        assert_eq!(unit.name, "_herdr.service");
        assert_eq!(
            unit.control_group,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/__herdr.service"
        );
    }

    #[test]
    fn a_system_unit_and_a_delegated_sub_cgroup_name_their_service() {
        let unit = unit_of_cgroup("0::/system.slice/herdr.service\n").expect("unit");
        assert_eq!(
            (unit.name.as_str(), unit.user_manager_uid),
            ("herdr.service", None)
        );
        let unit = unit_of_cgroup("0::/system.slice/herdr.service/payload\n").expect("unit");
        assert_eq!(unit.name, "herdr.service");
        assert_eq!(unit.control_group, "/system.slice/herdr.service");
    }

    #[test]
    fn a_scope_the_root_or_a_v1_only_text_is_no_unit() {
        assert_eq!(
            unit_of_cgroup("0::/user.slice/user-1000.slice/session-4.scope\n"),
            None
        );
        assert_eq!(unit_of_cgroup("0::/\n"), None);
        assert_eq!(
            unit_of_cgroup("1:name=systemd:/system.slice/x.service\n"),
            None
        );
    }

    #[test]
    fn the_unit_state_is_read_only_from_the_unit_of_this_cgroup() {
        let unit = unit_of_cgroup("0::/system.slice/herdr.service\n").expect("unit");
        assert_eq!(
            unit.verify(&show("loaded", "/system.slice/herdr.service", "42", "main"))
                .expect("state"),
            UnitState {
                main_pid: 42,
                notify_access: "main".into()
            }
        );
        // A wrong name: systemd answers for a unit it does not know.
        assert!(unit.verify(&show("not-found", "", "0", "none")).is_err());
        // Another unit's cgroup.
        assert!(unit
            .verify(&show("loaded", "/system.slice/other.service", "42", "main"))
            .is_err());
        assert!(unit
            .verify(&show("loaded", "/system.slice/herdr.service", "", "main"))
            .is_err());
    }

    fn recv_with_fds(socket: &UnixDatagram) -> (String, Vec<OwnedFd>) {
        let mut buf = [0u8; 512];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut control = [0u64; 32];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        let read = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) };
        assert!(read >= 0, "recvmsg: {}", io::Error::last_os_error());
        let mut fds = Vec::new();
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const libc::c_int);
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        (
            String::from_utf8_lossy(&buf[..read as usize]).into_owned(),
            fds,
        )
    }

    #[test]
    fn the_main_pid_and_its_barrier_reach_the_manager_in_order() {
        // Not temp_dir(): a long TMPDIR would exceed the socket path limit.
        let dir =
            std::path::PathBuf::from("/tmp").join(format!("herdr-notify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("notify.sock");
        let manager = UnixDatagram::bind(&path).expect("bind");
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let (text, fds) = recv_with_fds(&manager);
                seen.push((text, fds.len()));
                drop(fds);
            }
            seen
        });
        let notifier = Notifier::connect(path.as_os_str()).expect("connect");
        notifier.send_main_pid(4242).expect("hand over");
        let seen = handle.join().expect("manager");
        assert_eq!(
            seen,
            vec![
                ("BARRIER=1\n".to_string(), 1),
                ("MAINPID=4242\n".to_string(), 0),
                ("BARRIER=1\n".to_string(), 1)
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_abstract_or_relative_socket_address_is_refused() {
        assert!(Notifier::connect(OsStr::new("@herdr-test")).is_err());
        assert!(Notifier::connect(OsStr::new("relative.sock")).is_err());
    }
}
