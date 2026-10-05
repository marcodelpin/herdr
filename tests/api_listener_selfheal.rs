//! Subprocess tests for the in-process api listener supervision (herdr-x843):
//! a dead api listener is respawned and the server never exits on its own over
//! it, while a requested shutdown is never undone by a respawn.

#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid,
};

const FAULT_ENV: &str = "HERDR_TEST_API_LISTENER_PANIC_AFTER";
const RESPAWN_LOG: &str = "api listener respawned";
const DEATH_LOG: &str = "api listener died while the server is running";

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-x843-selfheal-{}-{nanos}",
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
        let _ = self.child.kill();
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

fn spawn_server(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
    fault_after: u64,
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
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    cmd.env("HERDR_LOG", "herdr=debug");
    cmd.env(FAULT_ENV, fault_after.to_string());

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: Some(pair.master),
        child,
    }
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

fn request(socket_path: &Path, line: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{line}")?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_string())
}

fn ping(socket_path: &Path) -> std::io::Result<String> {
    request(socket_path, r#"{"id":"1","method":"ping","params":{}}"#)
}

fn server_log(base: &Path) -> String {
    fn walk(dir: &Path, out: &mut String) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path
                .file_name()
                .is_some_and(|name| name == "herdr-server.log")
            {
                out.push_str(&fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    let mut out = String::new();
    walk(base, &mut out);
    out
}

fn wait_for_log(base: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let log = server_log(base);
        if log.contains(needle) || Instant::now() > deadline {
            return log;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

// (1) an injected listener death is followed by a respawn and the API answers
// again; (3) the process does not exit over the dead listener.
#[test]
fn listener_selfheal_respawns_and_answers_after_injected_death() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, 1);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    // wait_for_socket opened and closed one connection: that was connection 1,
    // served inline, and the listener panicked right after it.
    let log = wait_for_log(&base, RESPAWN_LOG, Duration::from_secs(10));
    assert!(log.contains(DEATH_LOG), "listener death not logged:\n{log}");
    assert!(log.contains(RESPAWN_LOG), "listener not respawned:\n{log}");

    let deadline = Instant::now() + Duration::from_secs(10);
    let response = loop {
        match ping(&api_socket) {
            Ok(response) if response.contains("pong") => break response,
            other if Instant::now() > deadline => panic!("api did not answer again: {other:?}"),
            _ => thread::sleep(Duration::from_millis(50)),
        }
    };
    assert!(response.contains("pong"));
    for _ in 0..3 {
        assert!(ping(&api_socket).unwrap().contains("pong"));
    }

    thread::sleep(Duration::from_millis(300));
    assert!(
        spawned.child.try_wait().unwrap().is_none(),
        "the server must stay up over a listener death"
    );
    assert_eq!(server_log(&base).matches(RESPAWN_LOG).count(), 1);

    cleanup_spawned_herdr(spawned, base);
}

// (2) a listener death after server.stop is not respawned and the server
// exits cleanly through its normal shutdown.
#[test]
fn listener_selfheal_no_respawn_after_server_stop() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    // Connection 1 is the readiness probe, connection 2 the ping, connection
    // 3 is server.stop: served inline, then the listener panics.
    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket, 3);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    assert!(ping(&api_socket).unwrap().contains("pong"));
    let stopped = request(
        &api_socket,
        r#"{"id":"stop","method":"server.stop","params":{}}"#,
    )
    .unwrap();
    assert!(
        stopped.contains("\"stop\""),
        "server.stop failed: {stopped}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = spawned.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "server did not exit after server.stop"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "clean exit expected, got {status:?}");
    let log = server_log(&base);
    assert!(!log.contains(RESPAWN_LOG), "respawned after stop:\n{log}");
    assert!(!log.contains(DEATH_LOG), "stop treated as a death:\n{log}");
    assert!(!api_socket.exists(), "api socket must be gone after stop");

    cleanup_spawned_herdr(spawned, base);
}
