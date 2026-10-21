//! Who owns a stop while the server is still starting (herdr-ir4j, ADR-0014).
//!
//! The API socket accepts connections long before the event loop runs:
//! startup still builds the app (which can wait on the plugin registry lock),
//! binds the client socket, and on a live handoff waits for the source to
//! commit. `StartupStop` owns every stop signal from the moment the handler
//! exists until the event loop takes it over. It is a state machine under one
//! mutex with its own thread; each startup step that changes what a stop may
//! do moves it to the next phase under that mutex:
//!
//! - `Booting` (nothing accepted, nothing owned, including the API socket
//!   probe): a stop exits the process at once;
//! - `Binding` / `Serving` (the API socket may hold accepted connections): a
//!   stop drains the API server at once, by the drain bound counted from the
//!   signal, and exits the process at that deadline unless the event loop
//!   took over; a startup step still blocked then is abandoned, not joined;
//! - `Committing` (handoff: `ready` written): the import cannot leave, the
//!   source may already have committed; the API is drained at once and the
//!   commitment wait, which has its own bound, is joined;
//! - `Saving` (handoff: `committed` read): the transferred session is being
//!   written; the stop waits for it, bounded;
//! - `Failed` (a startup error after the bind): the failing thread drains and
//!   leaves; the stop thread keeps its deadline;
//! - `EventLoop`: a signal wakes the loop, which ends the process the
//!   ordinary way.
//!
//! The thread exists before the handler is installed. When the OS refuses it,
//! the server does not start: no connection is ever accepted without
//! something acting on a stop.

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
#[cfg(unix)]
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc;
#[cfg(unix)]
use tracing::error;
use tracing::{info, warn};

use crate::api::{ApiShutdownControl, API_SHUTDOWN_DRAIN_TIMEOUT};
use crate::server::client_transport::ServerEvent;

/// How long after reading `committed` a stop waits for the handoff import to
/// write the transferred session before it gives the process up.
#[cfg(unix)]
pub(crate) const HANDOFF_SAVE_BOUND: Duration = Duration::from_secs(10);

/// Makes [`StartupStop::start`] fail as if the OS refused the thread.
const REFUSE_THREAD_FOR_TEST_ENV: &str = "HERDR_TEST_REFUSE_STARTUP_STOP_THREAD";

/// Where a stop signal goes: to the startup stop thread until the event loop
/// takes over, to the event loop from then on.
#[derive(Clone)]
pub(crate) struct StartupStop {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

enum Phase {
    Booting,
    Binding,
    Serving,
    #[cfg(unix)]
    Committing {
        until: Instant,
    },
    #[cfg(unix)]
    Saving {
        until: Instant,
    },
    Failed,
    EventLoop(mpsc::Sender<ServerEvent>),
}

struct State {
    phase: Phase,
    /// When the first stop signal arrived before the event loop took over.
    signalled_at: Option<Instant>,
    api: Option<ApiShutdownControl>,
}

impl StartupStop {
    /// Starts the stop thread. Call before the signal handler is installed;
    /// an error means the server must not start.
    pub(crate) fn start() -> io::Result<Self> {
        let stop = Self::unstarted();
        if std::env::var_os(REFUSE_THREAD_FOR_TEST_ENV).is_some() {
            return Err(io::Error::other(
                "startup stop thread refused by a test hook",
            ));
        }
        let thread_stop = stop.clone();
        crate::thread_spawn::spawn_named("herdr-startup-stop", move || {
            thread_stop.take_startup_stop();
        })?;
        Ok(stop)
    }

    /// A stop without its thread, for an event loop started by a test.
    pub(crate) fn unstarted() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    phase: Phase::Booting,
                    signalled_at: None,
                    api: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Plain values; a panicking holder cannot leave them torn.
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn set_phase(&self, phase: Phase) {
        let mut state = self.lock();
        if matches!(state.phase, Phase::EventLoop(_) | Phase::Failed) {
            return;
        }
        state.phase = phase;
        self.shared.changed.notify_all();
    }

