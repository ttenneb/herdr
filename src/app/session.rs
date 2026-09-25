use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

enum SessionSaveJob {
    Clear,
    Save {
        snapshot: Box<crate::persist::SessionSnapshot>,
        history: Option<crate::persist::SessionHistorySnapshot>,
    },
}

impl App {
    pub(super) fn schedule_session_save(&mut self) {
        if !self.no_session {
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
                if thread.join().is_err() {
                    self.session_writer_healthy
                        .store(false, std::sync::atomic::Ordering::Release);
                    self.ready_delegation_routes.clear();
                    tracing::warn!("session writer panicked; report routes quarantined");
                }
            }
        }
    }

    fn capture_session_save_job(&self) -> SessionSaveJob {
        if self.state.workspaces.is_empty() {
            SessionSaveJob::Clear
        } else {
            let snapshot = crate::persist::capture(
                &self.state.workspaces,
                &self.state.repositories,
                &self.state.space_order,
                &self.state.delegations,
                &self.state.collection_archive_times,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.state.active,
                self.state.selected,
                self.state.sidebar_width,
                self.state.sidebar_section_split,
                self.state.collapsed_space_keys.clone(),
            );
            let history = self.persist_pane_history.then(|| {
                crate::persist::capture_history(&self.state.workspaces, &self.terminal_runtimes)
            });
            SessionSaveJob::Save {
                snapshot: Box::new(snapshot),
                history,
            }
        }
    }

    pub(crate) fn start_background_session_save(&mut self) {
        if self.no_session {
            self.session_save_deadline = None;
            return;
        }

        self.reap_finished_session_save();
        if self.session_save_thread.is_some() {
            self.session_save_deadline = Some(Instant::now() + Duration::from_millis(250));
            return;
        }

        let job = self.capture_session_save_job();
        self.session_save_deadline = None;
        let path = self.session_save_path.clone();
        let writer = self.session_writer.clone();
        let health = self.session_writer_healthy.clone();
        match std::thread::Builder::new()
            .name("herdr-session-save".into())
            .spawn(move || run_session_save_job(job, &path, writer.as_ref(), &health))
        {
            Ok(thread) => self.session_save_thread = Some(thread),
            Err(err) => {
                tracing::warn!(err = %err, "failed to spawn session save thread; saving inline");
                run_session_save_job(
                    self.capture_session_save_job(),
                    &self.session_save_path,
                    self.session_writer.as_ref(),
                    &self.session_writer_healthy,
                );
            }
        }
    }

    pub(crate) fn save_session_now(&mut self) {
        if let Some(thread) = self.session_save_thread.take() {
            if thread.join().is_err() {
                self.session_writer_healthy
                    .store(false, std::sync::atomic::Ordering::Release);
                self.ready_delegation_routes.clear();
                tracing::warn!("session writer panicked; report routes quarantined");
            }
        }

        if self.no_session {
            self.session_save_deadline = None;
            return;
        }

        run_session_save_job(
            self.capture_session_save_job(),
            &self.session_save_path,
            self.session_writer.as_ref(),
            &self.session_writer_healthy,
        );
        self.session_save_deadline = None;
    }

    /// The only promotion barrier for a report route. No old background
    /// snapshot can follow this write, and other processes cannot acquire the
    /// directory writer while this App retains the lease.
    pub(crate) fn durably_save_delegation_edge(&mut self) -> std::io::Result<()> {
        let result = (|| -> std::io::Result<()> {
            if self.no_session {
                return Err(std::io::Error::other("session persistence disabled"));
            }
            if let Some(thread) = self.session_save_thread.take() {
                thread
                    .join()
                    .map_err(|_| std::io::Error::other("prior session writer panicked"))?;
            }
            // The old inode can remain locked after the pathname is replaced.
            // It conveys no ownership of the new lock: release it only after
            // the previous background writer has joined, then acquire anew.
            if self
                .session_writer
                .as_ref()
                .is_some_and(|writer| writer.validate(&self.session_save_path).is_err())
            {
                self.session_writer.take();
            }
            if self.session_writer.is_none() {
                self.session_writer = Some(crate::persist::SessionWriter::acquire(
                    &self.session_save_path,
                )?);
            }
            let SessionSaveJob::Save { snapshot, .. } = self.capture_session_save_job() else {
                return Err(std::io::Error::other("no session workspaces to persist"));
            };
            crate::persist::save_snapshot_ordered(
                &self.session_save_path,
                &snapshot,
                self.session_writer.as_ref().expect("writer acquired"),
            )?;
            Ok(())
        })();
        if result.is_err() {
            self.session_writer_healthy
                .store(false, std::sync::atomic::Ordering::Release);
            self.ready_delegation_routes.clear();
        } else {
            self.session_writer_healthy
                .store(true, std::sync::atomic::Ordering::Release);
        }
        result
    }
}

fn run_session_save_job(
    job: SessionSaveJob,
    path: &std::path::Path,
    writer: Option<&std::sync::Arc<crate::persist::SessionWriter>>,
    health: &std::sync::atomic::AtomicBool,
) {
    let result = match job {
        SessionSaveJob::Clear => crate::persist::clear_ordered(path, writer),
        SessionSaveJob::Save { snapshot, history } => {
            crate::persist::save_ordered(path, &snapshot, history.as_ref(), writer)
        }
    };
    if let Err(err) = result {
        health.store(false, std::sync::atomic::Ordering::Release);
        crate::logging::session_save_failed(path, &err.to_string());
    }
}
