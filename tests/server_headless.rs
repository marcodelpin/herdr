//! Integration tests for headless server mode.

#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, client_handshake, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid, CURRENT_PROTOCOL,
};

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-server-test-{}-{nanos}",
        std::process::id()
    ))
}

struct SpawnedHerdr {
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl SpawnedHerdr {
    fn close_master(&mut self) {
        drop(self._master.take());
    }
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        support::stop_spawned_herdr(&mut *self.child);
        self.close_master();

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }

            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

fn cleanup_spawned_herdr(spawned: SpawnedHerdr, base: PathBuf) {
    drop(spawned);
    cleanup_test_base(&base);
}

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not accept connections at {}", path.display());
}

fn spawn_server(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
    client_socket_path: &Path,
) -> SpawnedHerdr {
    spawn_server_with_env(
        config_home,
        runtime_dir,
        api_socket_path,
        client_socket_path,
        &[],
    )
}

fn spawn_server_with_env(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket_path: &Path,
    _client_socket_path: &Path,
    extra_env: &[(&str, &str)],
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
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
    cmd.env("HERDR_SOCKET_PATH", api_socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: Some(pair.master),
        child,
    }
}

fn ping_socket(socket_path: &Path) -> String {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");

    let request = r#"{"id":"1","method":"ping","params":{}}"#;
    writeln!(stream, "{}", request).unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    response.trim().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn server_creates_both_sockets() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);

    // Wait for both sockets to appear.
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Verify the client socket is a socket file.
    let metadata = fs::metadata(&client_socket).unwrap();
    let file_type = metadata.file_type();
    assert!(
        file_type.is_socket(),
        "client socket should be a socket file"
    );

    // Verify the API socket works.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "ping should return pong: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_starts_without_terminal() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);

    // Wait for the API socket to appear — proves the server started.
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // The server process should be running.
    if let Some(pid) = spawned.child.process_id() {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(result, 0, "server process should be running");
    }

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_api_responds_to_ping() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // Ping the API socket.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should respond to ping: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_removes_client_socket_on_exit() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Stop the server.
    support::stop_spawned_herdr(&mut *spawned.child);
    spawned.close_master();
    let _ = spawned.child.wait();

    // Give it a moment to clean up.
    thread::sleep(Duration::from_millis(300));

    // The client socket should be removed (best-effort by Drop).
    // If it still exists, it should be stale (not connectable).
    if client_socket.exists() {
        assert!(
            UnixStream::connect(&client_socket).is_err(),
            "stale client socket should not accept connections"
        );
    }

    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn server_cleans_up_stale_client_socket() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    // Create a stale client socket file (simulating a crashed server).
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    {
        let _listener = std::os::unix::net::UnixListener::bind(&client_socket).unwrap();
        // Drop the listener so the socket becomes stale.
    }

    // Now start the server — it should clean up the stale socket.
    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // The API should work.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should respond to ping: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_persists_after_client_disconnect() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Connect to the client socket and then immediately disconnect.
    {
        let _stream = UnixStream::connect(&client_socket).expect("should connect to client socket");
        // Immediately drop the connection.
    }

    // Give the server a moment to process the disconnect.
    thread::sleep(Duration::from_millis(200));

    // The server should still be running and the API should still respond.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "API should still respond after client disconnect: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn duplicate_server_start_fails_gracefully() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    // Start the first server.
    let spawned1 = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // Try to start a second server — it should fail.
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
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", &api_socket);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");

    let mut child2 = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child2.process_id());
    drop(pair.slave);

    // Wait for the second server to exit.
    let exit_status = child2.wait().unwrap();
    unregister_spawned_herdr_pid(child2.process_id());

    // The second server should exit with a non-zero code.
    assert!(!exit_status.success(), "duplicate server start should fail");

    cleanup_spawned_herdr(spawned1, base);
}

