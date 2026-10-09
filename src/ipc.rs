use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;

pub(crate) type LocalListener = interprocess::local_socket::Listener;
pub(crate) type LocalStream = interprocess::local_socket::Stream;

pub(crate) enum LocalStreamRead {
    Data,
    Pending,
    Closed,
}

pub(crate) enum LocalStreamReadCount {
    Data(usize),
    Pending,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    marker: Vec<u8>,
}

pub(crate) fn connect_local_stream(path: &Path) -> io::Result<LocalStream> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{prelude::*, GenericFilePath};

        let name = path.to_fs_name::<GenericFilePath>()?;
        LocalStream::connect(name)
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{prelude::*, GenericNamespaced};

        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        LocalStream::connect(name)
    }
}

/// Connects like `connect_local_stream`, giving up after `within` when the
/// listener does not take the connection (every Windows pipe instance busy,
/// a full Unix backlog).
pub(crate) fn connect_local_stream_within(
    path: &Path,
    within: std::time::Duration,
) -> io::Result<LocalStream> {
    use interprocess::local_socket::ConnectOptions;
    use interprocess::ConnectWaitMode;

    #[cfg(unix)]
    let name = {
        use interprocess::local_socket::{prelude::*, GenericFilePath};
        path.to_fs_name::<GenericFilePath>()?
    };
    #[cfg(windows)]
    let name = {
        use interprocess::local_socket::{prelude::*, GenericNamespaced};
        path.to_string_lossy()
            .to_string()
            .to_ns_name::<GenericNamespaced>()?
    };
    ConnectOptions::new()
        .name(name)
        .wait_mode(ConnectWaitMode::Timeout(
            within.max(std::time::Duration::from_millis(1)),
        ))
        .connect_sync()
}

pub(crate) fn bind_local_listener(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{prelude::*, GenericFilePath, ListenerOptions};

        let name = path.to_fs_name::<GenericFilePath>()?;
        ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .create_sync()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};

        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        let listener = ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .create_sync()?;
        fs::write(path, windows_socket_marker())?;
        Ok(listener)
    }
}

pub(crate) fn prepare_socket_path(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    if !path.exists() {
        return Ok(());
    }

    match connect_local_stream(path) {
        Ok(_) => {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, busy_message(path)));
        }
        Err(err) if stale_socket_connect_error(err.kind()) => {}
        Err(err) => return Err(err),
    }

    if let Err(err) = fs::remove_file(path) {
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
    }

    Ok(())
}

fn stale_socket_connect_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound | io::ErrorKind::TimedOut
    ) || (cfg!(windows) && kind == io::ErrorKind::WouldBlock)
}

pub(crate) fn local_stream_peer_closed(stream: &mut LocalStream) -> io::Result<bool> {
    probe_stream_closed(stream)
}

/// Waits at most `timeout` for room to write on a nonblocking stream. Returns
/// on readiness, on timeout, or on a signal; the caller rechecks its deadline.
#[cfg(unix)]
pub(crate) fn wait_local_stream_writable(
    stream: &LocalStream,
    timeout: std::time::Duration,
) -> io::Result<()> {
    use std::os::fd::{AsFd, AsRawFd};

    let LocalStream::UdSocket(socket) = stream;
    let mut poll_fd = libc::pollfd {
        fd: socket.as_fd().as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int;
    // SAFETY: one valid pollfd for a descriptor borrowed from a live stream.
    if unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) } < 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(())
}

pub(crate) fn set_local_stream_polling(stream: &mut LocalStream, enabled: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        stream.set_nonblocking(enabled)
    }

    #[cfg(windows)]
    {
        let _ = (stream, enabled);
        Ok(())
    }
}

/// Binds a listener for private terminal traffic. Unix callers restrict the
/// socket file after binding; Windows must set the named-pipe DACL at creation.
pub(crate) fn bind_private_local_listener(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        bind_local_listener(path)
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        use widestring::U16CString;

        let sddl = U16CString::from_str("D:P(A;;GA;;;SY)(A;;GA;;;OW)")
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let security_descriptor = SecurityDescriptor::deserialize(&sddl)?;
        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        let listener = ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .security_descriptor(security_descriptor)
            .create_sync()?;
        fs::write(path, windows_socket_marker())?;
        Ok(listener)
    }
}

