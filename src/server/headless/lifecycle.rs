use super::*;

const LIVE_HANDOFF_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(6);

pub(super) fn wait_for_live_handoff_response_write(
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
) {
    let Some(response_write_complete) = response_write_complete else {
        return;
    };

    match response_write_complete.recv_timeout(LIVE_HANDOFF_RESPONSE_WRITE_TIMEOUT) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            warn!("timed out waiting for live handoff response write; old server exiting");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            warn!("live handoff response writer disconnected; old server exiting");
        }
    }
}

impl HeadlessServer {
    #[cfg(unix)]
    pub(super) fn perform_live_handoff(
        &mut self,
        params: crate::api::schema::ServerLiveHandoffParams,
    ) -> io::Result<()> {
        info!("starting live handoff");
        // ADR-0018 D1: a server that leads a systemd unit moves the unit's main
        // pid before it commits, or the unit ends every pane together with it.
        let supervision = match crate::server::systemd_unit::probe() {
            Ok(supervision) => supervision,
            Err(err) => {
                self.handoff_in_progress = false;
                warn!(err = %err, "live handoff refused");
                return Err(io::Error::other(format!("live handoff refused: {err}")));
            }
        };
        let import_exe = params.import_exe.as_deref().map(std::path::PathBuf::from);
        let socket_path = crate::server::handoff::handoff_socket_path();
        let token = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let listener = match crate::server::handoff::bind_listener(&socket_path) {
            Ok(listener) => listener,
            Err(err) => {
                self.handoff_in_progress = false;
                return Err(err);
            }
        };

        let mut pane_by_terminal = HashMap::new();
        for ws in &self.app.state.workspaces {
            for tab in &ws.tabs {
                for (pane_id, pane) in &tab.panes {
                    pane_by_terminal.insert(pane.attached_terminal_id.clone(), pane_id.raw());
                }
            }
        }

        self.handoff_in_progress = true;
        self.disconnect_all_clients_for_handoff();
        let _ = reject_pending_client_connections(&self.client_listener);

        let mut paused_terminal_ids = Vec::new();
        for terminal_id in pane_by_terminal.keys() {
            if let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) {
                if let Err(err) = runtime.pause_handoff_reader(Duration::from_secs(2)) {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    return Err(err);
                }
                paused_terminal_ids.push(terminal_id.clone());
            }
        }

        let snapshot = crate::persist::capture(
            &self.app.state.workspaces,
            &self.app.state.terminals,
            &self.app.terminal_runtimes,
            self.app.state.active,
            self.app.state.selected,
        );

        let mut handoff_entries = Vec::new();
        for (terminal_id, runtime) in self.app.terminal_runtimes.iter() {
            let Some(pane_id) = pane_by_terminal.get(terminal_id).copied() else {
                continue;
            };
            let mut handoff_runtime = runtime.handoff_runtime_state(pane_id);
            handoff_runtime.agent_state = self
                .app
                .state
                .terminals
                .get(terminal_id)
                .and_then(|terminal| terminal.handoff_agent_state());
            let has_agent_session = self
                .app
                .state
                .terminals
                .get(terminal_id)
                .is_some_and(|terminal| terminal.persisted_agent_session.is_some());
            if !has_agent_session {
                handoff_runtime.initial_history_ansi = runtime.handoff_history_ansi();
            }
            handoff_entries.push((terminal_id.clone(), handoff_runtime));
        }

        let panes = handoff_entries
            .iter()
            .map(|(_, runtime)| runtime.clone())
            .collect();
        let manifest = crate::server::handoff::manifest_for(
            snapshot,
            panes,
            params.expected_protocol,
            params.expected_version,
            self.api_window_title.clone(),
            self.host_shutdown_intent.get(),
        );
        let mut import_child = match crate::server::handoff::spawn_handoff_import(
            import_exe.as_deref(),
            &socket_path,
            &token,
        ) {
            Ok(child) => child,
            Err(err) => {
                self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                return Err(err);
            }
        };
        let child_pid = import_child.id();
        info!(pid = child_pid, socket = %socket_path.display(), "spawned handoff import server");

