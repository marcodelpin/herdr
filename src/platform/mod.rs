//! Platform-specific process and filesystem operations.
//!
//! Centralizes OS-dependent behavior behind a clean boundary so core
//! modules don't scatter `#[cfg]` branches through product logic.

#[cfg(unix)]
pub(crate) mod ssh_agent;

pub(crate) struct HostShutdownMonitor {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HostShutdownMonitor {
    pub(crate) fn start(
        requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let task = monitor_host_shutdown(requested, wake);
        Self { task }
    }
}

impl Drop for HostShutdownMonitor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn monitor_host_shutdown(
    _requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _wake: impl Fn() + Send + Sync + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    None
}

#[cfg(not(windows))]
pub(crate) fn host_shutdown_in_progress() -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub name: String,
    pub argv0: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cmdline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundJob {
    pub process_group_id: u32,
    pub processes: Vec<ForegroundProcess>,
}

/// A request from outside the process to stop the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerQuitSignal {
    #[cfg(unix)]
    Interrupt,
    #[cfg(unix)]
    Terminate,
    #[cfg(not(unix))]
    ConsoleControl,
}

impl std::fmt::Display for ServerQuitSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            #[cfg(unix)]
            Self::Interrupt => "SIGINT",
            #[cfg(unix)]
            Self::Terminate => "SIGTERM",
            #[cfg(not(unix))]
            Self::ConsoleControl => "console control event",
        })
    }
}

#[cfg(not(unix))]
pub(crate) fn spawn_server_signal_monitor(
    on_quit: impl Fn(ServerQuitSignal) + Send + Sync + 'static,
) {
    if let Err(err) = ctrlc::set_handler(move || on_quit(ServerQuitSignal::ConsoleControl)) {
        tracing::warn!(%err, "failed to install server stop handler");
    }
}

#[cfg(not(unix))]
pub(crate) fn ignore_server_hangup() {}

#[cfg(not(unix))]
pub(crate) fn local_stream_peer_description(_stream: &crate::ipc::LocalStream) -> Option<String> {
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Terminate,
    Kill,
}

/// Why a pane runtime ended, before application persistence policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitReason {
    Exited,
    Interrupted,
    /// Imported runtimes have no child wait handle in the replacement server.
    #[cfg(unix)]
    Handoff,
    WaitFailed,
}

impl ChildExitReason {
    pub(crate) fn requires_session_checkpoint(self) -> bool {
        match self {
            Self::Interrupted => true,
            #[cfg(unix)]
            Self::Handoff => true,
            _ => false,
        }
    }
}

#[cfg(unix)]
pub(crate) use unix_common::{
    classify_child_exit, poll_fd_readable, read_fd, shared_ssh_control_path,
};

#[cfg(not(any(unix, windows)))]
pub(crate) fn classify_child_exit(_status: &portable_pty::ExitStatus) -> ChildExitReason {
    ChildExitReason::Exited
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_executable() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()
}

pub(crate) fn detached_custom_command_process(command: &str) -> std::process::Command {
    let mut process = detached_custom_command_process_platform(command);
    configure_background_command(&mut process);
    process
}

pub(crate) fn pane_custom_command_pty_builder(command: &str) -> portable_pty::CommandBuilder {
    pane_custom_command_pty_builder_platform(command)
}

pub(crate) fn apply_pane_runtime_marker(command: &mut portable_pty::CommandBuilder) {
    apply_pane_runtime_marker_platform(command);
}

pub(crate) fn prepare_paste_text_for_pty(text: String) -> String {
    prepare_paste_text_for_pty_platform(text)
}

pub(crate) fn plugin_runtime_path(path: &std::path::Path) -> std::path::PathBuf {
    plugin_runtime_path_platform(path)
}

pub(crate) fn normalize_cwd_for_launch(path: &std::path::Path) -> std::path::PathBuf {
    normalize_cwd_for_launch_platform(path)
}

