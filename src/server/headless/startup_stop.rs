//! A stop signal received while the server is still starting (herdr-ir4j).
//!
//! The API socket accepts connections before the event loop runs: startup
//! still builds the app (which can wait on the plugin registry lock), binds the
//! client socket, and on a live handoff waits for the source to commit. A
//! SIGTERM or SIGINT in that window must not wait for any of it. From the
//! signal on:
//!
//! - the stop thread drains the API server at once, by the drain's own bound
//!   counted from the signal: admission closes and every accepted connection
//!   gets its reply or `server_unavailable`, including a request already
//!   handed to the app and a client that never finished its request;
//! - startup is then given until a bound to reach the event loop, which takes
//!   the stop over and ends the process the ordinary way (session saved,
//!   sockets removed, exit status 0);
//! - a startup that is still not there at the bound is abandoned: the stop
//!   thread exits the process with status 0. Whatever that step was doing ends
//!   with the process (a lock wait releases nothing it did not take; panes a
//!   half-built app spawned close with it; the session file on disk stays the
//!   one startup loaded). It is not joined: it may never finish.
//!
//! The bound is the drain's for an ordinary start. A handoff import gives its
//! commitment wait the time that wait has anyway: leaving after `ready` can
//! strand every pane when the source has already committed, so that step is
//! joined, within its own timeout, never abandoned.
//!
//! A stop requested through the API is not a signal and is unchanged: the
//! event loop takes it once it runs.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::api::{ApiShutdownControl, API_SHUTDOWN_DRAIN_TIMEOUT};
use crate::server::client_transport::ServerEvent;

/// Where a stop signal goes: to startup until the event loop takes over, to
/// the event loop from then on.
#[derive(Clone, Default)]
pub(crate) struct StartupStop {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// When the first stop signal arrived before the event loop took over.
    signalled_at: Option<Instant>,
    /// Set once the event loop runs; from then on a signal wakes it.
    event_loop: Option<mpsc::Sender<ServerEvent>>,
    /// The startup drain of the API server finished.
    drained: bool,
}

impl StartupStop {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Plain flags and a sender; a panicking holder cannot leave them torn.
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Called by the signal monitor after the stop flag is set.
    pub(crate) fn signalled(&self) {
        let mut state = self.lock();
        if let Some(event_loop) = &state.event_loop {
            let _ = event_loop.try_send(ServerEvent::QuitSignal);
        } else if state.signalled_at.is_none() {
            state.signalled_at = Some(Instant::now());
            self.shared.changed.notify_all();
        }
    }

    /// The event loop takes every stop over. A signal received before this
    /// already set the stop flag, which the loop's first pass reads; a stop
    /// startup was draining continues there, by the same deadline.
    pub(crate) fn hand_over_to_event_loop(&self, event_loop: mpsc::Sender<ServerEvent>) {
        let mut state = self.lock();
        state.event_loop = Some(event_loop);
        self.shared.changed.notify_all();
    }

    /// Starts the thread that takes a stop signal received before the event
    /// loop runs. Call right after the API socket is bound. `startup_bound`
    /// is how long after the signal startup may still reach the event loop.
    pub(crate) fn watch(&self, api: ApiShutdownControl, startup_bound: Duration) {
        let stop = self.clone();
        let spawned = crate::thread_spawn::spawn_named("herdr-startup-stop", move || {
            stop.take_startup_stop(&api, startup_bound);
        });
        if let Err(err) = spawned {
            // The stop then waits for the event loop, as before herdr-ir4j.
            warn!(%err, "failed to spawn the startup stop thread");
        }
    }

    fn take_startup_stop(&self, api: &ApiShutdownControl, startup_bound: Duration) {
        let signalled_at = {
            let mut state = self.lock();
            loop {
                if state.event_loop.is_some() {
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
            }
        };

        info!("stop signal received during server startup; draining the api server");
        api.drain_for_shutdown_by(signalled_at + API_SHUTDOWN_DRAIN_TIMEOUT);

        let abandon_at = signalled_at + startup_bound;
        let mut state = self.lock();
        state.drained = true;
        self.shared.changed.notify_all();
        while state.event_loop.is_none() {
            let remaining = abandon_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if state.event_loop.is_some() {
            return;
        }
        // The lock stays held: the event loop cannot take over while the
        // process leaves.
        warn!(
            bound_ms = startup_bound.as_millis() as u64,
            "server startup did not finish after the stop signal; exiting without it"
        );
        crate::logging::shutdown("server");
        std::process::exit(0);
    }

    /// Startup ended without reaching the event loop (an error). A stop
    /// signal received before that is still draining the API server on its
    /// own thread; wait for it, within its bound, so the process does not
    /// leave accepted connections behind on the way out.
    pub(crate) fn settle_after_failed_startup(&self) {
        let mut state = self.lock();
        let Some(signalled_at) = state.signalled_at else {
            return;
        };
        let wait_until = signalled_at + API_SHUTDOWN_DRAIN_TIMEOUT;
        while state.event_loop.is_none() && !state.drained {
            let remaining = wait_until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signal_after_the_hand_over_wakes_the_event_loop() {
        let stop = StartupStop::default();
        let (tx, mut rx) = mpsc::channel(4);
        stop.hand_over_to_event_loop(tx);

        stop.signalled();

        assert!(matches!(rx.try_recv(), Ok(ServerEvent::QuitSignal)));
        assert!(stop.lock().signalled_at.is_none());
    }

    #[test]
    fn a_signal_before_the_hand_over_is_kept_for_startup() {
        let stop = StartupStop::default();
        stop.signalled();
        let first = stop.lock().signalled_at;
        assert!(first.is_some());

        stop.signalled();
        assert_eq!(stop.lock().signalled_at, first, "the first signal counts");
    }

    #[test]
    fn settling_without_a_signal_returns_at_once() {
        let stop = StartupStop::default();
        let started = Instant::now();
        stop.settle_after_failed_startup();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