pub(crate) fn poll_local_stream_read(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamRead> {
    match poll_local_stream_read_count(stream, buf)? {
        LocalStreamReadCount::Data(read) => {
            let _ = read;
            Ok(LocalStreamRead::Data)
        }
        LocalStreamReadCount::Pending => Ok(LocalStreamRead::Pending),
        LocalStreamReadCount::Closed => Ok(LocalStreamRead::Closed),
    }
}

pub(crate) fn poll_local_stream_read_count(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamReadCount> {
    #[cfg(unix)]
    {
        match stream.read(buf) {
            Ok(0) => Ok(LocalStreamReadCount::Closed),
            Ok(read) => Ok(LocalStreamReadCount::Data(read)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                Ok(LocalStreamReadCount::Pending)
            }
            Err(err) => Err(err),
        }
    }

    #[cfg(windows)]
    {
        match windows_named_pipe_available(stream)? {
            None => Ok(LocalStreamReadCount::Closed),
            Some(0) => Ok(LocalStreamReadCount::Pending),
            Some(_) => match stream.read(buf) {
                Ok(0) => Ok(LocalStreamReadCount::Closed),
                Ok(read) => Ok(LocalStreamReadCount::Data(read)),
                Err(err) if is_connection_closed_error(&err) => Ok(LocalStreamReadCount::Closed),
                Err(err) => Err(err),
            },
        }
    }
}

#[cfg(unix)]
fn probe_stream_closed(stream: &mut LocalStream) -> io::Result<bool> {
    stream.set_nonblocking(true)?;
    let mut probe = [0u8; 1];
    let status = match stream.read(&mut probe) {
        Ok(0) => Ok(true),
        Ok(_) => Ok(true),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(err) if is_connection_closed_error(&err) => Ok(true),
        Err(err) => Err(err),
    };
    stream.set_nonblocking(false)?;
    status
}

#[cfg(windows)]
fn probe_stream_closed(stream: &mut LocalStream) -> io::Result<bool> {
    Ok(windows_named_pipe_available(stream)?.is_none())
}

#[cfg(windows)]
fn windows_named_pipe_available(stream: &mut LocalStream) -> io::Result<Option<u32>> {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    let LocalStream::NamedPipe(pipe) = stream;
    let mut available = 0;
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::PeekNamedPipe(
            pipe.as_handle().as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(Some(available));
    }

    let err = io::Error::last_os_error();
    if is_connection_closed_error(&err) || windows_named_pipe_closed_error(&err) {
        return Ok(None);
    }
    Err(err)
}

pub(crate) fn is_connection_closed_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WriteZero
    )
}

#[cfg(windows)]
fn windows_named_pipe_closed_error(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(6 | 109 | 232 | 233))
}

/// Upper bound on connections taken out of the listen backlog at shutdown.
const MAX_PENDING_CONNECTIONS_AT_SHUTDOWN: usize = 4096;

/// Makes the listener refuse further connections where the platform can do it
/// atomically. Linux fails `connect()` on a shut-down listening socket under
/// the same lock that queues a connection, so every connection is either
/// already queued or refused. Elsewhere this changes nothing and the caller
/// relies on removing the public name and closing the listener.
pub(crate) fn refuse_new_local_connections(listener: &LocalListener) {
    #[cfg(unix)]
    {
        use std::os::fd::{AsFd, AsRawFd};

        let LocalListener::UdSocket(listener) = listener;
        // SAFETY: the descriptor is borrowed from a live listener for this call.
        let _ = unsafe { libc::shutdown(listener.as_fd().as_raw_fd(), libc::SHUT_RD) };
    }

    #[cfg(windows)]
    {
        let _ = listener;
    }
}