#[test]
fn client_handshake_succeeds() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Connect to the client socket and perform a handshake.
    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");

    // Send Hello with the current protocol version, 80 cols, 24 rows.
    let (version, error) =
        client_handshake(&mut stream, CURRENT_PROTOCOL, 80, 24).expect("handshake should succeed");

    assert_eq!(
        version, CURRENT_PROTOCOL,
        "server should report current protocol version"
    );
    assert!(
        error.is_none(),
        "handshake should not have an error: {:?}",
        error
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn client_handshake_rejects_incompatible_version() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Connect to the client socket and send Hello with version 0 (pre-persistence).
    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");

    let (version, error) = client_handshake(&mut stream, 0, 80, 24)
        .expect("should read Welcome response even on rejection");

    assert_eq!(
        version, CURRENT_PROTOCOL,
        "server should report its current protocol version"
    );
    assert!(
        error.is_some(),
        "version 0 should be rejected with an error"
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn client_handshake_clamps_small_terminal_size() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Send Hello with 0x0 terminal size — should be clamped.
    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");

    let (version, error) = client_handshake(&mut stream, CURRENT_PROTOCOL, 0, 0)
        .expect("handshake with 0x0 should succeed (server clamps)");

    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(
        error.is_none(),
        "0x0 size should be accepted (clamped): {:?}",
        error
    );

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn no_hello_client_closed_within_five_seconds() {
    // Client connection that sends no Hello is closed within 5 seconds.
    // The server sets a handshake timeout of 4 seconds to guarantee the connection
    // is closed within the 5-second deadline even with OS overhead.
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_file(&client_socket, Duration::from_secs(10));

    // Connect but don't send Hello — just a raw connection.
    let mut stream = UnixStream::connect(&client_socket).expect("should connect to client socket");

    // Set a read timeout longer than the handshake timeout so we can detect
    // when the server closes the connection.
    stream
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();

    let start = Instant::now();

    // Try to read from the stream. The server should close the connection
    // within 5 seconds, causing our read to return with an error (EOF or
    // connection reset).
    let mut buf = [0u8; 1024];
    let result = stream.read(&mut buf);
    let elapsed = start.elapsed();

    // The read should fail (connection closed by server).
    assert!(
        result.is_err() || result.unwrap() == 0,
        "server should close the connection when no Hello is sent"
    );

    // The connection should be closed within 5 seconds.
    assert!(
        elapsed < Duration::from_secs(5),
        "connection should be closed within 5 seconds, took {:?}",
        elapsed
    );

    // Verify the server is still healthy — a proper client can still connect.
    let mut good_stream =
        UnixStream::connect(&client_socket).expect("should connect after no-hello client");
    let (version, error) = client_handshake(&mut good_stream, CURRENT_PROTOCOL, 80, 24)
        .expect("proper handshake should still work after no-hello client");
    assert_eq!(version, CURRENT_PROTOCOL);
    assert!(error.is_none());

    // API should still work.
    let response = ping_socket(&api_socket);
    assert!(
        response.contains("pong"),
        "server should still respond to ping: {response}"
    );

    cleanup_spawned_herdr(spawned, base);
}

fn server_log_path(config_home: &Path) -> PathBuf {
    let app_dir_name = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    config_home.join(app_dir_name).join("herdr-server.log")
}

fn wait_for_exit(child: &mut Box<dyn Child + Send + Sync>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn wait_for_log_line(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let content = fs::read_to_string(path).unwrap_or_default();
        if let Some(line) = content.lines().find(|line| line.contains(needle)) {
            return line.to_string();
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "log {} did not contain {needle:?}:\n{}",
        path.display(),
        fs::read_to_string(path).unwrap_or_default()
    );
}

#[test]
fn server_survives_hangup_and_logs_why_it_stops() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    let pid = spawned.child.process_id().expect("server pid") as libc::pid_t;

    assert_eq!(unsafe { libc::kill(pid, libc::SIGHUP) }, 0);
    // Closing the terminal hangs up the server's whole session too.
    spawned.close_master();
    assert!(
        !wait_for_exit(&mut spawned.child, Duration::from_millis(500)),
        "server must not stop on SIGHUP"
    );
    assert!(ping_socket(&api_socket).contains("pong"));

    // The API socket is bound before the event loop installs the SIGTERM
    // handler. On a loaded host that gap exceeds the 500 ms above, and a
    // SIGTERM landing inside it ends the process with no shutdown line
    // (herdr-9ds0). Wait for the startup line the loop logs first.
    wait_for_log_line(
        &server_log_path(&config_home),
        "app.startup",
        Duration::from_secs(30),
    );
    // Startup line and handler install are adjacent statements.
    thread::sleep(Duration::from_millis(200));

    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert!(
        wait_for_exit(&mut spawned.child, Duration::from_secs(10)),
        "server must stop on SIGTERM"
    );
    let line = wait_for_log_line(
        &server_log_path(&config_home),
        "server shutdown initiated",
        Duration::from_secs(5),
    );
    assert!(line.contains("reason=SIGTERM"), "{line}");

    cleanup_spawned_herdr(spawned, base);
}

#[test]
fn server_stop_request_logs_its_caller() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    let mut stream = UnixStream::connect(&api_socket).unwrap();
    writeln!(
        stream,
        r#"{{"id":"stop","method":"server.stop","params":{{}}}}"#
    )
    .unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    assert!(response.contains("\"ok\""), "{response}");

    assert!(wait_for_exit(&mut spawned.child, Duration::from_secs(10)));
    let line = wait_for_log_line(
        &server_log_path(&config_home),
        "server shutdown initiated",
        Duration::from_secs(5),
    );
    let expected = if cfg!(any(target_os = "linux", target_os = "macos")) {
        format!("server.stop request from pid {}", std::process::id())
    } else {
        "server.stop request".to_string()
    };
    assert!(line.contains(&expected), "{line}");

    cleanup_spawned_herdr(spawned, base);
}

/// A connection the server accepted, or that still sits in its listen
/// backlog, when SIGTERM arrives was made to a live server. It gets an
/// explicit error line before the process exits, never a close with no bytes.
/// The clients withhold the end of their request, so every one of them is at
/// that boundary when the signal lands; no timing puts them there.
#[test]
fn sigterm_answers_connections_that_have_not_finished_their_request() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    let pid = spawned.child.process_id().expect("server pid") as libc::pid_t;
    // Waits for the event loop, so the signal finds these clients mid-request
    // on a running server; the startup window has its own test (herdr-ir4j).
    wait_for_log_line(
        &server_log_path(&config_home),
        "app.startup",
        Duration::from_secs(30),
    );
    thread::sleep(Duration::from_millis(200));

    let mut clients = Vec::new();
    for index in 0..4 {
        let mut client = UnixStream::connect(&api_socket).expect("connect to a live server");
        client
            .write_all(format!(r#"{{"id":"partial-{index}","method":"pi"#).as_bytes())
            .unwrap();
        clients.push(("partial request", client));
    }
    for _ in 0..4 {
        let client = UnixStream::connect(&api_socket).expect("connect to a live server");
        clients.push(("nothing sent", client));
    }

    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);

    for (kind, client) in clients {
        client
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut line = String::new();
        let read = BufReader::new(client).read_line(&mut line);
        assert!(
            matches!(read, Ok(bytes) if bytes > 0) && line.ends_with('\n'),
            "connection with {kind} got no reply at shutdown: {read:?} {line:?}"
        );
        assert!(
            line.contains(r#""code":"server_unavailable""#)
                && line.contains("server is shutting down"),
            "connection with {kind}: {line}"
        );
    }

    assert!(
        wait_for_exit(&mut spawned.child, Duration::from_secs(15)),
        "server must stop on SIGTERM"
    );
    assert!(!api_socket.exists(), "api socket must be removed");

    cleanup_spawned_herdr(spawned, base);
}

// ---------------------------------------------------------------------------
// A stop signal inside the startup window (herdr-ir4j)
//
// The API socket accepts connections before the event loop runs. A SIGTERM
// in that window must take the shutdown drain whatever startup is doing, and
// within the drain's own bound counted from the signal. Each test below holds
// startup in one blocking step behind an acknowledged barrier (the server
// proves it is there), proves the accepted requests are still unanswered,
// signals, and only then lets startup go on, if at all.
// ---------------------------------------------------------------------------

/// The API shutdown drain's bound (API_SHUTDOWN_DRAIN_TIMEOUT, 7 s) plus 1 s
/// for scheduling: every accepted connection is answered by then, counted
/// from the signal.
const STARTUP_STOP_REPLY_BOUND: Duration = Duration::from_secs(8);
/// A server whose startup never finishes exits once the drain bound passed;
/// 3 s on top for scheduling and logging.
const STARTUP_STOP_EXIT_BOUND: Duration = Duration::from_secs(10);

/// One connection accepted inside the startup window.
struct WindowClient {
    kind: &'static str,
    /// The id the reply must carry; None for a request the server never read
    /// whole, which is answered without one.
    id: Option<String>,
    stream: UnixStream,
}

/// Complete requests (handed to the app, which does not run yet), a request
/// without its newline, and a connection that sent nothing.
fn connect_startup_window_clients(api_socket: &Path) -> Vec<WindowClient> {
    let mut clients = Vec::new();
    for index in 0..4 {
        let id = format!("startup-{index}");
        let mut stream = UnixStream::connect(api_socket).expect("connect to a live server");
        stream
            .write_all(
                format!("{{\"id\":\"{id}\",\"method\":\"workspace.list\",\"params\":{{}}}}\n")
                    .as_bytes(),
            )
            .unwrap();
        clients.push(WindowClient {
            kind: "dispatched request",
            id: Some(id),
            stream,
        });
    }
    for index in 0..2 {
        let mut stream = UnixStream::connect(api_socket).expect("connect to a live server");
        stream
            .write_all(format!(r#"{{"id":"partial-{index}","method":"workspace.li"#).as_bytes())
            .unwrap();
        clients.push(WindowClient {
            kind: "request without its newline",
            id: None,
            stream,
        });
    }
    for _ in 0..2 {
        let stream = UnixStream::connect(api_socket).expect("connect to a live server");
        clients.push(WindowClient {
            kind: "nothing sent",
            id: None,
            stream,
        });
    }
    clients
}

/// Proves every client is still waiting (no byte, no EOF), then sends SIGTERM
/// and returns when it was sent.
fn sigterm_inside_startup_window(pid: libc::pid_t, clients: &[WindowClient]) -> Instant {
    // Long enough for a running event loop to answer the dispatched requests.
    thread::sleep(Duration::from_millis(300));
    for client in clients {
        client.stream.set_nonblocking(true).unwrap();
        let mut byte = [0u8; 1];
        let read = (&client.stream).read(&mut byte);
        assert!(
            matches!(&read, Err(err) if err.kind() == std::io::ErrorKind::WouldBlock),
            "{} must still be unanswered when the signal is sent: {read:?}",
            client.kind
        );
        client.stream.set_nonblocking(false).unwrap();
    }
    let signalled = Instant::now();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    signalled
}

/// Every client reads one whole line, a reply or `server_unavailable`, before
/// the drain bound counted from the signal; never EOF, never a reset.
fn assert_answered_after_startup_sigterm(clients: Vec<WindowClient>, signalled: Instant) {
    for client in clients {
        let remaining = (signalled + STARTUP_STOP_REPLY_BOUND)
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        client.stream.set_read_timeout(Some(remaining)).unwrap();
        let mut line = String::new();
        let read = BufReader::new(&client.stream).read_line(&mut line);
        let elapsed = signalled.elapsed();
        assert!(
            matches!(read, Ok(bytes) if bytes > 0) && line.ends_with('\n'),
            "{} accepted during startup got no reply {elapsed:?} after SIGTERM: {read:?} {line:?}",
            client.kind
        );
        assert!(
            elapsed <= STARTUP_STOP_REPLY_BOUND,
            "{} answered {elapsed:?} after SIGTERM, past the drain bound",
            client.kind
        );
        match &client.id {
            Some(id) => assert!(
                line.contains(&format!("\"id\":\"{id}\""))
                    && (line.contains("\"result\"")
                        || line.contains(r#""code":"server_unavailable""#)),
                "{}: {line}",
                client.kind
            ),
            None => assert!(
                line.contains(r#""code":"server_unavailable""#),
                "{}: {line}",
                client.kind
            ),
        }
    }
}

/// The replies came from the stop, not from a startup that finished first.
fn assert_startup_not_finished(log: &Path, started_line: &str) {
    let content = fs::read_to_string(log).unwrap_or_default();
    assert!(
        !content.contains(started_line),
        "startup finished before the replies, so the window was not exercised:\n{content}"
    );
}

fn wait_for_exit_status(
    child: &mut Box<dyn Child + Send + Sync>,
    timeout: Duration,
) -> Option<portable_pty::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().ok().flatten() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

/// Startup held by the test hook right after the API bind. Once every
/// connection is answered the hold is released: startup reaches the event
/// loop, which takes the stop over and ends the process the ordinary way.
#[test]
fn sigterm_while_startup_is_held_after_the_api_bind_answers_every_accepted_connection() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let release = base.join("release-startup-hold");
    let log = server_log_path(&config_home);

    let mut spawned = spawn_server_with_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &client_socket,
        &[
            ("HERDR_TEST_HOLD_AFTER_API_BIND_MS", "30000"),
            (
                "HERDR_TEST_HOLD_AFTER_API_BIND_RELEASE",
                release.to_str().unwrap(),
            ),
        ],
    );
    wait_for_log_line(
        &log,
        "startup held after the api bind for a test",
        Duration::from_secs(30),
    );
    let pid = spawned.child.process_id().expect("server pid") as libc::pid_t;

    let clients = connect_startup_window_clients(&api_socket);
    let signalled = sigterm_inside_startup_window(pid, &clients);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_answered_after_startup_sigterm(clients, signalled);
        assert_startup_not_finished(&log, "herdr server started");
    }));
    fs::write(&release, b"").unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }

    let status = wait_for_exit_status(&mut spawned.child, STARTUP_STOP_EXIT_BOUND)
        .expect("server must stop on a SIGTERM received during startup");
    assert!(status.success(), "exit status {status:?}");
    assert!(!api_socket.exists(), "api socket must be removed");
    let line = wait_for_log_line(&log, "server shutdown initiated", Duration::from_secs(5));
    assert!(line.contains("reason=SIGTERM"), "{line}");

    cleanup_spawned_herdr(spawned, base);
}

/// Startup blocked in App::try_new on the plugin registry lock, which another
/// process holds and never releases while the test runs. Startup does not
/// finish, so the stop must end the process without it.
#[test]
fn sigterm_while_startup_waits_for_the_plugin_registry_lock_answers_every_accepted_connection() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let log = server_log_path(&config_home);

    let app_dir = log.parent().unwrap().to_path_buf();
    fs::create_dir_all(&app_dir).unwrap();
    let registry_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(app_dir.join(".plugins.lock"))
        .unwrap();
    registry_lock.lock().unwrap();

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, &client_socket);
    wait_for_log_line(
        &log,
        "waiting for the plugin registry lock held by another process",
        Duration::from_secs(30),
    );
    let pid = spawned.child.process_id().expect("server pid") as libc::pid_t;

    let clients = connect_startup_window_clients(&api_socket);
    let signalled = sigterm_inside_startup_window(pid, &clients);
    assert_answered_after_startup_sigterm(clients, signalled);
    assert_startup_not_finished(&log, "herdr server started");

    let status = wait_for_exit_status(
        &mut spawned.child,
        STARTUP_STOP_EXIT_BOUND.saturating_sub(signalled.elapsed()),
    )
    .expect("server must exit even though its startup never finishes");
    assert!(
        signalled.elapsed() <= STARTUP_STOP_EXIT_BOUND,
        "exit {:?} after SIGTERM",
        signalled.elapsed()
    );
    assert!(status.success(), "exit status {status:?}");
    assert!(!api_socket.exists(), "api socket must be removed");

    drop(registry_lock);
    cleanup_spawned_herdr(spawned, base);
}

