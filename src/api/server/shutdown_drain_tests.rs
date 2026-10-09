//! Shutdown drain: a connection the server accepted gets a reply or an
//! explicit `server_unavailable` error, never a close with no bytes.
//!
//! Every case holds its connection at one boundary with a barrier (the test
//! owns the app side of the request channel, or the client withholds bytes),
//! then announces shutdown. None of them depends on timing to reach the
//! boundary.

use super::*;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::sync::atomic::AtomicU64;
use tokio::sync::mpsc;

/// Generous: only reached when the behavior under test is broken.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_socket_path(name: &str) -> PathBuf {
    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "herdr-drain-{name}-{}-{}.sock",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Line(String),
    ClosedWithoutReply,
    Failed(io::ErrorKind),
}

/// A client already reading its reply on a helper thread.
struct PendingReply(std::sync::mpsc::Receiver<Outcome>);

impl PendingReply {
    /// Fails the test instead of hanging it when no reply and no close come.
    fn wait(self, stage: &str) -> Outcome {
        self.0
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| panic!("{stage}: the client got neither a reply nor a close"))
    }
}

/// Starts reading one reply line. Use it before the server side writes when
/// that write waits for the client: a Windows reply is not delivered until the
/// client read it, so a test that reads only afterwards deadlocks itself.
fn start_reading_reply(stream: LocalStream) -> PendingReply {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let outcome = match BufReader::new(stream).read_line(&mut line) {
            Ok(_) if line.ends_with('\n') => Outcome::Line(line),
            Ok(_) => Outcome::ClosedWithoutReply,
            Err(err) => Outcome::Failed(err.kind()),
        };
        let _ = tx.send(outcome);
    });
    PendingReply(rx)
}

fn read_reply(stream: LocalStream) -> Outcome {
    start_reading_reply(stream).wait("reading the reply")
}

fn error_of(outcome: Outcome) -> (String, String, String) {
    let Outcome::Line(line) = outcome else {
        panic!("expected a reply line, got {outcome:?}");
    };
    let value: Value = serde_json::from_str(&line).expect("reply is one json line");
    let text = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("reply has no {pointer}: {line}"))
            .to_owned()
    };
    (text("/id"), text("/error/code"), text("/error/message"))
}

fn assert_shutting_down(outcome: Outcome, id: &str) {
    assert_eq!(
        error_of(outcome),
        (
            id.to_owned(),
            "server_unavailable".to_owned(),
            "server is shutting down".to_owned()
        )
    );
}

/// Serves one accepted stream the way the accept loop does: counted first,
/// then handed to its own thread.
fn serve_connection(
    drain: &Arc<ApiDrain>,
    api_tx: &ApiRequestSender,
    running: &Arc<AtomicBool>,
    stream: LocalStream,
) {
    let in_flight = drain.admit();
    let api_tx = api_tx.clone();
    let running = Arc::clone(running);
    std::thread::spawn(move || {
        let _ = handle_connection_with_stop(
            stream,
            &api_tx,
            &EventHub::default(),
            &running,
            None,
            ConnectionShutdown {
                server_stop: None,
                in_flight,
            },
            #[cfg(unix)]
            None,
        );
    });
}

/// The app side of the API: the test receives requests and decides whether a
/// responder is ever answered.
struct App {
    drain: Arc<ApiDrain>,
    running: Arc<AtomicBool>,
    api_tx: ApiRequestSender,
    api_rx: mpsc::UnboundedReceiver<ApiRequestMessage>,
    paths: Vec<PathBuf>,
}

impl App {
    fn new() -> Self {
        let (api_tx, api_rx) = mpsc::unbounded_channel();
        Self {
            drain: Arc::new(ApiDrain::default()),
            running: Arc::new(AtomicBool::new(true)),
            api_tx,
            api_rx,
            paths: Vec::new(),
        }
    }

    fn serve(&self, stream: LocalStream) {
        serve_connection(&self.drain, &self.api_tx, &self.running, stream);
    }

