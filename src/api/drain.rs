//! Shutdown drain for the JSON API socket.
//!
//! A connection the accept loop took must get a reply, or an explicit
//! `server_unavailable` error, before the process may exit. Three facts are
//! tracked here and nowhere else:
//!
//! - every accepted connection is counted from the accept thread, before its
//!   worker thread exists, until its handler returned and its reply is written;
//! - shutdown is announced once (`begin_shutdown`); from then on connection
//!   threads stop waiting for the app and answer for themselves;
//! - the accept loop acknowledges that it stopped admitting, after it removed
//!   the public name, emptied the listen backlog and closed the listener.
//!
//! Shutdown waits on those facts through a condition variable. It never
//! samples a counter and never infers quiescence from quiet polls.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

#[derive(Default)]
struct DrainState {
    admission_closed: bool,
    next_connection: u64,
    /// Connection id to the stage it is in, for the abandonment log.
    in_flight: BTreeMap<u64, &'static str>,
}

#[derive(Default)]
pub(crate) struct ApiDrain {
    state: Mutex<DrainState>,
    changed: Condvar,
    draining: AtomicBool,
}

impl ApiDrain {
    fn lock(&self) -> MutexGuard<'_, DrainState> {
        // The state is plain counters; a panicking holder cannot leave it torn.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one accepted connection. Called on the accept thread so that a
    /// connection is in flight before anything can observe the count.
    pub(crate) fn admit(self: &Arc<Self>) -> InFlight {
        let mut state = self.lock();
        let id = state.next_connection;
        state.next_connection = state.next_connection.wrapping_add(1);
        state.in_flight.insert(id, "reading request");
        InFlight {
            drain: Arc::clone(self),
            id,
        }
    }

    /// Announces shutdown. The caller must already have stopped handling
    /// requests: after this, a connection thread whose response channel is
    /// empty answers `server_unavailable` itself.
    pub(crate) fn begin_shutdown(&self) {
        let _state = self.lock();
        self.draining.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// The accept loop stopped admitting and its listener is closed.
    pub(crate) fn acknowledge_admission_closed(&self) {
        let mut state = self.lock();
        state.admission_closed = true;
        self.changed.notify_all();
    }

    pub(crate) fn admission_closed(&self) -> bool {
        self.lock().admission_closed
    }

    /// Waits for the accept loop's acknowledgement. `false` means the deadline
    /// passed first.
    pub(crate) fn wait_admission_closed(&self, deadline: Instant) -> bool {
        let mut state = self.lock();
        while !state.admission_closed {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            if remaining.is_zero() {
                return false;
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Waits until no accepted connection is in flight. On deadline expiry it
    /// returns the stage of every connection that is being abandoned.
    pub(crate) fn wait_idle(&self, deadline: Instant) -> Result<(), Vec<&'static str>> {
        let mut state = self.lock();
        while !state.in_flight.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero());
            let Some(remaining) = remaining else {
                return Err(state.in_flight.values().copied().collect());
            };
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn in_flight_stages(&self) -> Vec<&'static str> {
        self.lock().in_flight.values().copied().collect()
    }
}

/// One accepted connection. Dropping it tells shutdown the connection is done.
pub(crate) struct InFlight {
    drain: Arc<ApiDrain>,
    id: u64,
}

impl InFlight {
    pub(crate) fn drain(&self) -> &Arc<ApiDrain> {
        &self.drain
    }

    pub(crate) fn set_stage(&self, stage: &'static str) {
        if let Some(current) = self.drain.lock().in_flight.get_mut(&self.id) {
            *current = stage;
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut state = self.drain.lock();
        state.in_flight.remove(&self.id);
        self.drain.changed.notify_all();
    }
}

thread_local! {
    /// The drain of the connection this thread is serving. Every dispatch a
    /// connection thread makes, including the ones nested in wait handlers,
    /// reads it instead of taking the drain as a parameter.
    static CONNECTION_DRAIN: RefCell<Option<Arc<ApiDrain>>> = const { RefCell::new(None) };
}

/// Marks the current thread as serving a connection of `drain` until dropped.
pub(crate) struct ConnectionScope {
    previous: Option<Arc<ApiDrain>>,
}

pub(crate) fn enter_connection(drain: Arc<ApiDrain>) -> ConnectionScope {
    let previous = CONNECTION_DRAIN.with(|current| current.borrow_mut().replace(drain));
    ConnectionScope { previous }
}

impl Drop for ConnectionScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CONNECTION_DRAIN.with(|current| *current.borrow_mut() = previous);
    }
}

/// The drain of the connection this thread serves, if it serves one.
pub(crate) fn current_connection_drain() -> Option<Arc<ApiDrain>> {
    CONNECTION_DRAIN.with(|current| current.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn idle_wait_reports_the_stage_of_every_abandoned_connection() {
        let drain = Arc::new(ApiDrain::default());
        let first = drain.admit();
        let second = drain.admit();
        second.set_stage("workspace.list");

        assert_eq!(
            drain.wait_idle(Instant::now()),
            Err(vec!["reading request", "workspace.list"])
        );

        drop(first);
        drop(second);
        assert_eq!(drain.wait_idle(Instant::now()), Ok(()));
    }

    #[test]
    fn idle_wait_wakes_when_the_last_connection_finishes() {
        let drain = Arc::new(ApiDrain::default());
        let in_flight = drain.admit();
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let waiter = {
            let drain = Arc::clone(&drain);
            std::thread::spawn(move || {
                waiting_tx.send(()).unwrap();
                drain.wait_idle(Instant::now() + Duration::from_secs(30))
            })
        };

        waiting_rx.recv().unwrap();
        drop(in_flight);

        assert_eq!(waiter.join().unwrap(), Ok(()));
    }

    #[test]
    fn admission_wait_needs_the_accept_loop_acknowledgement() {
        let drain = Arc::new(ApiDrain::default());
        drain.begin_shutdown();
        assert!(drain.is_draining());
        assert!(!drain.wait_admission_closed(Instant::now()));

        drain.acknowledge_admission_closed();
        assert!(drain.wait_admission_closed(Instant::now()));
    }

    #[test]
    fn connection_scope_restores_the_previous_drain() {
        assert!(current_connection_drain().is_none());
        let outer = Arc::new(ApiDrain::default());
        let inner = Arc::new(ApiDrain::default());
        {
            let _outer = enter_connection(Arc::clone(&outer));
            {
                let _inner = enter_connection(Arc::clone(&inner));
                assert!(Arc::ptr_eq(&current_connection_drain().unwrap(), &inner));
            }
            assert!(Arc::ptr_eq(&current_connection_drain().unwrap(), &outer));
        }
        assert!(current_connection_drain().is_none());
    }
}
