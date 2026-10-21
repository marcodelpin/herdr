"""Apply one named mutant to a herdr checkout. Each replacement must match exactly once."""
import sys

LIFECYCLE = "src/server/headless/lifecycle.rs"
DRAIN = "src/api/drain.rs"
SERVER = "src/api/server.rs"

MUTANTS = {
    # Shutdown goes back to one reject of the queued requests: no drain.
    "M1-drain-removed": [
        (LIFECYCLE, "api_server.drain_for_shutdown();", "let _ = api_server;"),
    ],
    # The drain stays but no connection counts as in flight.
    "M2-accounting-disabled": [
        (DRAIN, 'state.in_flight.insert(id, "reading request");', ""),
    ],
    # Finding 1: agent.prompt is excluded from the drain again.
    "F1-prompt-not-counted": [
        (
            SERVER,
            "        Method::AgentPrompt(params) => {\n            let response = prompt_agent(",
            "        Method::AgentPrompt(params) => {\n            drop(in_flight);\n            let response = prompt_agent(",
        ),
    ],
    # Finding 2: responders the app never answers are not resolved.
    "F2-held-responders-unresolved": [
        (SERVER, "Err(_) => return shutting_down_response(request_id),", "Err(_) => {}"),
    ],
    # Finding 3: admission closes without serving what was queued.
    "F3-backlog-not-served": [
        (SERVER, "    take_pending_local_connections(listener)\n}", "    Vec::new()\n}"),
    ],
    # Finding 4: admission is acknowledged while the listener lives on.
    "F4-admission-stays-open": [
        (SERVER, "    drop(listener);\n    drain.acknowledge_admission_closed();",
         "    std::mem::forget(listener);\n    drain.acknowledge_admission_closed();"),
        (SERVER, "    refuse_new_local_connections(listener);\n", ""),
        (SERVER, "if let Err(err) = remove_socket_file_if_owned(path, identity) {\n        if err.kind() != io::ErrorKind::NotFound {\n            warn!(path = %path.display(), err = %err, \"failed to remove api socket on shutdown\");",
         "if let Err(err) = Ok::<(), io::Error>(()) {\n        if err.kind() != io::ErrorKind::NotFound {\n            warn!(path = %path.display(), err = %err, \"failed to remove api socket on shutdown\");"),
    ],
    # Finding 6: a client still sending its request gets no explicit error.
    "F6-slow-client-dropped": [
        (SERVER, "if current_connection_drain().is_some_and(|drain| drain.is_draining()) {", "if false {"),
    ],
    # Finding 6: the drain gives up at once and abandons what is in flight.
    "F6b-drain-deadline-zero": [
        (SERVER, "STREAM_WRITE_TIMEOUT.saturating_add(Duration::from_secs(2));", "Duration::ZERO;"),
    ],
    # Finding 5 (r2b): the bounded wait for the Windows reply flush waits forever.
    "B-flush-bound-ignored": [
        ("src/ipc.rs", "match done_rx.recv_timeout(bound) {",
         "match done_rx.recv().map_err(|_| RecvTimeoutError::Disconnected) {"),
    ],
    # A wait ended by shutdown closes without a reply again.
    "W-wait-not-told": [
        (SERVER, "None if !running.load(Ordering::Relaxed) =>", "None if false =>"),
    ],
}

MUTANTS.update({
    # r3 F1: running is cleared before draining is announced again.
    "R3-F1-flag-order": [
        (SERVER, "    drain.begin_shutdown();\n    running.store(false, Ordering::Release);",
         "    running.store(false, Ordering::Release);\n    drain.begin_shutdown();"),
    ],
    # r3 F4: events.subscribe stops counting before its first reply.
    "R3-F4-subscribe-guard-dropped": [
        (SERVER, "            let result = stream_subscriptions(\n",
         "            drop(in_flight);\n            let in_flight = Arc::new(ApiDrain::default()).admit();\n            let result = stream_subscriptions(\n"),
    ],
    # r3 F5: back to a per-operation send timeout on a blocking write.
    "R3-F5-per-operation-timeout": [
        (SERVER, "    stream.set_nonblocking(true)?;\n    let result = write_all_nonblocking_by(stream, bytes, deadline);\n    let restored = stream.set_nonblocking(false);\n    result.and(restored)",
         "    let _ = write_all_nonblocking_by;\n    stream.set_send_timeout(Some(deadline.saturating_duration_since(Instant::now())))?;\n    stream.write_all(bytes)"),
    ],
    # r3 F7: every reply confirms delivery again, also in normal operation.
    "R3-F7-always-confirm": [
        (SERVER, "fn reply_needs_confirmation() -> bool {\n    current_connection_drain().is_some_and(|drain| drain.is_draining())",
         "fn reply_needs_confirmation() -> bool {\n    true"),
    ],
    # r3 F6: a confirmation that could not start counts as delivered.
    "R3-F6-unstarted-released": [
        (SERVER, "            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));\n",
         "            let _ = deadline;\n"),
    ],
    # r3 F6: a helper spawn failure is not recognised as unstarted.
    "R3-F6b-spawn-failure-unmarked": [
        ("src/ipc.rs", "    })\n    .map_err(confirmation_not_started)?;", "    })?;"),
    ],
})

BOOTSTRAP = "src/server/headless/bootstrap.rs"
INSTALL_EARLY = """    let quit_signal = {
        let _runtime = rt.enter();
        install_server_signal_handling(&server_stop)
    };
"""

MUTANTS.update({
    # r4 herdr-ir4j: signal handling installed after the API bind and the
    # startup work that follows it (here: after the test hold).
    "IR4J-install-after-bind": [
        (BOOTSTRAP, INSTALL_EARLY, ""),
        (BOOTSTRAP, "    hold_startup_after_api_bind_for_test();\n",
         "    hold_startup_after_api_bind_for_test();\n" + INSTALL_EARLY),
    ],
    # r4 herdr-ir4j: back where 0.9.5 had it, right before the event loop.
    "IR4J-install-in-run": [
        (BOOTSTRAP, INSTALL_EARLY, ""),
        (BOOTSTRAP, "        server.app.run_plugin_startup_hooks();\n\n        server.run(quit_signal).await",
         "        server.app.run_plugin_startup_hooks();\n        let quit_signal = install_server_signal_handling(&server.server_stop);\n        server.run(quit_signal).await"),
    ],
})


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--list":
        print("\n".join(MUTANTS))
        return 0
    root, name = sys.argv[1], sys.argv[2]
    for path, old, new in MUTANTS[name]:
        full = f"{root}/{path}"
        text = open(full, encoding="utf-8").read()
        if text.count(old) != 1:
            print(f"MUTANT-NOT-APPLIED {name}: {text.count(old)} matches in {path} for {old[:50]!r}")
            return 2
        open(full, "w", encoding="utf-8").write(text.replace(old, new))
    return 0


if __name__ == "__main__":
    sys.exit(main())