    /// Called by the signal monitor after the stop flag is set.
    pub(crate) fn signalled(&self) {
        let mut state = self.lock();
        if let Phase::EventLoop(event_loop) = &state.phase {
            let _ = event_loop.try_send(ServerEvent::QuitSignal);
        } else if state.signalled_at.is_none() {
            state.signalled_at = Some(Instant::now());
            self.shared.changed.notify_all();
        }
    }

    /// The API socket path was probed; the bind comes next, after which the
    /// socket can hold accepted connections.
    pub(crate) fn api_socket_probed(&self) {
        self.set_phase(Phase::Binding);
    }

    /// The API server is listening; a stop from here on drains it.
    pub(crate) fn api_bound(&self, api: ApiShutdownControl) {
        let mut state = self.lock();
        state.api = Some(api);
        if matches!(state.phase, Phase::Booting | Phase::Binding) {
            state.phase = Phase::Serving;
        }
        self.shared.changed.notify_all();
    }

    /// Asked before a handoff import writes `ready`. False when a stop was
    /// already recorded: the import must then not report ready, so its source
    /// keeps ownership. Otherwise a stop from here on waits for the
    /// commitment wait, which ends by `until`.
    #[cfg(unix)]
    pub(crate) fn begin_commit_wait(&self, until: Instant) -> bool {
        let mut state = self.lock();
        if state.signalled_at.is_some() {
            return false;
        }
        state.phase = Phase::Committing { until };
        self.shared.changed.notify_all();
        true
    }

    /// The source committed: the import owns the panes, and the session it
    /// carries is written next.
    #[cfg(unix)]
    pub(crate) fn committed(&self) {
        self.set_phase(Phase::Saving {
            until: Instant::now() + HANDOFF_SAVE_BOUND,
        });
    }

    /// The transferred session is on disk; a stop may give startup up again.
    #[cfg(unix)]
    pub(crate) fn saved(&self) {
        self.set_phase(Phase::Serving);
    }

    /// Startup failed after the API bind. Drains the API server, by the
    /// deadline of a stop already recorded or else by the drain bound from
    /// now, so every accepted connection is answered before the caller drops
    /// the server handle and leaves.
    pub(crate) fn fail_startup(&self) {
        let (api, deadline) = {
            let mut state = self.lock();
            state.phase = Phase::Failed;
            self.shared.changed.notify_all();
            let deadline =
                state.signalled_at.unwrap_or_else(Instant::now) + API_SHUTDOWN_DRAIN_TIMEOUT;
            (state.api.clone(), deadline)
        };
        if let Some(api) = api {
            api.drain_for_shutdown_by(deadline);
        }
    }

    /// The event loop takes every stop over. A signal received before this
    /// already set the stop flag, which the loop's first pass reads; a stop
    /// the thread was draining continues there, by the same deadline.
    pub(crate) fn hand_over_to_event_loop(&self, event_loop: mpsc::Sender<ServerEvent>) {
        let mut state = self.lock();
        state.phase = Phase::EventLoop(event_loop);
        self.shared.changed.notify_all();
    }

