//! Why the server is stopping, recorded where the request arrives and logged
//! once when shutdown begins.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::platform::ServerQuitSignal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShutdownReason {
    Signal(ServerQuitSignal),
    ApiStop { caller: Option<String> },
    HostShutdown,
    Unknown,
}

impl std::fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal(signal) => write!(f, "{signal}"),
            Self::ApiStop {
                caller: Some(caller),
            } => write!(f, "server.stop request from {caller}"),
            Self::ApiStop { caller: None } => f.write_str("server.stop request"),
            Self::HostShutdown => f.write_str("host shutdown"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// The server's stop flag plus the reason recorded by the first stop request.
#[derive(Clone, Default)]
pub(crate) struct ServerStop {
    flag: Arc<AtomicBool>,
    reason: Arc<Mutex<Option<ShutdownReason>>>,
    /// Closed while a live handoff decides which server owns the session
    /// (bd herdr-t6kr, ADR-0018 D2): a stop arriving then is held as late.
    intake: Arc<Mutex<StopIntake>>,
}

#[derive(Default)]
enum StopIntake {
    #[default]
    Open,
    Closed {
        late: Option<ShutdownReason>,
    },
}

impl ServerStop {
    pub(crate) fn flag(&self) -> &Arc<AtomicBool> {
        &self.flag
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Records a stop. While the intake is closed for a handoff decision the
    /// stop is held as late instead (ADR-0018 D2 step 1).
    pub(crate) fn request(&self, reason: ShutdownReason) {
        let Ok(mut intake) = self.intake.lock() else {
            self.apply(reason);
            return;
        };
        if let StopIntake::Closed { late } = &mut *intake {
            tracing::info!(%reason, "a stop arrived during the handoff decision; held as a late stop");
            late.get_or_insert(reason);
            return;
        }
        // Applied under the intake lock, so it cannot slip in between the check
        // and the close of `close_intake_for_handoff`.
        self.apply(reason);
    }

    /// An api `server.stop`: refused while the intake is closed, so the caller
    /// retries against whichever server then owns the socket path.
    pub(crate) fn request_unless_deciding(&self, reason: ShutdownReason) -> bool {
        let Ok(intake) = self.intake.lock() else {
            self.apply(reason);
            return true;
        };
        if matches!(*intake, StopIntake::Closed { .. }) {
            return false;
        }
        self.apply(reason);
        true
    }

    fn apply(&self, reason: ShutdownReason) {
        if let Ok(mut recorded) = self.reason.lock() {
            recorded.get_or_insert(reason);
        }
        self.flag.store(true, Ordering::Release);
    }

    pub(crate) fn take_reason(&self) -> Option<ShutdownReason> {
        self.reason.lock().ok()?.take()
    }

    /// Closes the intake for a handoff decision. Returns false, leaving it
    /// open, when a stop was already requested.
    pub(crate) fn close_intake_for_handoff(&self) -> bool {
        let Ok(mut intake) = self.intake.lock() else {
            return false;
        };
        if self.is_requested() {
            return false;
        }
        *intake = StopIntake::Closed { late: None };
        true
    }

    /// Reopens the intake after an aborted handoff and applies a stop held as
    /// late. Returns whether one was.
    pub(crate) fn reopen_intake(&self) -> bool {
        let late = match self.intake.lock() {
            Ok(mut intake) => match std::mem::take(&mut *intake) {
                StopIntake::Closed { late } => late,
                StopIntake::Open => None,
            },
            Err(_) => None,
        };
        match late {
            Some(reason) => {
                self.apply(reason);
                true
            }
            None => false,
        }
    }

    /// Takes a stop held as late without applying it: after a commit it
    /// belongs to the replacement server. The intake stays closed.
    pub(crate) fn take_late_stop(&self) -> Option<ShutdownReason> {
        match &mut *self.intake.lock().ok()? {
            StopIntake::Closed { late } => late.take(),
            StopIntake::Open => None,
        }
    }
}

/// Test hook (codex t6kr-r4fix P2): names the file that [`trace_for_test`]
/// appends shutdown steps to, so an integration test can read their order
/// without depending on when an ordinary autosave runs.
pub(crate) const TRACE_FOR_TEST_ENV_VAR: &str = "HERDR_TEST_SHUTDOWN_TRACE";

/// Appends `step` as one line to the file named by [`TRACE_FOR_TEST_ENV_VAR`];
/// does nothing when the variable is unset.
pub(crate) fn trace_for_test(step: &str) {
    let Some(path) = std::env::var_os(TRACE_FOR_TEST_ENV_VAR) else {
        return;
    };
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, format!("{step}\n").as_bytes()));
    if let Err(err) = appended {
        tracing::warn!(err = %err, step, "failed to append to the shutdown trace for a test");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_signal_during_a_closed_intake_is_held_and_applied_on_reopen() {
        let stop = ServerStop::default();
        assert!(stop.close_intake_for_handoff());
        stop.request(ShutdownReason::HostShutdown);
        assert!(!stop.is_requested());
        assert!(stop.reopen_intake());
        assert!(stop.is_requested());
        assert_eq!(stop.take_reason(), Some(ShutdownReason::HostShutdown));
    }

    #[test]
    fn an_api_stop_during_a_closed_intake_is_refused_and_not_held() {
        let stop = ServerStop::default();
        assert!(stop.close_intake_for_handoff());
        assert!(!stop.request_unless_deciding(ShutdownReason::ApiStop { caller: None }));
        assert!(!stop.reopen_intake());
        assert!(!stop.is_requested());
        assert!(stop.request_unless_deciding(ShutdownReason::ApiStop { caller: None }));
        assert!(stop.is_requested());
    }

    #[test]
    fn the_intake_does_not_close_over_a_stop_already_requested() {
        let stop = ServerStop::default();
        stop.request(ShutdownReason::HostShutdown);
        assert!(!stop.close_intake_for_handoff());
        assert!(stop.request_unless_deciding(ShutdownReason::ApiStop { caller: None }));
    }

    #[test]
    fn a_late_stop_taken_after_a_commit_is_not_applied() {
        let stop = ServerStop::default();
        assert!(stop.close_intake_for_handoff());
        stop.request(ShutdownReason::HostShutdown);
        assert_eq!(stop.take_late_stop(), Some(ShutdownReason::HostShutdown));
        assert_eq!(stop.take_late_stop(), None);
        assert!(!stop.is_requested());
        assert!(!stop.reopen_intake());
    }

    #[test]
    fn first_stop_reason_wins() {
        let stop = ServerStop::default();
        assert!(!stop.is_requested());

        stop.request(ShutdownReason::HostShutdown);
        stop.request(ShutdownReason::ApiStop { caller: None });

        assert!(stop.is_requested());
        assert_eq!(stop.take_reason(), Some(ShutdownReason::HostShutdown));
        assert_eq!(stop.take_reason(), None);
    }

    #[test]
    fn api_stop_reason_names_the_caller() {
        let reason = ShutdownReason::ApiStop {
            caller: Some("pid 42 (herdr), parent pid 7 (zsh)".into()),
        };
        assert_eq!(
            reason.to_string(),
            "server.stop request from pid 42 (herdr), parent pid 7 (zsh)"
        );
    }
}