#[cfg(not(windows))]
fn normalize_cwd_for_launch_platform(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

#[cfg(not(windows))]
fn plugin_runtime_path_platform(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

#[cfg(not(windows))]
fn prepare_paste_text_for_pty_platform(text: String) -> String {
    text
}

#[cfg(not(windows))]
pub(crate) fn terminal_title_for_presentation(title: &str) -> &str {
    title
}

#[cfg(not(windows))]
fn apply_pane_runtime_marker_platform(_command: &mut portable_pty::CommandBuilder) {}

pub(crate) fn configure_background_command(command: &mut std::process::Command) {
    configure_background_command_platform(command);
}

#[cfg(not(windows))]
fn configure_background_command_platform(_command: &mut std::process::Command) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlatformCapabilities {
    pub(crate) live_handoff: bool,
    pub(crate) direct_terminal_attach: bool,
    pub(crate) preserve_legacy_doubled_escape_input: bool,
}

pub(crate) const fn capabilities() -> PlatformCapabilities {
    PlatformCapabilities {
        live_handoff: cfg!(unix),
        direct_terminal_attach: cfg!(unix),
        preserve_legacy_doubled_escape_input: cfg!(target_os = "macos"),
    }
}

/// The byte the host terminal sends for Backspace according to the tty's erase
/// setting (e.g. `^H` for MobaXterm and PuTTY-style terminals), when known.
pub(crate) fn terminal_erase_byte() -> Option<u8> {
    #[cfg(unix)]
    return unix_common::terminal_erase_byte();
    #[cfg(not(unix))]
    None
}

pub(crate) fn terminal_grid_size() -> std::io::Result<(u16, u16)> {
    #[cfg(unix)]
    let (cols, rows) = unix_common::read_terminal_grid_size()?;
    #[cfg(windows)]
    let (cols, rows) = windows::read_terminal_grid_size()?;
    #[cfg(not(any(unix, windows)))]
    let (cols, rows) = fallback::read_terminal_grid_size()?;

    if cols == 0 || rows == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal reported a zero-sized grid",
        ));
    }
    Ok((cols, rows))
}