    fn wait_until<'a>(
        &'a self,
        state: MutexGuard<'a, State>,
        until: Instant,
    ) -> MutexGuard<'a, State> {
        let remaining = until.saturating_duration_since(Instant::now());
        self.shared
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    fn take_startup_stop(&self) {
        let mut state = self.lock();
        let signalled_at = loop {
            if matches!(state.phase, Phase::EventLoop(_)) {
                return;
            }
            if let Some(signalled_at) = state.signalled_at {
                break signalled_at;
            }
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        };
        let drain_deadline = signalled_at + API_SHUTDOWN_DRAIN_TIMEOUT;

        if matches!(state.phase, Phase::Booting) {
            // The lock stays held: startup cannot move on to the bind while
            // the process leaves.
            info!("stop signal received before the api socket was bound; exiting");
            crate::logging::shutdown("server");
            std::process::exit(0);
        }
        while matches!(state.phase, Phase::Binding) && Instant::now() < drain_deadline {
            state = self.wait_until(state, drain_deadline);
        }
        if matches!(state.phase, Phase::EventLoop(_)) {
            return;
        }
        let api = state.api.clone();
        drop(state);

        if let Some(api) = &api {
            info!("stop signal received during server startup; draining the api server");
            api.drain_for_shutdown_by(drain_deadline);
        }

        let mut state = self.lock();
        loop {
            let until = match &state.phase {
                Phase::EventLoop(_) => return,
                #[cfg(unix)]
                Phase::Committing { until } | Phase::Saving { until } => *until,
                Phase::Booting | Phase::Binding | Phase::Serving | Phase::Failed => drain_deadline,
            };
            if Instant::now() >= until {
                break;
            }
            state = self.wait_until(state, until);
        }

        // The lock stays held: the event loop cannot take over while the
        // process leaves.
        let status = match &state.phase {
            #[cfg(unix)]
            Phase::Committing { .. } => {
                warn!("handoff commitment wait did not end after the stop signal; exiting");
                0
            }
            #[cfg(unix)]
            Phase::Saving { .. } => {
                error!(
                    bound_ms = HANDOFF_SAVE_BOUND.as_millis() as u64,
                    "handoff session save did not finish after the stop signal; exiting without it"
                );
                1
            }
            Phase::Failed => {
                warn!("failed server startup did not end after the stop signal; exiting");
                1
            }
            _ => {
                warn!(
                    bound_ms = API_SHUTDOWN_DRAIN_TIMEOUT.as_millis() as u64,
                    "server startup did not finish after the stop signal; exiting without it"
                );
                0
            }
        };
        if let Some(api) = &state.api {
            // The drain already unlinked it; this covers a drain that could
            // not run.
            let _ = api.remove_socket_file_if_owned();
        }
        crate::logging::shutdown("server");
        std::process::exit(status);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn signalled_at(stop: &StartupStop) -> Option<Instant> {
        stop.lock().signalled_at
    }

    #[test]
    fn a_signal_after_the_hand_over_wakes_the_event_loop() {
        let stop = StartupStop::unstarted();
        let (tx, mut rx) = mpsc::channel(4);
        stop.hand_over_to_event_loop(tx);

        stop.signalled();

        assert!(matches!(rx.try_recv(), Ok(ServerEvent::QuitSignal)));
        assert!(signalled_at(&stop).is_none());
    }

    #[test]
    fn a_signal_before_the_hand_over_is_kept_and_the_first_counts() {
        let stop = StartupStop::unstarted();
        stop.signalled();
        let first = signalled_at(&stop);
        assert!(first.is_some());

        stop.signalled();
        assert_eq!(signalled_at(&stop), first);
    }

    #[cfg(unix)]
    #[test]
    fn a_stop_recorded_before_ready_refuses_the_commit_wait() {
        let stop = StartupStop::unstarted();
        assert!(stop.begin_commit_wait(Instant::now() + Duration::from_secs(1)));

        let stopped = StartupStop::unstarted();
        stopped.signalled();
        assert!(!stopped.begin_commit_wait(Instant::now() + Duration::from_secs(1)));
    }

    #[test]
    fn a_failed_startup_is_not_moved_back_by_a_later_phase() {
        let stop = StartupStop::unstarted();
        stop.fail_startup();
        stop.api_socket_probed();
        assert!(matches!(stop.lock().phase, Phase::Failed));
    }

    #[test]
    fn failing_without_an_api_server_returns_at_once() {
        let stop = StartupStop::unstarted();
        stop.signalled();
        let started = Instant::now();
        stop.fail_startup();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