        let mut fds = Vec::new();
        let duplicate_result = (|| {
            for (terminal_id, _) in &handoff_entries {
                let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) else {
                    continue;
                };
                fds.push(runtime.duplicate_handoff_fd()?);
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(err) = duplicate_result {
            for fd in fds {
                let _ = unsafe { libc::close(fd) };
            }
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(err);
        }

        let mut stream = match crate::server::handoff::accept_and_validate_on(
            listener,
            &socket_path,
            &token,
            &manifest,
        ) {
            Ok(stream) => stream,
            Err(err) => {
                for fd in fds {
                    let _ = unsafe { libc::close(fd) };
                }
                crate::server::handoff::cleanup_failed_import_child(&mut import_child);
                self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                return Err(err);
            }
        };

        let send_result = crate::server::handoff::send_fds_and_wait_restored(&mut stream, &fds);
        for fd in fds {
            let _ = unsafe { libc::close(fd) };
        }
        if let Err(err) = send_result {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(err);
        }

        if let Some(api_server) = &self.api_server {
            let _ = api_server.remove_socket_file_if_owned();
        } else {
            let _ = std::fs::remove_file(crate::api::socket_path());
        }
        let _ = remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity);
        if let Err(err) = crate::server::handoff::wait_ready(&mut stream) {
            crate::server::handoff::cleanup_failed_import_child(&mut import_child);
            match self.wait_then_restore_public_sockets_after_failed_handoff() {
                Ok(()) => {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                }
                Err(restore_err) => {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    return Err(io::Error::other(format!(
                        "handoff replacement server did not become ready: {err}; old server could not restore public sockets: {restore_err}"
                    )));
                }
            }
            return Err(io::Error::other(format!(
                "handoff replacement server did not become ready: {err}"
            )));
        }
        let supervised = match &supervision {
            crate::server::systemd_unit::Supervision::Supervised(lead) => {
                if let Err(err) =
                    self.decide_supervised_handoff(lead, &mut stream, &mut import_child)
                {
                    match self.wait_then_restore_public_sockets_after_failed_handoff() {
                        Ok(()) => {
                            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                        }
                        Err(restore_err) => {
                            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                            return Err(io::Error::other(format!(
                                "{err}; old server could not restore public sockets: {restore_err}"
                            )));
                        }
                    }
                    return Err(err);
                }
                true
            }
            crate::server::systemd_unit::Supervision::Unsupervised => false,
        };
        if let Err(err) = crate::server::handoff::report_committed(&mut stream) {
            if supervised {
                // systemd already names the import server as main process: that
                // cannot be taken back, and reaping it would hide its end from
                // systemd. Exit as handed over (ADR-0018 crash table).
                warn!(err = %err, pid = import_child.id(), "systemd took the import server as main process, but the commit could not be sent; exiting without reaping it");
            } else {
                crate::server::handoff::cleanup_failed_import_child(&mut import_child);
                match self.wait_then_restore_public_sockets_after_failed_handoff() {
                    Ok(()) => {
                        self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    }
                    Err(restore_err) => {
                        self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                        return Err(io::Error::other(format!(
                            "handoff replacement server was ready, but commit failed: {err}; old server could not restore public sockets: {restore_err}"
                        )));
                    }
                }
                return Err(err);
            }
        }

        for (terminal_id, runtime) in self.app.terminal_runtimes.drain_for_handoff() {
            if !pane_by_terminal.contains_key(&terminal_id) {
                continue;
            }
            debug!(terminal = %terminal_id, "preserving pane runtime for handoff");
            runtime.preserve_for_handoff();
        }
        if let Some(reason) = self.server_stop.take_late_stop() {
            // ADR-0018 D2 step 5: the replacement owns the stop now. It is this
            // server's unreaped child, so its pid cannot have been reused.
            let pid = import_child.id();
            info!(%reason, pid, "a stop reached the old server during the handoff decision; forwarding it to the replacement");
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        crate::server::handoff::wait_owned_ack(&mut stream);

        Ok(())
    }

    /// ADR-0018 D2: the decision between `ready` and `committed`. `Ok` means
    /// commit: systemd names the import server as the unit's main process. An
    /// error means the handoff is off, the import server has been told, and the
    /// panes stay with this server.
    #[cfg(unix)]
    fn decide_supervised_handoff(
        &mut self,
        lead: &crate::server::systemd_unit::UnitLead,
        stream: &mut std::os::unix::net::UnixStream,
        import_child: &mut std::process::Child,
    ) -> io::Result<()> {
        use crate::server::systemd_unit::HandOver;

        if !self.server_stop.close_intake_for_handoff() {
            self.abort_import(stream, import_child, None, true);
            return Err(io::Error::other(
                "live handoff aborted: a stop arrived before the decision; the old server keeps the panes and stops",
            ));
        }
        let import_pid = import_child.id();
        let (rollback, main_is_self, reason) = match lead.hand_main_pid_to(import_pid) {
            HandOver::Accepted => {
                info!(unit = %lead.describe(), pid = import_pid, "systemd took the import server as main process; committing the handoff");
                return Ok(());
            }
            HandOver::Refused => (
                None,
                true,
                format!(
                    "systemd did not take the import server as the main process of {}",
                    lead.describe()
                ),
            ),
            HandOver::Unknown(why) => (
                Some(std::process::id()),
                false,
                format!(
                    "systemd's answer about the main process of {} is unknown ({why}); rolled back through the import server",
                    lead.describe()
                ),
            ),
        };
        self.abort_import(stream, import_child, rollback, main_is_self);
        Err(io::Error::other(format!(
            "live handoff aborted: {reason}; the old server keeps the panes"
        )))
    }

    /// Tells the import server the handoff is off, takes over a stop it
    /// received, and reaps it only once this server is known to be the unit's
    /// main process again (a reap would hide the import server's end from
    /// systemd if systemd still named it).
    #[cfg(unix)]
    fn abort_import(
        &mut self,
        stream: &mut std::os::unix::net::UnixStream,
        import_child: &mut std::process::Child,
        rollback_main_pid: Option<u32>,
        main_is_self: bool,
    ) {
        let reply = crate::server::handoff::send_abort(stream, rollback_main_pid)
            .and_then(|()| crate::server::handoff::wait_aborted(stream));
        self.server_stop.reopen_intake();
        let main_confirmed = match &reply {
            Ok(reply) => {
                if reply.stop_requested {
                    self.server_stop.request(ShutdownReason::ApiStop {
                        caller: Some("a stop received by the handoff import server".into()),
                    });
                }
                main_is_self || reply.rollback_done
            }
            Err(err) => {
                warn!(err = %err, "the handoff import server did not answer the abort");
                main_is_self
            }
        };
        if main_confirmed {
            crate::server::handoff::cleanup_failed_import_child(import_child);
        } else {
            warn!(
                pid = import_child.id(),
                "systemd may still name the import server as main process; leaving it unreaped so systemd sees its real end"
            );
        }
    }

    pub(super) fn finish_live_handoff_shutdown(&mut self) {
        self.shutting_down = true;
        self.app.state.should_quit = true;
        self.app.policy.persist_session = false;
        info!("live handoff completed; old server exiting");
    }

    #[cfg(not(unix))]
    pub(super) fn perform_live_handoff(
        &mut self,
        _params: crate::api::schema::ServerLiveHandoffParams,
    ) -> io::Result<()> {
        Err(io::Error::other("live handoff is only supported on Unix"))
    }

    #[cfg(unix)]
    fn restore_public_sockets_after_failed_handoff(&mut self) -> io::Result<()> {
        let api_tx = self
            .api_tx
            .clone()
            .ok_or_else(|| io::Error::other("cannot restore api socket without api sender"))?;
        let api_server = api::start_server_with_stop_control(
            api_tx,
            self.app.event_hub.clone(),
            self.server_stop.clone(),
        )?;

        let client_path = client_socket_path();
        prepare_socket_path(&client_path)?;
        let listener = bind_local_listener(&client_path)?;
        restrict_socket_permissions(&client_path)?;
        let client_socket_identity = socket_file_identity(&client_path)?;
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;

        self.api_server = Some(api_server);
        self.client_listener = listener;
        self.client_socket_path = client_path;
        self.client_socket_identity = client_socket_identity;
        Ok(())
    }

    #[cfg(unix)]
    fn wait_then_restore_public_sockets_after_failed_handoff(&mut self) -> io::Result<()> {
        let timeout = crate::server::handoff::COMMIT_TIMEOUT + Duration::from_secs(2);
        wait_for_old_public_sockets_to_close(timeout)?;
        self.restore_public_sockets_after_failed_handoff()
    }

    #[cfg(unix)]
    fn rollback_handoff_before_commit(
        &mut self,
        socket_path: &Path,
        paused_terminal_ids: &[crate::terminal::TerminalId],
    ) {
        for terminal_id in paused_terminal_ids {
            if let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) {
                runtime.set_handoff_reader_paused(false);
            }
        }
        self.handoff_in_progress = false;
        let _ = std::fs::remove_file(socket_path);
    }

