use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

enum SessionSaveJob {
    Clear,
    Save {
        snapshot: crate::persist::SessionSnapshot,
        history: Option<crate::persist::SessionHistorySnapshot>,
    },
}

impl App {
    pub(super) fn schedule_session_save(&mut self) {
        if self.policy.persist_session {
            self.pane_exit_checkpoint_pending = false;
            self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.state.session_dirty {
            self.state.session_dirty = false;
            self.schedule_session_save();
        }
    }

    fn reap_finished_session_save(&mut self) {
        if self
            .session_save_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(thread) = self.session_save_thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn capture_session_save_job(&self) -> SessionSaveJob {
        if self.state.workspaces.is_empty() {
            SessionSaveJob::Clear
        } else {
            let snapshot = crate::persist::capture(
                &self.state.workspaces,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.state.active,
                self.state.selected,
            );
            let history = self.persist_pane_history.then(|| {
                crate::persist::capture_history(
                    &snapshot,
                    &self.state.workspaces,
                    &self.terminal_runtimes,
                )
            });
            SessionSaveJob::Save { snapshot, history }
        }
    }

    pub(crate) fn start_background_session_save(&mut self) {
        if !self.policy.persist_session {
            self.session_save_deadline = None;
            return;
        }

        self.reap_finished_session_save();
        if self.session_save_thread.is_some() {
            self.session_save_deadline = Some(Instant::now() + Duration::from_millis(250));
            return;
        }

        let job = self.capture_session_save_job();
        self.pane_exit_checkpoint_pending = false;
        self.session_save_deadline = None;
        let writer = self.session_writer.clone();
        match std::thread::Builder::new()
            .name("herdr-session-save".into())
            .spawn(move || {
                // Logged by the writer.
                let _ = run_session_save_job(job, &writer);
            }) {
            Ok(thread) => self.session_save_thread = Some(thread),
            Err(err) => {
                tracing::warn!(err = %err, "failed to spawn session save thread; saving inline");
                let _ = run_session_save_job(self.capture_session_save_job(), &self.session_writer);
            }
        }
    }

    pub(crate) fn save_session_now(&mut self) {
        // Logged by the writer.
        let _ = self.try_save_session_now();
    }

    /// [`Self::save_session_now`], and whether the session reached the disk
    /// (a failure is logged either way).
    pub(crate) fn try_save_session_now(&mut self) -> std::io::Result<()> {
        if let Some(thread) = self.session_save_thread.take() {
            let _ = thread.join();
        }

        if !self.policy.persist_session {
            self.session_save_deadline = None;
            return Ok(());
        }

        let saved = run_session_save_job(self.capture_session_save_job(), &self.session_writer);
        self.pane_exit_checkpoint_pending = false;
        self.session_save_deadline = None;
        saved
    }

    /// A live handoff transferred this session and the source no longer
    /// saves it: until a write reaches the disk, it exists nowhere else.
    #[cfg(unix)]
    pub(crate) fn owe_transferred_session(&mut self) {
        if !self.policy.persist_session {
            return;
        }
        match self.session_writer.lock() {
            Ok(mut writer) => writer.owe_transferred_session(),
            Err(err) => err.into_inner().owe_transferred_session(),
        }
    }

    /// Whether a session transferred by a live handoff is still not on disk.
    pub(crate) fn transferred_session_unsaved(&self) -> bool {
        match self.session_writer.lock() {
            Ok(writer) => writer.owes_transferred_session(),
            Err(err) => err.into_inner().owes_transferred_session(),
        }
    }

    pub(crate) fn checkpoint_session_before_pane_exit(&mut self) {
        if !self.policy.persist_session
            || (self.pane_exit_checkpoint_pending && !self.state.session_dirty)
        {
            return;
        }
        self.save_session_now();
        self.pane_exit_checkpoint_pending = true;
        self.state.session_dirty = false;
    }

    pub(crate) fn finish_checkpointed_pane_exit(&mut self) {
        if self.pane_exit_checkpoint_pending {
            self.state.session_dirty = false;
            self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) fn save_session_on_shutdown(&mut self) {
        if self.pane_exit_checkpoint_pending && !self.state.session_dirty {
            self.session_save_deadline = None;
            crate::server::shutdown::trace_for_test("final session save: checkpoint current");
            return;
        }
        // Logged by the writer.
        if self.try_save_session_now().is_ok() {
            crate::server::shutdown::trace_for_test("final session save");
        }
    }
}

fn run_session_save_job(
    job: SessionSaveJob,
    writer: &std::sync::Mutex<crate::persist::SessionWriter>,
) -> std::io::Result<()> {
    let mut writer = match writer.lock() {
        Ok(writer) => writer,
        Err(err) => {
            tracing::warn!(err = %err, "session writer is poisoned; refusing to modify session");
            return Err(std::io::Error::other("session writer is poisoned"));
        }
    };
    match job {
        SessionSaveJob::Clear => writer.try_clear(),
        SessionSaveJob::Save { snapshot, history } => writer.try_save(&snapshot, history.as_ref()),
    }
}