    /// An accepted connection and its client end.
    fn connect(&mut self, name: &str) -> LocalStream {
        use interprocess::local_socket::traits::Listener as _;

        let path = unique_socket_path(name);
        let listener = bind_local_listener(&path).unwrap();
        self.paths.push(path.clone());
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        self.serve(listener.accept().unwrap());
        client
    }

    /// Barrier: returns once the connection thread dispatched its request and
    /// waits for the app. The caller keeps the message, so the responder
    /// stays alive and unanswered.
    fn held_request(&mut self) -> ApiRequestMessage {
        recv_request_within(&mut self.api_rx)
    }

    /// What `ServerHandle::drain_for_shutdown` announces.
    fn begin_shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.drain.begin_shutdown();
    }

    fn assert_drained(&self) {
        assert_eq!(self.drain.wait_idle(Instant::now() + REPLY_TIMEOUT), Ok(()));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Windows listener marker files, and Unix socket files.
        for path in self.paths.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Bounded: a broken dispatcher fails the test instead of hanging it.
fn recv_request_within(
    api_rx: &mut mpsc::UnboundedReceiver<ApiRequestMessage>,
) -> ApiRequestMessage {
    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        match api_rx.try_recv() {
            Ok(request) => return request,
            Err(mpsc::error::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "request never reached the app");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(err) => panic!("app request channel closed: {err}"),
        }
    }
}

/// Runs `work` on its own thread and fails the test if it does not finish in
/// time, so a missing shutdown exit cannot hang the suite.
fn finish_within<T: Send + 'static>(stage: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(work());
    });
    done_rx
        .recv_timeout(REPLY_TIMEOUT)
        .unwrap_or_else(|_| panic!("{stage} did not finish"))
}

/// A request the app took and holds unanswered is counted as in flight under
/// its method and gets the explicit error once shutdown is announced.
fn assert_held_request_is_answered(name: &str, method: &'static str, params: &str) {
    let mut app = App::new();
    let mut client = app.connect(name);
    writeln!(
        client,
        r#"{{"id":"held","method":"{method}","params":{params}}}"#
    )
    .unwrap();

    let held = app.held_request();
    assert_eq!(
        app.drain.in_flight_stages(),
        vec![method],
        "a dispatched request must hold shutdown"
    );

    app.begin_shutdown();

    assert_shutting_down(read_reply(client), "held");
    app.assert_drained();
    drop(held);
}

#[test]
fn queued_request_is_answered_at_shutdown() {
    assert_held_request_is_answered("queued", "workspace.list", "{}");
}

#[test]
fn agent_prompt_without_wait_holds_shutdown_and_is_answered() {
    assert_held_request_is_answered(
        "prompt",
        "agent.prompt",
        r#"{"target":"agent_1","text":"hello"}"#,
    );
}

#[test]
fn agent_prompt_with_wait_holds_shutdown_and_is_answered() {
    assert_held_request_is_answered(
        "prompt-wait",
        "agent.prompt",
        r#"{"target":"agent_1","text":"hello","wait":{"timeout_ms":60000}}"#,
    );
}

#[test]
fn deferred_alt_screen_read_is_answered_at_shutdown() {
    // The app parks such a read in its pending or deferred list and stops
    // polling it at shutdown; the responder is never answered.
    assert_held_request_is_answered(
        "alt-screen",
        "pane.read",
        r#"{"pane_id":"pane_1","source":"recent","lines":200}"#,
    );
}

