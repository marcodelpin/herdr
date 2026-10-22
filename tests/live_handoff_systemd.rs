#![cfg(target_os = "linux")]
//! A live handoff by a server that leads a systemd service (bd herdr-t6kr,
//! ADR-0018 in herdr-AGENT). A fake service manager plays systemd: it reads the
//! notification socket, takes `MAINPID=` only from the current main process (as
//! `NotifyAccess=main` does), completes barriers by closing their fds, and
//! writes the `systemctl show` answer the server reads through a test knob.

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid, wait_for_socket,
};

const UNIT_CGROUP: &str = "0::/test.slice/herdr-test.service\n";
const UNIT_CONTROL_GROUP: &str = "/test.slice/herdr-test.service";
const DECISION_DELAY_ENV: &str = "HERDR_TEST_HANDOFF_DECISION_DELAY_MS";

struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        support::stop_spawned_herdr(&mut *self.child);
        unregister_spawned_herdr_pid(pid);
    }
}

fn unique_test_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!("/tmp/hlhs-{}-{n}", std::process::id()))
}

fn spawn_server(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
    extra_env: &[(&str, &str)],
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::write(
        config_home.join("herdr/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    support::isolate_herdr_test_process(&mut cmd);
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket);
    cmd.env(
        "HERDR_CLIENT_SOCKET_PATH",
        runtime_dir.join("herdr-client.sock"),
    );
    cmd.env("SHELL", "/bin/sh");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn try_request(
    socket_path: &Path,
    request: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|err| format!("read timeout: {err}"))?;
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|err| format!("write {request}: {err}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| format!("read the answer to {request}: {err}"))?;
    if line.is_empty() {
        return Err(format!("empty answer to {request}"));
    }
    serde_json::from_str(&line).map_err(|err| format!("parse {line:?}: {err}"))
}

fn request(socket_path: &Path, request: serde_json::Value) -> serde_json::Value {
    try_request(socket_path, &request).unwrap_or_else(|err| panic!("{err}"))
}

fn handoff_request() -> serde_json::Value {
    serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}})
}

fn stop_request() -> serde_json::Value {
    serde_json::json!({"id":"test:stop","method":"server.stop","params":{}})
}

fn assert_ok(response: &serde_json::Value) {
    assert!(
        response.get("result").is_some(),
        "api request failed: {response}"
    );
}

fn assert_error_contains(response: &serde_json::Value, needle: &str) {
    assert!(
        response.get("error").is_some() && response.to_string().contains(needle),
        "expected an error naming {needle:?}, got {response}"
    );
}

fn ping_ok(socket_path: &Path) -> bool {
    try_request(
        socket_path,
        &serde_json::json!({"id":"test:ping","method":"ping","params":{}}),
    )
    .is_ok_and(|response| response.get("result").is_some())
}