#[cfg(not(windows))]
pub fn launch_server_daemon_command(command: &mut std::process::Command) -> std::io::Result<u32> {
    command.spawn().map(|child| child.id())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn prepare_server_process(_handoff_import: bool) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn detach_server_daemon_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    #[cfg(target_os = "macos")]
    macos::configure_server_daemon_context(command);

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn current_process_is_detached_server_daemon() -> bool {
    unsafe { libc::getsid(0) == libc::getpid() }
}

/// Raised by the SIGWINCH handler, consumed by the host resize watcher.
#[cfg(unix)]
static TERMINAL_RESIZE_SIGNALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn record_terminal_resize_signal(_signal: libc::c_int) {
    TERMINAL_RESIZE_SIGNALLED.store(true, std::sync::atomic::Ordering::Release);
}

/// Records SIGWINCH events that size polling can miss.
#[cfg(unix)]
pub(crate) fn watch_terminal_resize_signal() {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction =
        record_terminal_resize_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // Keep blocking stdin and socket reads from failing with EINTR.
    action.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
pub(crate) fn watch_terminal_resize_signal() {}

/// Returns whether a terminal size change was signalled since the last call.
#[cfg(unix)]
pub(crate) fn take_terminal_resize_signal() -> bool {
    TERMINAL_RESIZE_SIGNALLED.swap(false, std::sync::atomic::Ordering::AcqRel)
}

/// Windows relies on size polling.
#[cfg(not(unix))]
pub(crate) fn take_terminal_resize_signal() -> bool {
    false
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LimitedRead {
    Empty,
    Complete(Vec<u8>),
    Oversized,
}

pub(crate) fn read_limited_reader(
    mut reader: impl std::io::Read,
    max_bytes: usize,
) -> std::io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    while bytes.len() < max_bytes {
        let remaining = max_bytes - bytes.len();
        let read_len = remaining.min(buffer.len());
        let bytes_read = match reader.read(&mut buffer[..read_len]) {
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if bytes_read == 0 {
            return if bytes.is_empty() {
                Ok(LimitedRead::Empty)
            } else {
                Ok(LimitedRead::Complete(bytes))
            };
        }
        bytes.extend_from_slice(&buffer[..bytes_read]);
    }

    let mut sentinel = [0_u8; 1];
    loop {
        return match reader.read(&mut sentinel) {
            Ok(0) if bytes.is_empty() => Ok(LimitedRead::Empty),
            Ok(0) => Ok(LimitedRead::Complete(bytes)),
            Ok(_) => Ok(LimitedRead::Oversized),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => Err(err),
        };
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteSshConfigPaths {
    pub(crate) user_config: Option<std::path::PathBuf>,
    pub(crate) system_config: Option<std::path::PathBuf>,
    pub(crate) multiplexing: bool,
}

pub(crate) const REMOTE_BRIDGE_IDLE_TIMEOUT_SUPPORTED: bool =
    cfg!(any(target_os = "linux", target_os = "macos"));

#[cfg(unix)]
mod remote_bridge;
#[cfg(all(test, unix))]
mod remote_bridge_tests;
#[cfg(unix)]
mod unix_common;
#[cfg(unix)]
pub(crate) mod unix_image_files;
#[cfg(unix)]
pub(crate) use unix_common::{
    begin_cli_output, end_cli_output, forward_remote_bridge_stdio, ignore_server_hangup,
    local_stream_peer_description, spawn_server_signal_monitor, RemoteBridgeWake,
};

mod client_state;
pub(crate) use client_state::{create_private_state_file, replace_file, sync_parent_directory};

#[cfg(not(unix))]
pub(crate) fn begin_cli_output() {}

#[cfg(not(unix))]
pub(crate) fn end_cli_output() {}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(windows))]
pub(crate) use process_cwd as pane_process_cwd;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use fallback::*;

/// What one liveness read proved about a process the pane identified as its
/// agent. Only `Gone` may end a held agent: a failed read proves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneProcessLiveness {
    /// Alive in the pane's terminal session, in this process group. A stopped
    /// or backgrounded job still counts.
    Alive(u32),
    /// Exited, a zombie, a later process reusing the pid, or a process that
    /// left the pane's terminal session and so can never return to its front.
    Gone,
    /// A read failed or returned a value that proves neither.
    Unknown,
}

/// Liveness of a held agent on a platform that reads its facts one at a time,
/// as Windows does. Each read answers `None` when it failed, and only a read
/// that succeeded may prove the agent gone.
#[cfg(any(windows, test))]
pub(crate) fn held_process_liveness(
    pid: u32,
    running: Option<bool>,
    start_token_matches: impl FnOnce() -> Option<bool>,
    in_pane: impl FnOnce() -> Option<bool>,
) -> PaneProcessLiveness {
    match running {
        Some(true) => {}
        Some(false) => return PaneProcessLiveness::Gone,
        None => return PaneProcessLiveness::Unknown,
    }
    match start_token_matches() {
        Some(true) => {}
        Some(false) => return PaneProcessLiveness::Gone,
        None => return PaneProcessLiveness::Unknown,
    }
    match in_pane() {
        Some(true) => PaneProcessLiveness::Alive(pid),
        Some(false) => PaneProcessLiveness::Gone,
        None => PaneProcessLiveness::Unknown,
    }
}

/// Whether a held agent still belongs to its pane on Windows: a descendant of
/// the pane shell, or an escaped Git Bash agent that carries the pane's
/// runtime marker. `listed` says the process table holds both the shell and
/// the agent: a held agent missing from it, or with its shell missing, means
/// the table was not read. Each read answers `None` when it failed, and so does
/// the result unless a read that succeeded already decides it.
#[cfg(any(windows, test))]
pub(crate) fn pane_membership(
    listed: bool,
    descends_from_shell: bool,
    shell_is_git_bash: impl FnOnce() -> Option<bool>,
    identifies_agent: impl FnOnce() -> Option<bool>,
    shell_marker: impl FnOnce() -> Option<Option<String>>,
    agent_marker: impl FnOnce() -> Option<Option<String>>,
) -> Option<bool> {
    if descends_from_shell {
        return Some(true);
    }
    if !listed {
        return None;
    }
    let shell_is_git_bash = shell_is_git_bash();
    if shell_is_git_bash == Some(false) {
        return Some(false);
    }
    let identifies_agent = identifies_agent();
    if identifies_agent == Some(false) {
        return Some(false);
    }
    shell_is_git_bash?;
    identifies_agent?;
    let Some(pane_marker) = shell_marker()?.filter(|marker| !marker.is_empty()) else {
        return Some(false);
    };
    Some(agent_marker()?.as_deref() == Some(pane_marker.as_str()))
}

/// The exit code Windows reports for a process that is still running.
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_STILL_ACTIVE: u32 = 259;

/// The error OpenProcess sets for a pid that names no process.
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_ERROR_INVALID_PARAMETER: i32 = 87;

/// The reads Windows makes on an opened held agent, each as the raw API
/// result: `None` when the call failed.
#[cfg(any(windows, test))]
pub(crate) trait HeldProcessReads {
    /// The code GetExitCodeProcess reported.
    fn exit_code(&self) -> Option<u32>;
    /// The creation time of the process.
    fn creation_time(&self) -> Option<u64>;
    /// Whether foreground selection could still pick the process for its pane.
    fn in_pane(&self) -> Option<bool>;
}

/// Liveness of a held agent on Windows from the result of opening it:
/// `Err` carries the OS error code OpenProcess set, if any. Only an open that
/// names no process, or a read that succeeded, may prove the agent gone.
#[cfg(any(windows, test))]
pub(crate) fn opened_held_process_liveness(
    pid: u32,
    start_token: u64,
    opened: Result<impl HeldProcessReads, Option<i32>>,
) -> PaneProcessLiveness {
    let reads = match opened {
        Ok(reads) => reads,
        Err(Some(WINDOWS_ERROR_INVALID_PARAMETER)) => return PaneProcessLiveness::Gone,
        Err(_) => return PaneProcessLiveness::Unknown,
    };
    held_process_liveness(
        pid,
        reads
            .exit_code()
            .map(|exit_code| exit_code == WINDOWS_STILL_ACTIVE),
        || {
            reads
                .creation_time()
                .map(|creation_time| creation_time == start_token)
        },
        || reads.in_pane(),
    )
}

/// Whether `path` names a regular file; `None` when its metadata could not be
/// read, which proves nothing. Only a read that finds no entry proves absence.
#[cfg(any(windows, test))]
pub(crate) fn path_is_file_read(path: &std::path::Path) -> Option<bool> {
    Some(path.is_file())
}

/// Whether `path` is the bash.exe of a Git for Windows install; `None` when a
/// file check it needed failed. `is_file` answers as `path_is_file_read`.
#[cfg(any(windows, test))]
pub(crate) fn git_bash_executable_path_read(
    path: &std::path::Path,
    is_file: impl Fn(&std::path::Path) -> Option<bool>,
) -> Option<bool> {
    use std::ffi::OsStr;

    let named = |path: &std::path::Path, name: &str| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|file_name| file_name.eq_ignore_ascii_case(name))
    };
    if !named(path, "bash.exe") || !path.is_absolute() {
        return Some(false);
    }
    if !is_file(path)? {
        return Some(false);
    }
    let Some(bin_dir) = path.parent().filter(|bin_dir| named(bin_dir, "bin")) else {
        return Some(false);
    };
    let Some(mut root) = bin_dir.parent() else {
        return Some(false);
    };
    if named(root, "usr") {
        let Some(parent) = root.parent() else {
            return Some(false);
        };
        root = parent;
    }

    let msys = is_file(&root.join("usr").join("bin").join("msys-2.0.dll"));
    let git = is_file(&root.join("cmd").join("git.exe"));
    if msys == Some(false) || git == Some(false) {
        return Some(false);
    }
    Some(msys? && git?)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn available_pane_shell_from_job(child_pid: u32, job: ForegroundJob) -> Option<String> {
    if job.process_group_id != child_pid
        || job.processes.iter().any(|process| process.pid != child_pid)
    {
        return None;
    }
    job.processes
        .into_iter()
        .find(|process| process.pid == child_pid)
        .map(|process| process.name)
        .filter(|name| is_pane_shell_process_name(name))
}

fn normalized_process_name(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn is_powershell_process_name(name: &str) -> bool {
    matches!(
        normalized_process_name(name).as_str(),
        "pwsh" | "powershell"
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn interactive_unix_shell_command(
    argv: &[String],
    shell_name: &str,
    quote_posix_arg: fn(&str) -> String,
) -> Option<String> {
    let quote = if is_powershell_process_name(shell_name) {
        quote_powershell_arg
    } else {
        quote_posix_arg
    };
    let mut parts = argv.iter();
    let mut command = quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&quote(part));
    }
    Some(command)
}

pub(crate) fn quote_powershell_arg(value: &str) -> String {
    if !value.is_empty()
        && !value.starts_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'+' | b'=')
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn quote_windows_command_line_arg(value: &str) -> String {
    if !value.is_empty()
        && !value
            .chars()
            .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\x0b' | '"'))
    {
        return value.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
        }
        backslashes = 0;
        quoted.push(ch);
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

pub(crate) fn is_pane_shell_process_name(name: &str) -> bool {
    let normalized = normalized_process_name(name);
    matches!(
        normalized.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "mksh"
            | "csh"
            | "tcsh"
            | "elvish"
            | "xonsh"
            | "nu"
            | "pwsh"
            | "powershell"
            | "cmd"
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_agent_hint(_pid: u32) -> Option<crate::detect::Agent> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn parse_agent_env_hint(environ: &[u8]) -> Option<crate::detect::Agent> {
    for record in environ.split(|&byte| byte == 0) {
        let Some(value) = record.strip_prefix(b"HERDR_AGENT=") else {
            continue;
        };
        return crate::detect::parse_agent_label(std::str::from_utf8(value).ok()?);
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug)]
pub(crate) struct InputSourceRestore;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn switch_to_ascii_input_source() -> Option<InputSourceRestore> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn pump_input_source_runloop() {}

/// Switches the host keyboard input source while prefix mode is active.
///
/// `App` drives this through a trait so the prefix-mode transitions can be
/// tested with a fake, without touching the real macOS APIs or leaking a
/// platform-specific restore type into `App`.
pub(crate) trait PrefixInputSource {
    /// Switch to an ASCII-capable input source for prefix commands. No-op if
    /// the current source is already ASCII-capable, the platform is
    /// unsupported, or the switch fails. Calling it again before `restore`
    /// keeps the source saved by the first call.
    fn switch_to_ascii(&mut self);

    /// Restore whatever `switch_to_ascii` saved. No-op if nothing was switched.
    fn restore(&mut self);
}

/// Production [`PrefixInputSource`] backed by the per-platform API.
#[derive(Default)]
pub(crate) struct RealPrefixInputSource {
    restore: Option<InputSourceRestore>,
}

impl PrefixInputSource for RealPrefixInputSource {
    fn switch_to_ascii(&mut self) {
        if self.restore.is_none() {
            // Drain pending input-source-change notifications so the read below is fresh (see
            // `pump_input_source_runloop`); a no-op on non-macOS.
            pump_input_source_runloop();
            self.restore = switch_to_ascii_input_source();
        }
    }

    fn restore(&mut self) {
        let _ = self.restore.take();
    }
}

#[cfg(all(test, any(unix, windows)))]
#[test]
fn child_exit_classification_only_checkpoints_interruptions() {
    for code in [0, 1, 130, 255, 0xC0000005] {
        let reason = classify_child_exit(&portable_pty::ExitStatus::with_exit_code(code));
        assert_eq!(reason, ChildExitReason::Exited, "exit code {code:#x}");
        assert!(!reason.requires_session_checkpoint());
    }
    #[cfg(windows)]
    let status = portable_pty::ExitStatus::with_exit_code(0xC000013A);
    #[cfg(not(windows))]
    let status = portable_pty::ExitStatus::with_signal("Terminated: 15");
    assert_eq!(classify_child_exit(&status), ChildExitReason::Interrupted);
    assert!(classify_child_exit(&status).requires_session_checkpoint());
    #[cfg(unix)]
    assert!(ChildExitReason::Handoff.requires_session_checkpoint());
    assert!(!ChildExitReason::WaitFailed.requires_session_checkpoint());
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn held_process_liveness_keeps_an_agent_whose_exit_code_query_failed() {
        assert_eq!(
            held_process_liveness(20, None, || Some(true), || Some(true)),
            PaneProcessLiveness::Unknown,
            "a failed exit-code query proves nothing"
        );
        assert_eq!(
            held_process_liveness(20, Some(false), || Some(true), || Some(true)),
            PaneProcessLiveness::Gone,
            "an observed exit code other than still active"
        );
        assert_eq!(
            held_process_liveness(20, Some(true), || Some(true), || Some(true)),
            PaneProcessLiveness::Alive(20)
        );
        assert_eq!(
            held_process_liveness(20, Some(true), || Some(false), || Some(true)),
            PaneProcessLiveness::Gone,
            "a later process reusing the pid"
        );
        assert_eq!(
            held_process_liveness(20, Some(true), || None, || Some(true)),
            PaneProcessLiveness::Unknown,
            "a failed creation time read"
        );
    }

    #[test]
    fn held_process_liveness_keeps_an_agent_whose_membership_read_failed() {
        let marker = |value: &str| Some(Some(value.to_string()));
        let membership = |listed, git_bash, agent, shell_marker, agent_marker| {
            pane_membership(
                listed,
                false,
                || git_bash,
                || agent,
                || shell_marker,
                || agent_marker,
            )
        };

        assert_eq!(
            held_process_liveness(20, Some(true), || Some(true), || None),
            PaneProcessLiveness::Unknown,
            "a failed membership read proves nothing"
        );
        assert_eq!(
            held_process_liveness(20, Some(true), || Some(true), || Some(false)),
            PaneProcessLiveness::Gone,
            "a read that shows the agent outside the pane"
        );

        let pane = marker("pane-a");
        assert_eq!(
            membership(true, Some(true), Some(true), pane.clone(), None),
            None,
            "the agent's runtime marker could not be read"
        );
        assert_eq!(
            membership(true, Some(true), Some(true), None, pane.clone()),
            None,
            "the shell's runtime marker could not be read"
        );
        assert_eq!(
            membership(false, Some(true), Some(true), pane.clone(), pane.clone()),
            None,
            "a failed process snapshot lists neither process"
        );
        assert_eq!(
            membership(true, None, Some(true), pane.clone(), pane.clone()),
            None,
            "the shell executable could not be read"
        );
        assert_eq!(
            membership(true, Some(true), None, pane.clone(), pane.clone()),
            None,
            "the agent command line could not be read"
        );

        assert_eq!(
            membership(true, Some(true), Some(true), pane.clone(), pane.clone()),
            Some(true),
            "an escaped agent with the pane marker"
        );
        assert_eq!(
            membership(true, Some(true), Some(true), pane.clone(), marker("pane-b")),
            Some(false),
            "a marker from another pane"
        );
        assert_eq!(
            membership(true, Some(true), Some(true), pane.clone(), Some(None)),
            Some(false),
            "an agent without a marker"
        );
        assert_eq!(
            membership(true, Some(true), Some(true), Some(None), pane.clone()),
            Some(false),
            "a shell without a marker"
        );
        assert_eq!(
            membership(true, Some(false), None, None, None),
            Some(false),
            "escape only applies to Git Bash"
        );
        assert_eq!(
            membership(true, None, Some(false), None, None),
            Some(false),
            "an escaped process that is not an agent"
        );
        assert_eq!(
            pane_membership(false, true, || None, || None, || None, || None),
            Some(true),
            "a descendant of the pane shell"
        );
    }

    struct StubHeldProcess {
        exit_code: Option<u32>,
        creation_time: Option<u64>,
        in_pane: Option<bool>,
    }

    impl HeldProcessReads for StubHeldProcess {
        fn exit_code(&self) -> Option<u32> {
            self.exit_code
        }

        fn creation_time(&self) -> Option<u64> {
            self.creation_time
        }

        fn in_pane(&self) -> Option<bool> {
            self.in_pane
        }
    }

    #[test]
    fn opened_held_process_liveness_keeps_an_agent_whose_api_reads_failed() {
        let liveness = |exit_code, creation_time, in_pane| {
            opened_held_process_liveness(
                20,
                7,
                Ok::<_, Option<i32>>(StubHeldProcess {
                    exit_code,
                    creation_time,
                    in_pane,
                }),
            )
        };
        let active = Some(WINDOWS_STILL_ACTIVE);

        assert_eq!(
            liveness(None, Some(7), Some(true)),
            PaneProcessLiveness::Unknown,
            "GetExitCodeProcess failed"
        );
        assert_eq!(
            liveness(Some(0), Some(7), Some(true)),
            PaneProcessLiveness::Gone,
            "an exit code other than still active"
        );
        assert_eq!(
            liveness(active, Some(7), Some(true)),
            PaneProcessLiveness::Alive(20)
        );
        assert_eq!(
            liveness(active, None, Some(true)),
            PaneProcessLiveness::Unknown,
            "the creation time query failed"
        );
        assert_eq!(
            liveness(active, Some(8), Some(true)),
            PaneProcessLiveness::Gone,
            "a later process reusing the pid"
        );
        assert_eq!(
            liveness(active, Some(7), None),
            PaneProcessLiveness::Unknown,
            "the membership read failed"
        );
        assert_eq!(
            liveness(active, Some(7), Some(false)),
            PaneProcessLiveness::Gone,
            "the agent left the pane"
        );

        let not_opened =
            |error| opened_held_process_liveness(20, 7, Err::<StubHeldProcess, _>(error));
        assert_eq!(
            not_opened(Some(WINDOWS_ERROR_INVALID_PARAMETER)),
            PaneProcessLiveness::Gone,
            "OpenProcess names no process"
        );
        assert_eq!(
            not_opened(Some(5)),
            PaneProcessLiveness::Unknown,
            "OpenProcess was denied access"
        );
        assert_eq!(
            not_opened(None),
            PaneProcessLiveness::Unknown,
            "OpenProcess failed without an error code"
        );
    }

    #[test]
    fn git_bash_executable_path_read_keeps_a_failed_file_check_unknown() {
        use std::path::Path;

        let bash = Path::new("/git/usr/bin/bash.exe");
        let check = |failed: &'static str, missing: &'static str| {
            git_bash_executable_path_read(bash, move |path: &Path| {
                let path = path.to_string_lossy();
                if path.ends_with(failed) {
                    None
                } else {
                    Some(!path.ends_with(missing))
                }
            })
        };

        assert_eq!(check("-", "-"), Some(true), "a Git for Windows bash");
        assert_eq!(check("git.exe", "-"), None, "cmd/git.exe could not be read");
        assert_eq!(
            check("msys-2.0.dll", "-"),
            None,
            "msys-2.0.dll could not be read"
        );
        assert_eq!(check("bash.exe", "-"), None, "bash.exe could not be read");
        assert_eq!(
            check("git.exe", "msys-2.0.dll"),
            Some(false),
            "a read that found msys-2.0.dll missing decides"
        );
        assert_eq!(check("-", "git.exe"), Some(false), "no cmd/git.exe");
        assert_eq!(
            git_bash_executable_path_read(Path::new("/git/usr/bin/zsh.exe"), |_| None),
            Some(false),
            "not bash.exe"
        );
        assert_eq!(
            git_bash_executable_path_read(Path::new("/git/usr/lib/bash.exe"), |_| Some(true)),
            Some(false),
            "bash.exe outside a bin directory"
        );
    }

    #[test]
    fn path_is_file_read_tells_a_failed_metadata_read_from_a_missing_file() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "herdr-path-is-file-read-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let locked = root.join("cmd");
        std::fs::create_dir_all(&locked).expect("create dir");
        let file = root.join("file");
        std::fs::write(&file, b"").expect("write file");
        std::fs::write(locked.join("git.exe"), b"").expect("write locked file");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock dir");
        // A process that can still read the locked directory, such as root,
        // cannot observe the failure this test needs.
        let denied = std::fs::metadata(locked.join("git.exe")).is_err();

        let answers = (
            path_is_file_read(&file),
            path_is_file_read(&root.join("missing")),
            path_is_file_read(&file.join("below-a-file")),
            path_is_file_read(&root),
            path_is_file_read(&locked.join("git.exe")),
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock dir");
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(answers.0, Some(true), "a regular file");
        assert_eq!(answers.1, Some(false), "no such file");
        assert_eq!(answers.2, Some(false), "a parent that is a file");
        assert_eq!(answers.3, Some(false), "a directory");
        if denied {
            assert_eq!(answers.4, None, "access denied proves nothing");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pane_process_liveness_rejects_processes_outside_the_pane_session() {
        use std::os::unix::process::CommandExt;

        let mut detached = std::process::Command::new("sleep");
        detached.arg("30");
        // SAFETY: setsid is async-signal-safe and touches only the child.
        unsafe {
            detached.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut detached = detached.spawn().expect("spawn detached");
        let token = process_start_token(detached.id()).expect("start token");
        let mut gone = std::process::Command::new("true").spawn().expect("spawn");
        let gone_pid = gone.id();
        gone.wait().expect("reap");

        assert_eq!(
            pane_process_liveness(std::process::id(), detached.id(), token),
            PaneProcessLiveness::Gone,
            "a live process in another terminal session"
        );
        assert_eq!(
            pane_process_liveness(gone_pid, detached.id(), token),
            PaneProcessLiveness::Unknown,
            "a pane shell that is gone proves nothing about the agent"
        );
        let _ = detached.kill();
        let _ = detached.wait();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn pane_process_liveness_follows_the_agent_process_not_its_job() {
        use std::os::unix::process::CommandExt;

        let shell_pid = std::process::id();
        let mut wrapper = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn wrapper");
        let job = wrapper.id();
        let mut agent = std::process::Command::new("sleep")
            .arg("30")
            .process_group(job as i32)
            .spawn()
            .expect("spawn agent");
        let agent_pid = agent.id();
        let token = process_start_token(agent_pid).expect("agent start token");
        let wrapper_token = process_start_token(job).expect("wrapper start token");

        assert_eq!(
            pane_process_liveness(shell_pid, agent_pid, token),
            PaneProcessLiveness::Alive(job)
        );
        assert_eq!(
            pane_process_liveness(shell_pid, agent_pid, token + 1),
            PaneProcessLiveness::Gone,
            "a reused pid has a different start token"
        );
        unsafe {
            libc::kill(agent_pid as libc::pid_t, libc::SIGSTOP);
        }
        assert_eq!(
            pane_process_liveness(shell_pid, agent_pid, token),
            PaneProcessLiveness::Alive(job)
        );

        unsafe {
            libc::kill(agent_pid as libc::pid_t, libc::SIGKILL);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pane_process_liveness(shell_pid, agent_pid, token) != PaneProcessLiveness::Gone
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            pane_process_liveness(shell_pid, agent_pid, token),
            PaneProcessLiveness::Gone,
            "an unreaped agent must not count as alive while its wrapper lives"
        );
        assert_eq!(
            pane_process_liveness(shell_pid, job, wrapper_token),
            PaneProcessLiveness::Alive(job)
        );
        agent.wait().expect("reap agent");
        assert_eq!(
            pane_process_liveness(shell_pid, agent_pid, token),
            PaneProcessLiveness::Gone
        );
        let _ = wrapper.kill();
        let _ = wrapper.wait();
    }

    #[test]
    fn terminal_resize_signal_is_recorded_once_per_delivery() {
        watch_terminal_resize_signal();
        assert!(!take_terminal_resize_signal());

        unsafe {
            libc::raise(libc::SIGWINCH);
        }

        assert!(take_terminal_resize_signal());
        assert!(!take_terminal_resize_signal());
    }

    #[test]
    fn pane_shell_process_names_reject_exec_replacement_programs() {
        for shell in ["bash", "-zsh", "/bin/fish", "pwsh", "powershell.exe"] {
            assert!(is_pane_shell_process_name(shell), "{shell}");
        }
        for program in ["vim", "nvim", "cargo", "test-runner", "opencode"] {
            assert!(!is_pane_shell_process_name(program), "{program}");
        }
    }

    #[test]
    fn detached_custom_command_preserves_unix_login_shell_flag() {
        let cmd = detached_custom_command_process("echo hello");
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("/bin/sh"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("-lc"),
                std::ffi::OsStr::new("echo hello")
            ]
        );
    }

    #[test]
    fn pane_custom_command_builder_preserves_unix_shell_flag() {
        let expected: Vec<std::ffi::OsString> =
            vec!["/bin/sh".into(), "-c".into(), "echo hello".into()];
        assert_eq!(
            pane_custom_command_pty_builder("echo hello").get_argv(),
            &expected
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_accepts_known_agents() {
        assert_eq!(
            parse_agent_env_hint(b"PATH=/bin\0HERDR_AGENT=claude\0TERM=xterm\0"),
            Some(crate::detect::Agent::Claude)
        );
        assert_eq!(
            parse_agent_env_hint(b"HERDR_AGENT=codex"),
            Some(crate::detect::Agent::Codex)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_ignores_missing_or_unknown_agents() {
        assert_eq!(parse_agent_env_hint(b"PATH=/bin\0TERM=xterm\0"), None);
        assert_eq!(parse_agent_env_hint(b"HERDR_AGENT=not-an-agent\0"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn interactive_shell_command_quotes_for_posix_and_powershell() {
        let argv = vec![
            "pi".into(),
            String::new(),
            "two words".into(),
            "a'b".into(),
            "$HOME".into(),
            "semi;colon".into(),
            "@options".into(),
        ];
        assert_eq!(
            interactive_shell_command(&argv, "bash").as_deref(),
            Some("pi '' 'two words' 'a'\\''b' '$HOME' 'semi;colon' @options")
        );
        assert_eq!(
            interactive_shell_command(&argv, "pwsh").as_deref(),
            Some("pi '' 'two words' 'a''b' '$HOME' 'semi;colon' '@options'")
        );
    }

    #[test]
    fn read_limited_reader_returns_complete_data_under_limit() {
        let input = std::io::Cursor::new(b"image".to_vec());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_returns_empty_for_empty_input() {
        let input = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Empty
        );
    }

    #[test]
    fn read_limited_reader_accepts_data_exactly_at_limit() {
        let input = std::io::Cursor::new(b"four".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Complete(b"four".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_rejects_data_over_limit() {
        let input = std::io::Cursor::new(b"oversized".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Oversized
        );
    }

    #[test]
    fn read_limited_reader_retries_interrupted_reads() {
        struct InterruptedOnce {
            interrupted: bool,
            inner: std::io::Cursor<Vec<u8>>,
        }

        impl std::io::Read for InterruptedOnce {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.inner.read(buffer)
            }
        }

        let input = InterruptedOnce {
            interrupted: false,
            inner: std::io::Cursor::new(b"image".to_vec()),
        };
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }
}

#[cfg(not(unix))]
pub(crate) fn shared_ssh_control_path(
    _namespace: &std::path::Path,
    _target: &str,
) -> std::io::Result<std::path::PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "interactive SSH recovery requires Unix OpenSSH multiplexing",
    ))
}