#[test]
fn deferred_worktree_operation_is_answered_at_shutdown() {
    // The completion of a worktree operation travels through the internal
    // event queue, which shutdown no longer processes.
    assert_held_request_is_answered("worktree", "worktree.create", r#"{"branch":"feature"}"#);
}

#[test]
fn response_sent_before_shutdown_wins_over_the_shutdown_error() {
    let mut app = App::new();
    let mut client = app.connect("answered");
    writeln!(
        client,
        r#"{{"id":"done","method":"workspace.list","params":{{}}}}"#
    )
    .unwrap();

    let held = app.held_request();
    // The app answered just before it announced shutdown. The connection
    // thread must deliver that response, not replace it with the error.
    held.respond_to
        .send(r#"{"id":"done","result":{"type":"ok"}}"#.to_owned())
        .unwrap();
    app.begin_shutdown();

    let Outcome::Line(line) = read_reply(client) else {
        panic!("expected the app response");
    };
    assert_eq!(line.trim_end(), r#"{"id":"done","result":{"type":"ok"}}"#);
    app.assert_drained();
}

#[test]
fn reply_being_written_holds_shutdown_until_the_client_has_it() {
    let mut app = App::new();
    let mut client = app.connect("writing");
    writeln!(
        client,
        r#"{{"id":"big","method":"workspace.list","params":{{}}}}"#
    )
    .unwrap();

    // Larger than any socket or pipe buffer: the write cannot finish while
    // the client is not reading.
    let reply = format!(
        r#"{{"id":"big","result":"{}"}}"#,
        "x".repeat(8 * 1024 * 1024)
    );
    app.held_request().respond_to.send(reply.clone()).unwrap();
    app.begin_shutdown();

    assert_eq!(
        app.drain
            .wait_idle(Instant::now() + Duration::from_millis(300)),
        Err(vec!["workspace.list"]),
        "shutdown must wait for a reply the client has not read yet"
    );

    let Outcome::Line(line) = read_reply(client) else {
        panic!("expected the full reply");
    };
    assert_eq!(
        line.len(),
        reply.len() + 1,
        "the reply must not be truncated"
    );
    app.assert_drained();
}

#[test]
fn client_still_sending_its_request_gets_an_explicit_error() {
    let mut app = App::new();
    let mut client = app.connect("slow");
    // No newline: the request is incomplete and stays so.
    client.write_all(br#"{"id":"slow","method":"pi"#).unwrap();
    client.flush().unwrap();

    app.begin_shutdown();

    assert_shutting_down(read_reply(client), "");
    app.assert_drained();
}

#[test]
fn connected_client_that_sent_nothing_gets_an_explicit_error() {
    let mut app = App::new();
    let client = app.connect("idle");

    app.begin_shutdown();

    assert_shutting_down(read_reply(client), "");
    app.assert_drained();
}

fn stream_pair(name: &str) -> (LocalStream, LocalStream, PathBuf) {
    use interprocess::local_socket::traits::Listener as _;

    let path = unique_socket_path(name);
    let listener = bind_local_listener(&path).unwrap();
    let client = crate::ipc::connect_local_stream(&path).unwrap();
    let server = listener.accept().unwrap();
    (client, server, path)
}

#[test]
fn wait_that_ends_because_the_server_stops_is_told_so() {
    let (client, mut server, path) = stream_pair("wait-stop");
    let running = AtomicBool::new(false);
    let reply = start_reading_reply(client);

    finish_wait_response(&mut server, None, "wait", "events.wait", false, &running).unwrap();
    drop(server);

    assert_shutting_down(reply.wait("wait ended by the server stopping"), "wait");
    let _ = std::fs::remove_file(path);
}

#[test]
fn wait_that_ends_because_the_client_left_writes_nothing() {
    let (client, mut server, path) = stream_pair("wait-left");
    let running = AtomicBool::new(true);

    finish_wait_response(&mut server, None, "wait", "events.wait", false, &running).unwrap();
    drop(server);

    assert_eq!(read_reply(client), Outcome::ClosedWithoutReply);
    let _ = std::fs::remove_file(path);
}

/// Connections already queued on the listener when shutdown begins were made
/// to a live server. The accept loop serves them before it acknowledges that
/// admission is closed, and nothing can connect after the acknowledgement.
#[test]
fn connections_queued_before_shutdown_are_served_and_later_ones_refused() {
    let app = App::new();
    let path = unique_socket_path("backlog");
    let listener = bind_local_listener(&path).unwrap();
    let identity = socket_file_identity(&path).unwrap();

    // A named pipe has one pending instance; a Unix socket has a backlog.
    let queued = if cfg!(windows) { 1 } else { 3 };
    let clients: Vec<LocalStream> = (0..queued)
        .map(|index| {
            let mut client = crate::ipc::connect_local_stream(&path).unwrap();
            writeln!(
                client,
                r#"{{"id":"q{index}","method":"ping","params":{{}}}}"#
            )
            .unwrap();
            client
        })
        .collect();

    // Nothing accepted them yet: the in-flight count is zero when shutdown
    // is announced, which is the state a sampled counter misreads as idle.
    assert!(app.drain.in_flight_stages().is_empty());
    app.begin_shutdown();
    assert!(
        !app.drain.wait_admission_closed(Instant::now()),
        "admission is not closed until the accept loop says so"
    );

    {
        let (path, drain, running, api_tx) = (
            path.clone(),
            Arc::clone(&app.drain),
            Arc::clone(&app.running),
            app.api_tx.clone(),
        );
        finish_within("the accept loop at shutdown", move || {
            serve_api_listener(listener, &path, &identity, &running, &drain, |stream| {
                serve_connection(&drain, &api_tx, &running, stream)
            });
        });
    }

    assert!(app.drain.admission_closed());
    for (index, client) in clients.into_iter().enumerate() {
        let Outcome::Line(line) = read_reply(client) else {
            panic!("queued connection {index} got no reply");
        };
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], format!("q{index}"));
        assert!(reply["result"].is_object(), "{line}");
    }
    app.assert_drained();

    assert!(!path.exists(), "the public name is removed");
    assert!(
        crate::ipc::connect_local_stream(&path).is_err(),
        "no connection is admitted after the acknowledgement"
    );
}

/// The whole path through `ServerHandle::drain_for_shutdown`: the accept
/// thread is blocked in `accept()` and has to be woken, acknowledge, and the
/// drain has to wait for a connection that is still in flight.
#[test]
fn drain_for_shutdown_wakes_the_accept_loop_and_answers_what_it_accepted() {
    let path = unique_socket_path("handle");
    let (api_tx, mut api_rx) = mpsc::unbounded_channel();
    let listener = bind_local_listener(&path).unwrap();
    let identity = socket_file_identity(&path).unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let drain = Arc::new(ApiDrain::default());
    let thread = {
        let (path, identity) = (path.clone(), identity.clone());
        let (running, drain) = (Arc::clone(&running), Arc::clone(&drain));
        std::thread::spawn(move || {
            serve_api_listener(listener, &path, &identity, &running, &drain, |stream| {
                serve_connection(&drain, &api_tx, &running, stream)
            });
        })
    };
    let handle = ServerHandle {
        _thread: thread,
        path: path.clone(),
        identity,
        running,
        drain,
    };

    let mut held_client = crate::ipc::connect_local_stream(&path).unwrap();
    writeln!(
        held_client,
        r#"{{"id":"held","method":"workspace.list","params":{{}}}}"#
    )
    .unwrap();
    let mut slow_client = crate::ipc::connect_local_stream(&path).unwrap();
    slow_client.write_all(br#"{"id":"slow","#).unwrap();
    slow_client.flush().unwrap();

    // Barrier: the first request is with the app, unanswered.
    let held = recv_request_within(&mut api_rx);
    // The clients read while the drain runs: a Windows reply counts as
    // delivered only once the client read it, and the drain waits for that.
    let held_reply = start_reading_reply(held_client);
    let slow_reply = start_reading_reply(slow_client);

    let (drained_tx, drained_rx) = std::sync::mpsc::channel();
    let drainer = std::thread::spawn(move || {
        handle.drain_for_shutdown();
        let _ = drained_tx.send(());
        handle
    });
    drained_rx
        .recv_timeout(API_SHUTDOWN_DRAIN_TIMEOUT + REPLY_TIMEOUT)
        .expect("drain_for_shutdown did not return within its own bound");
    let handle = drainer.join().expect("drain thread panicked");

    assert!(
        handle.drain.admission_closed(),
        "the accept loop acknowledged"
    );
    assert!(
        handle.drain.in_flight_stages().is_empty(),
        "the drain returned only after every accepted connection finished"
    );
    assert_shutting_down(held_reply.wait("held request at shutdown"), "held");
    assert_shutting_down(slow_reply.wait("slow client at shutdown"), "");
    assert!(!path.exists());
    assert!(crate::ipc::connect_local_stream(&path).is_err());
    drop(held);
}

/// A named pipe drops unread bytes when the server end closes, so a reply
/// only counts as delivered once the client has read it.
#[cfg(windows)]
#[test]
fn windows_final_reply_is_not_delivered_until_the_client_read_it() {
    use interprocess::local_socket::traits::Listener as _;

    let path = unique_socket_path("flush");
    let listener = bind_local_listener(&path).unwrap();
    let client = crate::ipc::connect_local_stream(&path).unwrap();
    let mut server = listener.accept().unwrap();

    let drain = draining_drain();
    let (writing_tx, writing_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let _connection = enter_connection(drain);
        let _ = writing_tx.send(());
        let result = write_final_reply(&mut server, r#"{"id":"flush"}"#);
        let _ = done_tx.send((result, confirmations_started_on_this_thread()));
    });

    writing_rx
        .recv_timeout(REPLY_TIMEOUT)
        .expect("the writer thread started");
    // One-sided: a writer that does not wait for the client finishes at once.
    assert!(
        done_rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "the reply must not count as delivered before the client read it"
    );

    assert_eq!(
        read_reply(client),
        Outcome::Line("{\"id\":\"flush\"}\n".to_owned())
    );
    let (result, confirmations) = done_rx
        .recv_timeout(REPLY_TIMEOUT)
        .expect("the writer returns once the client has the reply");
    result.unwrap();
    assert_eq!(
        confirmations, 1,
        "the writer went through delivery confirmation"
    );
    writer.join().unwrap();
    let _ = std::fs::remove_file(path);
}

/// Delivering a reply waits for the client, but never past the write timeout:
/// a client that never reads must not hold its connection, and with it the
/// shutdown drain, forever.
#[cfg(windows)]
#[test]
fn windows_final_reply_to_a_client_that_never_reads_is_abandoned_at_the_write_timeout() {
    use interprocess::local_socket::traits::Listener as _;

    let path = unique_socket_path("never-reads");
    let listener = bind_local_listener(&path).unwrap();
    let client = crate::ipc::connect_local_stream(&path).unwrap();
    let mut server = listener.accept().unwrap();

    let drain = draining_drain();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let _connection = enter_connection(drain);
        let result = write_final_reply(&mut server, r#"{"id":"unread"}"#);
        let _ = done_tx.send(result.map_err(|err| err.kind()));
    });

    let result = done_rx
        .recv_timeout(STREAM_WRITE_TIMEOUT + REPLY_TIMEOUT)
        .expect("the writer is still waiting for a client that never reads");
    assert_eq!(result, Ok(()), "the write itself succeeded");
    assert!(
        started.elapsed() >= STREAM_WRITE_TIMEOUT - Duration::from_millis(100),
        "the writer gave up before the write timeout: {:?}",
        started.elapsed()
    );
    drop(client);
    let _ = std::fs::remove_file(path);
}

#[cfg(windows)]
fn draining_drain() -> Arc<ApiDrain> {
    let drain = Arc::new(ApiDrain::default());
    drain.begin_shutdown();
    drain
}

/// A subscription whose setup waits for the app has not replied yet: it holds
/// shutdown and gets the explicit error, like any request.
#[test]
fn subscription_waiting_for_its_setup_holds_shutdown_and_is_answered() {
    let mut app = App::new();
    let mut client = app.connect("subscribe-setup");
    writeln!(
        client,
        r#"{{"id":"sub","method":"events.subscribe","params":{{"subscriptions":[{{"type":"pane.output_matched","pane_id":"pane_1","source":"recent","match":{{"type":"substring","value":"never"}}}}]}}}}"#
    )
    .unwrap();

    let held = app.held_request();
    assert_eq!(
        app.drain.in_flight_stages(),
        vec!["events.subscribe"],
        "a subscription still setting up must hold shutdown"
    );

    app.begin_shutdown();

    // The setup reports the failed probe as its own error; what matters here
    // is that the client gets an explicit error line for its request.
    let (id, code, _) = error_of(read_reply(client));
    assert_eq!(id, "sub");
    assert!(!code.is_empty());
    app.assert_drained();
    drop(held);
}

/// Once `subscription_started` is written the stream has no final reply to
/// wait for and must not hold shutdown.
#[test]
fn established_subscription_does_not_hold_shutdown() {
    let mut app = App::new();
    let mut client = app.connect("subscribe-started");
    writeln!(
        client,
        r#"{{"id":"sub","method":"events.subscribe","params":{{"subscriptions":[{{"type":"workspace.renamed"}}]}}}}"#
    )
    .unwrap();
    let reader = BufReader::new(client);
    let (line, reader) = finish_within("reading subscription_started", move || {
        let mut reader = reader;
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        (line, reader)
    });
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["result"]["type"], "subscription_started", "{line}");
    app.assert_drained();
    drop(reader);
}

/// Normal replies keep the base behaviour; only a draining server confirms
/// delivery, so a client that never reads cannot pin a helper thread and a
/// duplicated handle for the server's lifetime.
#[test]
fn delivery_is_confirmed_only_while_the_server_drains() {
    let drain = Arc::new(ApiDrain::default());

    let (client, mut server, path) = stream_pair("confirm-normal");
    let reply = start_reading_reply(client);
    {
        let _connection = enter_connection(Arc::clone(&drain));
        write_final_reply(&mut server, r#"{"id":"normal"}"#).unwrap();
    }
    drop(server);
    assert_eq!(
        reply.wait("normal reply"),
        Outcome::Line("{\"id\":\"normal\"}\n".to_owned())
    );
    assert_eq!(
        confirmations_started_on_this_thread(),
        0,
        "no confirmation in normal operation"
    );
    let _ = std::fs::remove_file(path);

    let (client, mut server, path) = stream_pair("confirm-draining");
    let reply = start_reading_reply(client);
    drain.begin_shutdown();
    {
        let _connection = enter_connection(Arc::clone(&drain));
        write_final_reply(&mut server, r#"{"id":"draining"}"#).unwrap();
    }
    drop(server);
    assert_eq!(
        reply.wait("draining reply"),
        Outcome::Line("{\"id\":\"draining\"}\n".to_owned())
    );
    assert_eq!(
        confirmations_started_on_this_thread(),
        1,
        "a draining server confirms delivery"
    );
    let _ = std::fs::remove_file(path);
}

/// When the confirmation machinery cannot start, nothing is known about the
/// client: the reply keeps its connection until the bound instead of being
/// treated as delivered.
#[test]
fn confirmation_that_cannot_start_holds_the_reply_until_the_bound() {
    crate::thread_spawn::test_hook::fail_next_spawns(1);
    let err =
        crate::ipc::run_with_bound("confirm-no-thread", REPLY_TIMEOUT, || Ok(())).unwrap_err();
    assert!(is_confirmation_not_started(&err), "{err}");

    let deadline = Instant::now() + Duration::from_millis(200);
    settle_reply_confirmation(Err(err), deadline);
    assert!(Instant::now() >= deadline, "released before the bound");
}

/// The write timeout bounds the whole reply line, not each socket operation:
/// a client that keeps reading slowly must not stretch it.
#[cfg(unix)]
#[test]
fn reply_write_stops_at_its_deadline_even_while_the_client_keeps_reading() {
    use std::io::Read;

    let (mut client, mut server, path) = stream_pair("deadline");
    let started = Instant::now();
    let reader = std::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        let mut total = 0_usize;
        // Each read frees room well within any per-operation send timeout.
        while started.elapsed() < Duration::from_secs(3) {
            match client.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(read) => total += read,
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        total
    });

    let value = "x".repeat(16 * 1024 * 1024);
    let result = write_reply_line_by(&mut server, &value, started + Duration::from_millis(300));
    let elapsed = started.elapsed();
    drop(server);
    let read = reader.join().unwrap();

    assert_eq!(
        result.map_err(|err| err.kind()),
        Err(io::ErrorKind::TimedOut)
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the write ran {elapsed:?} past a 300 ms deadline"
    );
    assert!(read > 0, "the client was reading");
    let _ = std::fs::remove_file(path);
}