/// Takes every connection already queued on the listener, without blocking.
/// The listener stays in nonblocking accept mode; the caller closes it next.
pub(crate) fn take_pending_local_connections(listener: &LocalListener) -> Vec<LocalStream> {
    // Unix builds import the stream trait at module level.
    #[cfg(windows)]
    use interprocess::local_socket::traits::Stream as _;
    use interprocess::local_socket::{traits::Listener as _, ListenerNonblockingMode};

    let mut pending = Vec::new();
    if listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .is_err()
    {
        return pending;
    }
    while pending.len() < MAX_PENDING_CONNECTIONS_AT_SHUTDOWN {
        match listener.accept() {
            Ok(stream) => {
                // A Windows pipe instance created in nonblocking accept mode
                // hands out a nonblocking stream, whose writes can stop short.
                // The connection handler expects a blocking stream.
                if let Err(err) = stream.set_nonblocking(false) {
                    tracing::debug!(err = %err, "queued api connection stays nonblocking");
                }
                pending.push(stream);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    pending
}

/// Returns once the peer has received everything written to `stream`, or
/// fails with `TimedOut` once `bound` has passed.
///
/// A Unix socket keeps written bytes readable after the writer closes. A
/// Windows named pipe discards them when the server end closes, and
/// `interprocess` only flushes a dropped stream from a background thread that
/// process exit kills, so the writer has to wait here itself.
pub(crate) fn wait_until_peer_received(
    stream: &LocalStream,
    bound: std::time::Duration,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = (stream, bound);
        Ok(())
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;

        let LocalStream::NamedPipe(pipe) = stream;
        // FlushFileBuffers on a pipe returns only once the client has read
        // everything, and has no timeout of its own: a client that never reads
        // would hold the caller forever. It runs on a duplicate of the handle on
        // a helper thread, which ends when the client reads or disconnects, or
        // with the process; the caller stops waiting at the bound.
        let handle = pipe
            .inner()
            .as_handle()
            .try_clone_to_owned()
            .map_err(confirmation_not_started)?;
        let pipe = std::fs::File::from(handle);
        run_with_bound("herdr-api-flush", bound, move || pipe.sync_all())
    }
}

/// The work of a bounded wait could not even start: the outcome it would have
/// observed is unknown, not a failure of the peer.
#[derive(Debug)]
struct ConfirmationNotStarted(io::Error);

impl std::fmt::Display for ConfirmationNotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "delivery confirmation could not start: {}", self.0)
    }
}

impl std::error::Error for ConfirmationNotStarted {}

#[cfg(any(windows, test))]
fn confirmation_not_started(err: io::Error) -> io::Error {
    io::Error::new(err.kind(), ConfirmationNotStarted(err))
}

pub(crate) fn is_confirmation_not_started(err: &io::Error) -> bool {
    err.get_ref()
        .is_some_and(|inner| inner.is::<ConfirmationNotStarted>())
}

/// Runs `work` on its own thread and waits at most `bound` for its result.
/// On timeout the thread is left to finish on its own.
#[cfg(any(windows, test))]
pub(crate) fn run_with_bound(
    name: &str,
    bound: std::time::Duration,
    work: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    use std::sync::mpsc::{sync_channel, RecvTimeoutError};

    let (done_tx, done_rx) = sync_channel(1);
    crate::thread_spawn::spawn_named(name, move || {
        let _ = done_tx.send(work());
    })
    .map_err(confirmation_not_started)?;
    match done_rx.recv_timeout(bound) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{name} did not finish within {} ms", bound.as_millis()),
        )),
        Err(RecvTimeoutError::Disconnected) => {
            Err(io::Error::other(format!("{name} ended without a result")))
        }
    }
}