/// A handoff import server, killed with SIGKILL if a test leaves it running.
struct ImportServer {
    child: std::process::Child,
}

impl Drop for ImportServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        unregister_spawned_herdr_pid(Some(self.child.id()));
    }
}

fn read_handoff_line(stream: &mut UnixStream) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            Err(err) => panic!("handoff stream read failed: {err}"),
        }
    }
    String::from_utf8(line).unwrap()
}

/// A handoff import server that reported ready and waits for the source to
/// commit. Its API socket accepts connections and its event loop is not
/// running. The test is the handoff source and commits only once every
/// connection was answered; the import then reaches the event loop, which
/// ends the process the ordinary way.
#[test]
fn sigterm_while_a_handoff_import_waits_for_commitment_answers_every_accepted_connection() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let handoff_socket = base.join("handoff.sock");
    let log = server_log_path(&config_home);
    // Any value works: the import only echoes it back to its source.
    let handoff_nonce = format!("ir4j-{}", std::process::id());

    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join("herdr/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let listener = std::os::unix::net::UnixListener::bind(&handoff_socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_herdr"))
        .arg("server")
        .arg("--handoff-import")
        .arg(&handoff_socket)
        .arg(&handoff_nonce)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("HERDR_SOCKET_PATH", &api_socket)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_STARTUP_CWD")
        .env_remove("HERDR_SESSION")
        .env_remove("HERDR_ENV")
        .env("SHELL", "/bin/sh")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    register_spawned_herdr_pid(Some(child.id()));
    let mut import = ImportServer { child };

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut source = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "handoff import never connected");
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("handoff accept failed: {err}"),
        }
    };
    source.set_nonblocking(false).unwrap();
    source
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    assert_eq!(read_handoff_line(&mut source), handoff_nonce);
    let manifest = r#"{"version":1,"source_version":"ir4j-test","source_protocol":0,"expected_version":null,"expected_protocol":null,"snapshot":{"version":0,"workspaces":[],"active":null,"selected":0},"panes":[]}"#;
    source
        .write_all(format!("{manifest}\n").as_bytes())
        .unwrap();
    assert_eq!(read_handoff_line(&mut source), "validated");
    // No pane, so no file descriptor to pass.
    assert_eq!(read_handoff_line(&mut source), "restored");
    // The barrier: the import bound its API socket and now waits for commit.
    assert_eq!(read_handoff_line(&mut source), "ready");

    let pid = import.child.id() as libc::pid_t;
    let clients = connect_startup_window_clients(&api_socket);
    let signalled = sigterm_inside_startup_window(pid, &clients);
    assert_answered_after_startup_sigterm(clients, signalled);
    assert_startup_not_finished(&log, "handoff import server started");

    source.write_all(b"committed\n").unwrap();
    let deadline = Instant::now() + STARTUP_STOP_EXIT_BOUND;
    let status = loop {
        if let Some(status) = import.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "handoff import must stop on the SIGTERM it received before commit"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert!(status.success(), "exit status {status:?}");
    assert!(!api_socket.exists(), "api socket must be removed");
    let line = wait_for_log_line(&log, "server shutdown initiated", Duration::from_secs(5));
    assert!(line.contains("reason=SIGTERM"), "{line}");

    drop(import);
    cleanup_test_base(&base);
}
