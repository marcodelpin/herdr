//! Unix remote-host side of the SSH stdio bridge.

use std::io;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;


fn dbg3701(msg: &str) {
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/herdr-bridge-3701.log")
    {
        let pid = std::process::id();
        let _ = writeln!(f, "{pid} {msg}");
    }
}

pub(crate) fn run_remote_client_bridge() -> io::Result<()> {
    ensure_remote_server_running()?;

    let socket_path = crate::server::socket_paths::client_socket_path();
    let stream = UnixStream::connect(&socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to connect to remote Herdr client socket {}: {err}",
                socket_path.display()
            ),
        )
    })?;

    let mut stdout = io::stdout().lock();
    let mut socket_to_stdout = stream.try_clone()?;
    let mut stdin_to_socket = stream;

    let _upload = thread::spawn(move || {
        let mut stdin = io::stdin();
        let _ = copy_flush(&mut stdin, &mut stdin_to_socket, "up client->server");
        let _ = stdin_to_socket.shutdown(std::net::Shutdown::Write);
    });

    copy_flush(&mut socket_to_stdout, &mut stdout, "down server->client").map(|_| ())
}

fn copy_flush<R: io::Read, W: io::Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> io::Result<u64> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0;
    dbg3701(&format!("{label}: relay start"));
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => {
                dbg3701(&format!("{label}: EOF from reader, total={total}"));
                return Ok(total);
            }
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                dbg3701(&format!("{label}: READ ERROR {err}, total={total}"));
                return Err(err);
            }
        };
        dbg3701(&format!("{label}: read {read} (total_before={total})"));
        if let Err(err) = writer.write_all(&buffer[..read]) {
            dbg3701(&format!("{label}: WRITE ERROR {err} after {read}"));
            return Err(err);
        }
        if let Err(err) = writer.flush() {
            dbg3701(&format!("{label}: FLUSH ERROR {err} after {read}"));
            return Err(err);
        }
        total += read as u64;
        dbg3701(&format!("{label}: wrote {read}, total={total}"));
    }
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

    crate::server::autodetect::spawn_server_daemon()?;
    crate::server::autodetect::wait_for_server_socket(&socket_path, Duration::from_secs(5))
}