pub(crate) fn socket_file_identity(path: &Path) -> io::Result<SocketFileIdentity> {
    #[cfg(windows)]
    {
        Ok(SocketFileIdentity {
            marker: fs::read(path)?,
        })
    }

    #[cfg(unix)]
    {
        let metadata = fs::metadata(path)?;
        Ok(SocketFileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

pub(crate) fn remove_socket_file_if_owned(
    path: &Path,
    identity: &SocketFileIdentity,
) -> io::Result<()> {
    let current = match socket_file_identity(path) {
        Ok(current) => current,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if current != *identity {
        return Ok(());
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(windows)]
fn windows_socket_marker() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}:{now}", std::process::id())
}

#[cfg(unix)]
pub(crate) fn restrict_socket_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

#[cfg(windows)]
pub(crate) fn restrict_socket_permissions(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    #[test]
    fn run_with_bound_returns_the_result_of_work_that_finishes() {
        assert!(run_with_bound("bound-ok", Duration::from_secs(10), || Ok(())).is_ok());
        let err = run_with_bound("bound-err", Duration::from_secs(10), || {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    /// The Windows reply flush has no timeout of its own: a client that never
    /// reads must not hold the writer past its bound.
    #[test]
    fn run_with_bound_stops_waiting_for_work_that_never_finishes() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let result = run_with_bound("bound-stuck", Duration::from_millis(200), move || {
                let _ = release_rx.recv();
                Ok(())
            });
            let _ = outcome_tx.send(result.map_err(|err| err.kind()));
        });

        let outcome = outcome_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("run_with_bound kept waiting past its bound");
        assert_eq!(outcome, Err(io::ErrorKind::TimedOut));
        assert!(started.elapsed() >= Duration::from_millis(200));
        drop(release_tx);
    }

    use super::*;
    #[cfg(windows)]
    use interprocess::local_socket::traits::Listener as _;
    #[cfg(windows)]
    use std::path::PathBuf;

    #[test]
    fn stale_socket_connect_errors_keep_unix_would_block_strict() {
        assert!(stale_socket_connect_error(io::ErrorKind::ConnectionRefused));
        assert!(stale_socket_connect_error(io::ErrorKind::NotFound));
        assert!(stale_socket_connect_error(io::ErrorKind::TimedOut));
        assert_eq!(
            stale_socket_connect_error(io::ErrorKind::WouldBlock),
            cfg!(windows)
        );
    }

    #[cfg(windows)]
    #[test]
    fn private_named_pipe_accepts_same_user() {
        use std::io::Write as _;

        let path = temp_socket_marker_path("private-pipe");
        let _ = fs::remove_file(&path);
        let listener = bind_private_local_listener(&path).unwrap();
        let mut client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();
        client.write_all(b"remote").unwrap();

        let mut buffer = [0_u8; 16];
        assert!(matches!(
            poll_local_stream_read_count(&mut server, &mut buffer).unwrap(),
            LocalStreamReadCount::Data(6)
        ));
        assert_eq!(&buffer[..6], b"remote");

        drop(client);
        drop(server);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn remove_socket_file_if_owned_compares_windows_marker_contents() {
        let path = temp_socket_marker_path("same-len-marker");
        let _ = fs::remove_file(&path);

        fs::write(&path, b"marker-aa").expect("write first marker");
        let identity = socket_file_identity(&path).expect("read first identity");
        fs::write(&path, b"marker-bb").expect("replace with same-length marker");

        remove_socket_file_if_owned(&path, &identity).expect("remove owned marker");

        assert!(path.exists(), "same-length replacement marker must survive");

        let _ = fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn idle_named_pipe_peer_is_not_treated_as_closed() {
        let path = temp_socket_marker_path("idle-pipe");
        let listener = bind_local_listener(&path).unwrap();
        let _client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();

        assert!(!local_stream_peer_closed(&mut server).unwrap());

        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn disconnected_named_pipe_peer_is_treated_as_closed() {
        let path = temp_socket_marker_path("disconnected-pipe");
        let listener = bind_local_listener(&path).unwrap();
        let client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();

        drop(client);

        assert!(local_stream_peer_closed(&mut server).unwrap());

        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    fn temp_socket_marker_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("herdr-{name}-{}.sock", std::process::id()))
    }
}