    #[cfg(unix)]
    pub(super) fn nudge_handoff_panes_on_first_client_attach(&mut self) {
        if !self.pending_handoff_repaint_nudge {
            return;
        }
        self.pending_handoff_repaint_nudge = false;
        self.app
            .terminal_runtimes
            .nudge_child_redraw_after_handoff();
    }

    #[cfg(not(unix))]
    pub(super) fn nudge_handoff_panes_on_first_client_attach(&mut self) {}
    /// Initiates graceful shutdown.
    pub(super) fn initiate_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        let reason = if self.host_shutdown_requested.load(Ordering::Acquire) {
            ShutdownReason::HostShutdown
        } else {
            self.server_stop
                .take_reason()
                .unwrap_or(ShutdownReason::Unknown)
        };
        info!(%reason, "server shutdown initiated");
        self.shutting_down = true;

        // Clear client-local host graphics, then send ServerShutdown to all connected clients.
        let shutdown_msg = ServerMessage::ServerShutdown {
            reason: Some("server is shutting down".to_owned()),
        };
        self.send_to_all_clients(shutdown_msg);

        // Give client writer threads a moment to flush the shutdown message.
        // A short sleep ensures the message is written to the socket before
        // we close the connections.
        std::thread::sleep(Duration::from_millis(50));

