use super::*;

/// How long past the commitment wait's own timeout a stop keeps waiting for
/// it: the read returns at its timeout, the margin covers scheduling.
#[cfg(unix)]
const HANDOFF_COMMIT_WAIT_MARGIN: Duration = Duration::from_secs(1);

/// How long a failed write of a transferred handoff session waits before the
/// next attempt, within the handoff save bound.
#[cfg(unix)]
const HANDOFF_SAVE_RETRY: Duration = Duration::from_millis(250);

/// A startup error after the API socket was bound. Handled only after the
/// API server drained, so no accepted connection is left without a reply
/// (herdr-ir4j, ADR-0014 rule 3).
struct StartupFailure {
    err: io::Error,
    /// The socket another server already serves, reported as "already
    /// running" with exit status 1.
    already_running: Option<(&'static str, PathBuf)>,
    /// What startup had built (the App or the server holding it), destroyed
    /// only after the drain: destroying the App ends its panes, with grace
    /// periods that can add up to seconds.
    held: Option<Box<dyn std::any::Any>>,
}

impl StartupFailure {
    fn holding(mut self, built: impl std::any::Any) -> Self {
        self.held = Some(Box::new(built));
        self
    }
}

impl From<io::Error> for StartupFailure {
    fn from(err: io::Error) -> Self {
        Self {
            err,
            already_running: None,
            held: None,
        }
    }
}

/// Leaves a startup that failed after the API bind: drains the API server
/// first, drops its handle (which unlinks the socket path) and what startup
/// had built only afterwards.
fn leave_failed_startup(
    startup_stop: &StartupStop,
    api_server: Option<api::ServerHandle>,
    failure: StartupFailure,
) -> io::Error {
    warn!(err = %failure.err, "server startup failed");
    startup_stop.fail_startup();
    drop(api_server);
    drop(failure.held);
    if let Some((socket, path)) = failure.already_running {
        eprintln!("error: herdr server is already running");
        eprintln!("{socket} socket: {}", path.display());
        crate::logging::shutdown("server");
        std::process::exit(1);
    }
    failure.err
}

/// Run the headless server. This is the entry point called from main.rs.
pub fn run_server() -> io::Result<()> {
    crate::platform::ignore_server_hangup();
    let args: Vec<String> = std::env::args().collect();
    let handoff_import = args.get(2).map(String::as_str) == Some("--handoff-import");
    let process_context = crate::platform::prepare_server_process(handoff_import);
    init_logging();
    match process_context {
        Ok(true) => info!("server using persistent user service context"),
        Ok(false) => {}
        Err(err) => {
            warn!(%err, "could not select persistent user service context; retaining inherited context")
        }
    }
    crate::platform::raise_server_nofile_limit();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;
    // The stop thread first, the signal handler second, both before anything
    // can accept a connection. No stop thread, no server: the default
    // disposition stays and nothing is bound (ADR-0014 rule 1).
    let startup_stop = match StartupStop::start() {
        Ok(startup_stop) => startup_stop,
        Err(err) => {
            tracing::error!(%err, "cannot start the startup stop thread; refusing to start the server");
            return Err(io::Error::new(
                err.kind(),
                format!("cannot start the startup stop thread: {err}"),
            ));
        }
    };
    let server_stop = crate::server::shutdown::ServerStop::default();
    {
        let _runtime = rt.enter();
        install_server_signal_handling(&server_stop, &startup_stop);
    }

    let result = if handoff_import {
        let socket_path = args
            .get(3)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff socket"))?;
        let token = args
            .get(4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing handoff token"))?;
        run_handoff_import_server(&rt, &startup_stop, server_stop, &socket_path, token)
    } else {
        run_ordinary_server(&rt, &startup_stop, server_stop)
    };

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("server");
    result
}

/// Probes and binds the API socket. The probe runs while a stop still ends
/// the process at once; from the bind on, a stop drains this server.
fn bind_api_server(
    startup_stop: &StartupStop,
    api_tx: &api::ApiRequestSender,
    event_hub: &api::EventHub,
    server_stop: &crate::server::shutdown::ServerStop,
) -> io::Result<api::ServerHandle> {
    // Logged first: probing a socket path that another listener holds can
    // wait, and a stalled start names it.
    info!(path = %api::socket_path().display(), "preparing the api socket");
    let probed = api::prepare_api_socket_path();
    startup_stop.api_socket_probed();
    probed?;
    let api_server = api::start_server_with_stop_control(
        api_tx.clone(),
        event_hub.clone(),
        server_stop.clone(),
    )?;
    startup_stop.api_bound(api_server.shutdown_control());
    Ok(api_server)
}

fn already_running_api(err: io::Error) -> StartupFailure {
    if err.kind() == io::ErrorKind::AddrInUse {
        StartupFailure {
            err,
            already_running: Some(("api", api::socket_path())),
            held: None,
        }
    } else {
        err.into()
    }
}

fn already_running_client(err: io::Error) -> StartupFailure {
    if err.kind() == io::ErrorKind::AddrInUse {
        StartupFailure {
            err,
            already_running: Some(("client", client_socket_path())),
            held: None,
        }
    } else {
        err.into()
    }
}

fn run_ordinary_server(
    rt: &tokio::runtime::Runtime,
    startup_stop: &StartupStop,
    server_stop: crate::server::shutdown::ServerStop,
) -> io::Result<()> {
    let loaded_config = config::Config::load();
    #[cfg(windows)]
    if loaded_config.config.server.allow_unelevated_clients {
        crate::platform::allow_unelevated_clients();
    }
    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();

    // Start the JSON API socket server.
    let api_server = match bind_api_server(startup_stop, &api_tx, &event_hub, &server_stop) {
        Ok(server) => server,
        Err(err) => {
            return Err(leave_failed_startup(
                startup_stop,
                None,
                already_running_api(err),
            ))
        }
    };
    hold_startup_after_api_bind_for_test();

    let built = {
        let _runtime = rt.enter();
        build_ordinary_server(
            &loaded_config.config,
            &loaded_config.diagnostics,
            api_tx,
            api_rx,
            event_hub,
            server_stop,
        )
    };
    let mut server = match built {
        Ok(server) => server,
        Err(failure) => {
            return Err(leave_failed_startup(
                startup_stop,
                Some(api_server),
                failure,
            ))
        }
    };
    server.api_server = Some(api_server);
    startup_stop.hand_over_to_event_loop(server.server_event_tx.clone());
    rt.block_on(server.run())
}

fn build_ordinary_server(
    config: &config::Config,
    diagnostics: &[String],
    api_tx: api::ApiRequestSender,
    api_rx: tokio::sync::mpsc::UnboundedReceiver<api::ApiRequestMessage>,
    event_hub: api::EventHub,
    server_stop: crate::server::shutdown::ServerStop,
) -> Result<HeadlessServer, StartupFailure> {
    // Create the App (with AppState, event channels, etc.).
    let mut app = app::App::try_new(
        config,
        app::AppPolicy::PRODUCTION,
        config::config_diagnostic_summary(diagnostics),
        api_rx,
        event_hub,
    )?;
    seed_startup_workspace_if_empty(&mut app);

    // The client socket first, while the App is still here: a failure hands
    // the App back, to be destroyed after the drain (ADR-0014 rule 3).
    let client_socket = match ClientSocket::bind(&server_stop) {
        Ok(client_socket) => client_socket,
        Err(err) => return Err(already_running_client(err).holding(app)),
    };
    // The API server handle stays with bootstrap until the event loop takes
    // the stop over (ADR-0014 rule 3).
    let mut server = HeadlessServer::new(
        app,
        diagnostics,
        Some(api_tx),
        None,
        server_stop,
        client_socket,
    );

    info!(
        api_socket = %api::socket_path().display(),
        client_socket = %client_socket_path().display(),
        "herdr server started"
    );
    print_ready_message(&api::socket_path(), &client_socket_path());
    server.app.run_plugin_startup_hooks();
    Ok(server)
}

/// Test hook: holds startup right after the API socket binds, so an
/// integration test can land a signal in the window before the event loop
/// runs (herdr-ir4j). The hold is acknowledged in the server log, so a test
/// knows the server is inside the window, and lasts until the file named by
/// `HERDR_TEST_HOLD_AFTER_API_BIND_RELEASE` exists or the given time passed.
/// Capped so a stray value cannot stall a real start for long.
fn hold_startup_after_api_bind_for_test() {
    let Some(millis) = std::env::var("HERDR_TEST_HOLD_AFTER_API_BIND_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    let until = Instant::now() + Duration::from_millis(millis.min(60_000));
    let release = std::env::var_os("HERDR_TEST_HOLD_AFTER_API_BIND_RELEASE").map(PathBuf::from);
    info!("startup held after the api bind for a test");
    while Instant::now() < until && !release.as_deref().is_some_and(Path::exists) {
        std::thread::sleep(Duration::from_millis(10));
    }
    info!("startup hold after the api bind released");
}

fn seed_startup_workspace_if_empty(app: &mut app::App) {
    let Some(cwd) = take_startup_cwd() else {
        return;
    };

    if !app.state.workspaces.is_empty() {
        info!(
            cwd = %cwd.display(),
            "restored session already has workspaces; ignoring startup cwd"
        );
        return;
    }

    match app.create_workspace_with_options(cwd.clone(), true) {
        Ok(_) => {
            info!(cwd = %cwd.display(), "created startup workspace");
        }
        Err(err) => {
            warn!(cwd = %cwd.display(), err = %err, "failed to create startup workspace");
            app.state.mode = app::Mode::Navigate;
        }
    }
}

fn take_startup_cwd() -> Option<PathBuf> {
    let cwd = std::env::var_os(crate::server::autodetect::STARTUP_CWD_ENV_VAR)?;
    std::env::remove_var(crate::server::autodetect::STARTUP_CWD_ENV_VAR);
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

#[cfg(unix)]
fn run_handoff_import_server(
    rt: &tokio::runtime::Runtime,
    startup_stop: &StartupStop,
    server_stop: crate::server::shutdown::ServerStop,
    socket_path: &Path,
    token: &str,
) -> io::Result<()> {
    let loaded_config = config::Config::load();
    let mut received = crate::server::handoff::receive(socket_path, token)?;
    crate::server::handoff::log_import_result(received.manifest.panes.len());

    let (api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_hub = api::EventHub::default();

    let mut imports = HashMap::new();
    for (pane, fd) in received
        .manifest
        .panes
        .drain(..)
        .zip(received.fds.drain(..))
    {
        let pane_id = pane.pane_id;
        imports.insert(
            pane_id,
            crate::handoff_runtime::ImportedHandoffRuntime {
                master_fd: fd,
                state: pane,
            },
        );
    }

    // Until `ready` is written the source keeps ownership: a stop here ends
    // the import at once and the source rolls back (ADR-0014 rule 2).
    let app = {
        let _runtime = rt.enter();
        app::App::new_from_handoff(
            &loaded_config.config,
            config::config_diagnostic_summary(&loaded_config.diagnostics),
            api_rx,
            event_hub.clone(),
            &received.manifest.snapshot,
            &mut imports,
        )?
    };
    crate::server::handoff::report_restored(&mut received.stream)?;
    if std::env::var("HERDR_TEST_HANDOFF_IMPORT_FAIL").as_deref() == Ok("after_restored") {
        return Err(io::Error::other(
            "test handoff import failure after restored",
        ));
    }
    wait_for_old_public_sockets_to_close(Duration::from_secs(5))?;

    let api_server = match bind_api_server(startup_stop, &api_tx, &event_hub, &server_stop) {
        Ok(server) => server,
        Err(err) => return Err(leave_failed_startup(startup_stop, None, err.into())),
    };
    let built = {
        let _runtime = rt.enter();
        build_handoff_import_server(
            startup_stop,
            &loaded_config.diagnostics,
            app,
            &mut received,
            api_tx,
            server_stop,
        )
    };
    let mut server = match built {
        Ok(server) => server,
        Err(failure) => {
            return Err(leave_failed_startup(
                startup_stop,
                Some(api_server),
                failure,
            ))
        }
    };
    server.api_server = Some(api_server);
    startup_stop.hand_over_to_event_loop(server.server_event_tx.clone());
    rt.block_on(server.run())
}

#[cfg(unix)]
fn build_handoff_import_server(
    startup_stop: &StartupStop,
    diagnostics: &[String],
    app: app::App,
    received: &mut crate::server::handoff::ReceivedHandoff,
    api_tx: api::ApiRequestSender,
    server_stop: crate::server::shutdown::ServerStop,
) -> Result<HeadlessServer, StartupFailure> {
    let client_socket = match ClientSocket::bind(&server_stop) {
        Ok(client_socket) => client_socket,
        Err(err) => return Err(StartupFailure::from(err).holding(app)),
    };
    let mut server = HeadlessServer::new(
        app,
        diagnostics,
        Some(api_tx),
        None,
        server_stop,
        client_socket,
    );
    // Carried across before any client attaches, so the first title sent is
    // the override rather than the configured one it replaced.
    server.api_window_title = received.manifest.api_window_title.take();
    // A restart announced before the handoff still commits after it.
    server
        .host_shutdown_intent
        .set(received.manifest.host_shutdown_intent);

    // A stop already recorded aborts the import before `ready`, so the source
    // keeps ownership. From `ready` on the source may commit at any moment:
    // the commitment wait is joined, within its own timeout, never abandoned.
    let commit_wait_ends =
        Instant::now() + crate::server::handoff::COMMIT_TIMEOUT + HANDOFF_COMMIT_WAIT_MARGIN;
    if !startup_stop.begin_commit_wait(commit_wait_ends) {
        return Err(StartupFailure::from(io::Error::other(
            "stop signal received before the handoff import was ready",
        ))
        .holding(server));
    }
    let decision = crate::server::handoff::report_ready(&mut received.stream)
        .and_then(|()| crate::server::handoff::wait_decision(&mut received.stream));
    match decision {
        Ok(crate::server::handoff::SourceDecision::Commit) => {}
        Ok(crate::server::handoff::SourceDecision::Abort { rollback_main_pid }) => {
            let err =
                abort_handoff_import(&mut received.stream, &server.server_stop, rollback_main_pid);
            return Err(StartupFailure::from(err).holding(server));
        }
        Err(err) => return Err(StartupFailure::from(err).holding(server)),
    }
    let save_by = startup_stop.committed();
    server.app.assume_handoff_ownership();
    server.app.unpause_handoff_readers();
    server.pending_handoff_repaint_nudge = true;
    if let Err(err) = crate::server::handoff::report_owned(&mut received.stream) {
        warn!(err = %err, "failed to report handoff ownership; continuing as owner");
    }
    // The source no longer saves this session: write it before any step that
    // can wait on another process, so a stop or a crash from here on cannot
    // lose what was transferred (ADR-0014 rule 2).
    save_transferred_session(startup_stop, &mut server.app, save_by);

    info!("handoff import server started");
    print_ready_message(&api::socket_path(), &client_socket_path());
    server.app.run_plugin_startup_hooks();
    Ok(server)
}

/// Writes the session a handoff transferred, retrying a failed write until
/// `save_by`. Only a write that reached the disk counts as saved; a session
/// that never did is reported to `startup_stop`, so a stop that gives startup
/// up exits with status 1 instead of as a success (ADR-0014 invariant I6).
#[cfg(unix)]
fn save_transferred_session(startup_stop: &StartupStop, app: &mut app::App, save_by: Instant) {
    // Held by the session writer, so it outlives startup: the event loop
    // cannot end the process as a success while it is owed.
    app.owe_transferred_session();
    loop {
        let Err(err) = app.try_save_session_now() else {
            startup_stop.saved();
            return;
        };
        if Instant::now() + HANDOFF_SAVE_RETRY >= save_by {
            tracing::error!(
                %err,
                "the transferred session was not written within the handoff save bound; continuing, the event loop keeps trying"
            );
            app.state.session_dirty = true;
            startup_stop.save_failed();
            return;
        }
        warn!(%err, "writing the transferred session failed; retrying");
        std::thread::sleep(HANDOFF_SAVE_RETRY);
    }
}

/// ADR-0018 D2: the source called the handoff off. Stop taking stops, give the
/// systemd main pid back when asked, report a stop that arrived here, and end
/// with an error, so that an exit as main process still reads as a failure.
#[cfg(unix)]
fn abort_handoff_import(
    stream: &mut std::os::unix::net::UnixStream,
    server_stop: &crate::server::shutdown::ServerStop,
    rollback_main_pid: Option<u32>,
) -> io::Error {
    let stop_before_close = !server_stop.close_intake_for_handoff();
    let rollback_done = match rollback_main_pid {
        None => false,
        Some(pid) => match crate::server::systemd_unit::send_main_pid_rollback(pid) {
            Ok(()) => true,
            Err(err) => {
                warn!(pid, err = %err, "could not give the systemd main pid back to the handoff source");
                false
            }
        },
    };
    let stop_requested = stop_before_close || server_stop.take_late_stop().is_some();
    if let Err(err) = crate::server::handoff::report_aborted(
        stream,
        crate::server::handoff::AbortReply {
            stop_requested,
            rollback_done,
        },
    ) {
        warn!(err = %err, "could not answer the handoff abort");
    }
    io::Error::other("live handoff aborted by the source server")
}

#[cfg(not(unix))]
fn run_handoff_import_server(
    _rt: &tokio::runtime::Runtime,
    _startup_stop: &StartupStop,
    _server_stop: crate::server::shutdown::ServerStop,
    _socket_path: &Path,
    _token: &str,
) -> io::Result<()> {
    Err(io::Error::other("live handoff is only supported on Unix"))
}

fn print_ready_message(api_socket: &Path, client_socket: &Path) {
    let message = format!(
        "herdr server running; you can use any herdr CLI command in another terminal.\n\
         api socket: {}\n\
         client socket: {}\n\
         logs: {}\n\
         did you mean to open the Herdr TUI? run `herdr`; you do not need `herdr server`.\n",
        api_socket.display(),
        client_socket.display(),
        crate::session::data_dir()
            .join("herdr-server.log")
            .display()
    );
    // The launching terminal may already be gone; the server keeps running.
    let _ = io::Write::write_all(&mut io::stderr(), message.as_bytes());
}

/// Initialize logging for the server process.
fn init_logging() {
    crate::logging::init_file_logging("herdr-server.log");
}