fn wait_for_api(socket_path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !ping_ok(socket_path) {
        assert!(
            Instant::now() < deadline,
            "api did not answer at {}",
            socket_path.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// Gone or a zombie: a zombie has exited, only its parent has not reaped it.
fn process_gone(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => match stat.rsplit_once(") ") {
            Some((_, rest)) => rest.starts_with('Z') || rest.starts_with('X'),
            None => true,
        },
    }
}

fn wait_gone(pid: u32, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while !process_gone(pid) {
        assert!(Instant::now() < deadline, "{what} (pid {pid}) did not exit");
        thread::sleep(Duration::from_millis(20));
    }
}

fn live_server_pids(runtime_dir: &Path) -> Vec<u32> {
    support::herdr_server_pids_for_runtime_dir(runtime_dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|pid| !process_gone(*pid))
        .collect()
}

fn wait_for_server_pid_not_in(runtime_dir: &Path, known: &[u32], timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    loop {
        let pids = live_server_pids(runtime_dir);
        if let Some(pid) = pids.iter().copied().find(|pid| !known.contains(pid)) {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "no new server besides {known:?}; live server pids {pids:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_only_server(runtime_dir: &Path, pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let pids = live_server_pids(runtime_dir);
        if pids == [pid] {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected only server {pid}; live server pids {pids:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn shell_pid(api_socket: &Path, pane_id: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let info = request(
            api_socket,
            serde_json::json!({"id":"test:process-info","method":"pane.process_info","params":{"pane_id": pane_id}}),
        );
        if let Some(pid) = info["result"]["process_info"]["shell_pid"].as_u64() {
            return pid as u32;
        }
        assert!(
            Instant::now() < deadline,
            "no shell pid for {pane_id}: {info}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// A workspace created now is saved only by the final save: the session save
/// debounce is 5 s.
fn create_workspace(api_socket: &Path) -> (String, u32) {
    let created = request(
        api_socket,
        serde_json::json!({"id":"test:workspace:create","method":"workspace.create","params":{"cwd":"/tmp","focus":true}}),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("root pane id")
        .to_string();
    let shell = shell_pid(api_socket, &pane_id);
    (pane_id, shell)
}

fn saved_workspace_count(config_home: &Path) -> Option<usize> {
    let bytes = fs::read(config_home.join("herdr-dev/session.json")).ok()?;
    let session: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    session["workspaces"].as_array().map(Vec::len)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// Takes `MAINPID=` from the main process, as `NotifyAccess=main` does.
    Accept,
    /// Processes `MAINPID=` and keeps the main process: a refused hand-over.
    Ignore,
    /// Takes `MAINPID=` but answers `show` unreadably until the main pid is
    /// handed back to the first main process.
    AcceptThenHide,
}

struct ManagerState {
    first_main: u32,
    main: u32,
    notify_access: String,
    messages: Vec<(u32, String)>,
}

struct FakeManager {
    socket_path: PathBuf,
    show_file: PathBuf,
    state: Arc<Mutex<ManagerState>>,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeManager {
    fn start(dir: &Path, policy: Policy) -> Self {
        fs::create_dir_all(dir).unwrap();
        let socket_path = dir.join("notify.sock");
        let show_file = dir.join("systemd-show");
        let socket = UnixDatagram::bind(&socket_path).unwrap();
        let on: libc::c_int = 1;
        let set = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(set, 0, "SO_PASSCRED: {}", std::io::Error::last_os_error());
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let state = Arc::new(Mutex::new(ManagerState {
            first_main: 0,
            main: 0,
            notify_access: "main".into(),
            messages: Vec::new(),
        }));
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = state.clone();
            let done = done.clone();
            let show_file = show_file.clone();
            thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    let Some((sender, text, fds)) = recv_notification(&socket) else {
                        continue;
                    };
                    let mut state = state.lock().unwrap();
                    state.messages.push((sender, text.clone()));
                    for line in text.lines() {
                        let Some(pid) = line
                            .strip_prefix("MAINPID=")
                            .and_then(|pid| pid.parse::<u32>().ok())
                        else {
                            continue;
                        };
                        if sender != state.main || policy == Policy::Ignore {
                            continue;
                        }
                        state.main = pid;
                        let hidden = policy == Policy::AcceptThenHide && pid != state.first_main;
                        write_show(&show_file, &state, hidden);
                    }
                    drop(state);
                    // Closing a barrier fd completes the barrier: everything sent
                    // before it has been processed.
                    drop(fds);
                }
            })
        };
        Self {
            socket_path,
            show_file,
            state,
            done,
            thread: Some(thread),
        }
    }

    fn set_main(&self, pid: u32) {
        let mut state = self.state.lock().unwrap();
        state.first_main = pid;
        state.main = pid;
        write_show(&self.show_file, &state, false);
    }

    fn set_notify_access(&self, access: &str) {
        let mut state = self.state.lock().unwrap();
        state.notify_access = access.to_string();
        write_show(&self.show_file, &state, false);
    }

    fn main(&self) -> u32 {
        self.state.lock().unwrap().main
    }

    fn saw_main_pid(&self, sender: u32, pid: Option<u32>) -> bool {
        self.state
            .lock()
            .unwrap()
            .messages
            .iter()
            .any(|(from, text)| {
                *from == sender
                    && text.lines().any(|line| match pid {
                        Some(pid) => line == format!("MAINPID={pid}"),
                        None => line.starts_with("MAINPID="),
                    })
            })
    }

    fn main_pid_senders(&self, pid: u32) -> Vec<u32> {
        let needle = format!("MAINPID={pid}");
        self.state
            .lock()
            .unwrap()
            .messages
            .iter()
            .filter(|(_, text)| text.lines().any(|line| line == needle))
            .map(|(from, _)| *from)
            .collect()
    }
}

impl Drop for FakeManager {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn write_show(path: &Path, state: &ManagerState, hidden: bool) {
    let main_pid = if hidden {
        "unreadable".to_string()
    } else {
        state.main.to_string()
    };
    let tmp = path.with_extension("tmp");
    fs::write(
        &tmp,
        format!(
            "LoadState=loaded\nControlGroup={UNIT_CONTROL_GROUP}\nMainPID={main_pid}\nNotifyAccess={}\n",
            state.notify_access
        ),
    )
    .unwrap();
    fs::rename(&tmp, path).unwrap();
}

/// One notification with its sender pid (SO_PASSCRED) and passed fds.
fn recv_notification(socket: &UnixDatagram) -> Option<(u32, String, Vec<OwnedFd>)> {
    let mut buf = [0u8; 4096];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = [0u64; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    let read = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if read < 0 {
        return None;
    }
    let mut sender = 0;
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            match ((*cmsg).cmsg_level, (*cmsg).cmsg_type) {
                (libc::SOL_SOCKET, libc::SCM_CREDENTIALS) => {
                    let cred =
                        std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const libc::ucred);
                    sender = cred.pid as u32;
                }
                (libc::SOL_SOCKET, libc::SCM_RIGHTS) => {
                    let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let data = libc::CMSG_DATA(cmsg) as *const libc::c_int;
                    for index in 0..len / std::mem::size_of::<libc::c_int>() {
                        fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                            data.add(index),
                        )));
                    }
                }
                _ => {}
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Some((
        sender,
        String::from_utf8_lossy(&buf[..read as usize]).into_owned(),
        fds,
    ))
}

/// A server that leads the fake unit, with one workspace whose pane runs a shell.
struct Supervised {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    api_socket: PathBuf,
    manager: FakeManager,
    server: Option<SpawnedHerdr>,
    old_pid: u32,
    pane_id: String,
    shell_pid: u32,
}

fn start_supervised(policy: Policy, notify: bool, extra_env: &[(&str, &str)]) -> Supervised {
    let base = unique_test_dir();
    let _ = fs::remove_dir_all(&base);
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let manager = FakeManager::start(&base.join("manager"), policy);
    let notify_socket = manager.socket_path.display().to_string();
    let show_file = manager.show_file.display().to_string();
    let mut env = vec![
        ("HERDR_TEST_SYSTEMD_CGROUP", UNIT_CGROUP),
        ("HERDR_TEST_SYSTEMD_SHOW_FILE", show_file.as_str()),
    ];
    if notify {
        env.push(("NOTIFY_SOCKET", notify_socket.as_str()));
    }
    env.extend_from_slice(extra_env);
    let server = spawn_server(&config_home, &runtime_dir, &api_socket, &env);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let old_pid = server.child.process_id().expect("server pid");
    manager.set_main(old_pid);
    let (pane_id, shell_pid) = create_workspace(&api_socket);
    Supervised {
        base,
        config_home,
        runtime_dir,
        api_socket,
        manager,
        server: Some(server),
        old_pid,
        pane_id,
        shell_pid,
    }
}

impl Supervised {
    fn assert_panes_kept(&self) {
        assert!(
            !process_gone(self.shell_pid),
            "the pane shell {} died",
            self.shell_pid
        );
        assert_eq!(shell_pid(&self.api_socket, &self.pane_id), self.shell_pid);
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        let pids = live_server_pids(&self.runtime_dir);
        let _ = try_request(&self.api_socket, &stop_request());
        let deadline = Instant::now() + Duration::from_secs(10);
        for pid in pids {
            while !process_gone(pid) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if !process_gone(pid) {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
        self.server.take();
        cleanup_test_base(&self.base);
    }
}

/// ADR-0018 D2 commit: systemd takes the import server as main process, the
/// panes survive, and the new main process hands off again (a chain).
#[test]
fn a_unit_lead_hands_the_main_pid_to_the_import_server_and_keeps_the_panes() {
    let fx = start_supervised(Policy::Accept, true, &[]);
    assert_ok(&request(&fx.api_socket, handoff_request()));
    let second =
        wait_for_server_pid_not_in(&fx.runtime_dir, &[fx.old_pid], Duration::from_secs(10));
    assert_eq!(
        fx.manager.main(),
        second,
        "systemd must name the import server"
    );
    assert!(fx.manager.saw_main_pid(fx.old_pid, Some(second)));
    wait_gone(fx.old_pid, Duration::from_secs(5), "the old server");
    wait_for_api(&fx.api_socket, Duration::from_secs(10));
    fx.assert_panes_kept();

    assert_ok(&request(&fx.api_socket, handoff_request()));
    let third = wait_for_server_pid_not_in(
        &fx.runtime_dir,
        &[fx.old_pid, second],
        Duration::from_secs(10),
    );
    assert_eq!(fx.manager.main(), third);
    assert!(fx.manager.saw_main_pid(second, Some(third)));
    wait_gone(second, Duration::from_secs(5), "the second server");
    wait_for_api(&fx.api_socket, Duration::from_secs(10));
    fx.assert_panes_kept();
}

/// ADR-0018 D2 abort: systemd processed the hand-over and kept the old server
/// as main process, so the old server keeps the panes and serves again.
#[test]
fn a_hand_over_that_systemd_refuses_leaves_the_panes_with_the_old_server() {
    let fx = start_supervised(Policy::Ignore, true, &[]);
    assert_error_contains(
        &request(&fx.api_socket, handoff_request()),
        "did not take the import server",
    );
    assert_eq!(fx.manager.main(), fx.old_pid);
    wait_for_only_server(&fx.runtime_dir, fx.old_pid, Duration::from_secs(10));
    wait_for_api(&fx.api_socket, Duration::from_secs(10));
    fx.assert_panes_kept();
}

/// codex t6kr-r3 #2 and #3 by design (ADR-0018 D2 rollback): nothing commits
/// while systemd's answer is unknown; the import server hands the main pid
/// back, and the old server keeps the panes.
#[test]
fn an_unreadable_answer_rolls_the_main_pid_back_through_the_import_server() {
    let fx = start_supervised(Policy::AcceptThenHide, true, &[]);
    assert_error_contains(
        &request(&fx.api_socket, handoff_request()),
        "rolled back through the import server",
    );
    assert_eq!(fx.manager.main(), fx.old_pid, "the main pid must come back");
    let senders = fx.manager.main_pid_senders(fx.old_pid);
    assert!(
        senders
            .iter()
            .any(|sender| *sender != fx.old_pid && *sender != 0),
        "the import server must send the rollback, senders {senders:?}"
    );
    wait_for_only_server(&fx.runtime_dir, fx.old_pid, Duration::from_secs(10));
    wait_for_api(&fx.api_socket, Duration::from_secs(10));
    fx.assert_panes_kept();
}

/// codex t6kr-r3 #2 and #3 (ADR-0018 D2 step 5 and D3): a stop signal that
/// reaches the old server while it decides is forwarded to the replacement,
/// which saves the session and stops; nothing is left behind that could stop a
/// fresh server started later on the same socket.
#[test]
fn a_stop_signal_during_the_decision_is_forwarded_to_the_replacement() {
    let fx = start_supervised(Policy::Accept, true, &[(DECISION_DELAY_ENV, "1500")]);
    let api_socket = fx.api_socket.clone();
    let handoff = thread::spawn(move || request(&api_socket, handoff_request()));
    let deadline = Instant::now() + Duration::from_secs(15);
    let replacement = loop {
        let main = fx.manager.main();
        if main != fx.old_pid {
            break main;
        }
        assert!(
            Instant::now() < deadline,
            "the old server never handed the main pid over"
        );
        thread::sleep(Duration::from_millis(10));
    };
    // The read-back is still pending: the old server is deciding.
    assert_eq!(
        unsafe { libc::kill(fx.old_pid as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert_ok(&handoff.join().expect("handoff thread"));
    wait_gone(fx.old_pid, Duration::from_secs(5), "the old server");
    wait_gone(
        replacement,
        Duration::from_secs(10),
        "the replacement after the forwarded stop",
    );
    assert_eq!(
        saved_workspace_count(&fx.config_home),
        Some(1),
        "the replacement must save the session it stops with"
    );

    let fresh = spawn_server(&fx.config_home, &fx.runtime_dir, &fx.api_socket, &[]);
    wait_for_api(&fx.api_socket, Duration::from_secs(10));
    let fresh_pid = fresh.child.process_id().expect("fresh server pid");
    thread::sleep(Duration::from_secs(2));
    assert!(!process_gone(fresh_pid), "the fresh server was stopped");
    assert!(ping_ok(&fx.api_socket));
    assert_ok(&request(&fx.api_socket, stop_request()));
    wait_gone(fresh_pid, Duration::from_secs(10), "the fresh server");
}

/// ADR-0018 D2 abort with a stop: a `server stop` that reached the import
/// server (it owns the socket path during the decision) comes back in its
/// abort reply, and the old server, which kept the panes, saves and stops.
#[test]
fn a_stop_received_by_the_import_server_before_an_abort_stops_the_old_server() {
    let fx = start_supervised(Policy::Ignore, true, &[(DECISION_DELAY_ENV, "1500")]);
    let api_socket = fx.api_socket.clone();
    let handoff = thread::spawn(move || request(&api_socket, handoff_request()));
    let deadline = Instant::now() + Duration::from_secs(15);
    while !fx.manager.saw_main_pid(fx.old_pid, None) {
        assert!(
            Instant::now() < deadline,
            "the old server never asked for the hand-over"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_ok(&request(&fx.api_socket, stop_request()));
    assert_error_contains(
        &handoff.join().expect("handoff thread"),
        "did not take the import server",
    );
    wait_gone(
        fx.old_pid,
        Duration::from_secs(15),
        "the old server after the stop its import server received",
    );
    assert_eq!(saved_workspace_count(&fx.config_home), Some(1));
}

/// herdr-brrh (ADR-0018 D1): the main process of a unit that takes no
/// notifications refuses the handoff before pausing anything, because the unit
/// would end every pane together with it.
#[test]
fn a_unit_lead_without_notify_access_refuses_the_handoff() {
    // Today's unit: NotifyAccess=none, and so no NOTIFY_SOCKET.
    let fx = start_supervised(Policy::Accept, false, &[]);
    fx.manager.set_notify_access("none");
    assert_error_contains(
        &request(&fx.api_socket, handoff_request()),
        "NotifyAccess=none",
    );
    wait_for_only_server(&fx.runtime_dir, fx.old_pid, Duration::from_secs(5));
    fx.assert_panes_kept();
    drop(fx);

    // The drop-in is installed but the unit was not restarted since.
    let fx = start_supervised(Policy::Accept, false, &[]);
    assert_error_contains(
        &request(&fx.api_socket, handoff_request()),
        "has no NOTIFY_SOCKET",
    );
    wait_for_only_server(&fx.runtime_dir, fx.old_pid, Duration::from_secs(5));
    fx.assert_panes_kept();
}

/// herdr-whd2 (ADR-0018 D4): the final session save is on disk before the api
/// socket stops answering. `herdr server stop` returns then, and after a
/// handoff systemd kills the main process right after it returns.
#[test]
fn the_final_session_save_lands_before_the_api_socket_stops_answering() {
    let base = unique_test_dir();
    let _ = fs::remove_dir_all(&base);
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let server = spawn_server(&config_home, &runtime_dir, &api_socket, &[]);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let pid = server.child.process_id().expect("server pid");
    create_workspace(&api_socket);
    assert_ne!(saved_workspace_count(&config_home), Some(1));

    assert_ok(&request(&api_socket, stop_request()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&api_socket).is_ok() {
        assert!(Instant::now() < deadline, "the api socket kept answering");
        thread::sleep(Duration::from_millis(2));
    }
    // Freeze the server where `herdr server stop` returns.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGSTOP);
    }
    let saved = saved_workspace_count(&config_home);
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGCONT);
    }
    assert_eq!(
        saved,
        Some(1),
        "the final save must be on disk when the api socket stops answering"
    );
    wait_gone(pid, Duration::from_secs(10), "the server");
    drop(server);
    cleanup_test_base(&base);
}