        // Signal the main loop to exit.
        self.should_quit.store(true, Ordering::Release);
        self.app.state.should_quit = true;
    }

    /// Completes the shutdown sequence: send ServerShutdown to clients,
    /// close client connections, remove socket files, and clean up.
    pub(super) async fn complete_shutdown(&mut self) -> io::Result<()> {
        info!("completing server shutdown");
        self.reject_late_client_connections().await;

        // herdr-whd2 (ADR-0018 D4): `herdr server stop` returns once the sockets
        // stop answering, and after a live handoff systemd kills the main process
        // right after that, so the final save is written before any socket goes.
        if self.app.policy.persist_session {
            self.app.save_session_on_shutdown();
            self.app.policy.persist_session = false;
        }

        // Send ServerShutdown to all remaining clients.
        if !self.clients.is_empty() {
            let shutdown_msg = ServerMessage::ServerShutdown {
                reason: Some("server is shutting down".to_owned()),
            };
            self.send_to_all_clients(shutdown_msg);

            // Give writer threads a moment to flush before closing.
            std::thread::sleep(Duration::from_millis(50));
        }

        // Reject only the requests already queued when shutdown reached cleanup.
        self.reject_queued_api_requests_for_shutdown();

        // The loop handles no more requests. Stop admitting API connections and
        // let every accepted one finish with its reply or an explicit error.
        if let Some(api_server) = &self.api_server {
            api_server.drain_for_shutdown();
        }

        // Close all client connections.
        let staged_files = self
            .clients
            .drain()
            .flat_map(|(_, client)| client.staged_clipboard_files)
            .collect::<Vec<_>>();
        crate::server::clipboard_image::remove_files(staged_files);

        // Remove socket files.
        self.cleanup_sockets()?;

        Ok(())
    }

    /// Removes socket files created by the server.
    pub(super) fn cleanup_sockets(&self) -> io::Result<()> {
        if let Err(err) =
            remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity)
        {
            if err.kind() != io::ErrorKind::NotFound {
                warn!(
                    path = %self.client_socket_path.display(),
                    err = %err,
                    "failed to remove client socket on shutdown"
                );
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn wait_for_old_public_sockets_to_close(timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let api_socket = api::socket_path();
    let client_socket = client_socket_path();
    while Instant::now() < deadline {
        let api_open = api_socket.exists() && crate::ipc::connect_local_stream(&api_socket).is_ok();
        let client_open =
            client_socket.exists() && crate::ipc::connect_local_stream(&client_socket).is_ok();
        if !api_open && !client_open {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "old server sockets did not close before handoff import bind",
    ))
}
