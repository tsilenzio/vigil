//! Reactive session watching (ADR-0010, ADR-0011). Wraps a kqueue `Watcher`:
//! each session's `claude` PID is registered with `EVFILT_PROC`/`NOTE_EXIT` for
//! reactive death detection (SIGKILL fires no hook), and each session's transcript
//! with `EVFILT_VNODE`/`NOTE_WRITE` so an Esc interrupt marker is seen the instant
//! it is written rather than at a later sample.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kqueue::{EventFilter, FilterFlag, Ident, Watcher};

use crate::error::Error;

/// A reactive wake from the kqueue.
pub enum Wake {
    /// A watched process exited.
    Exited(u32),
    /// A watched transcript was written.
    Wrote(PathBuf),
}

pub struct SessionWatch {
    watcher: Watcher,
    pids: HashSet<u32>,
    transcripts: HashSet<PathBuf>,
}

impl SessionWatch {
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            watcher: Watcher::new()?,
            pids: HashSet::new(),
            transcripts: HashSet::new(),
        })
    }

    /// True when nothing is registered. The caller sleeps instead of polling in
    /// this case, since an unstarted kqueue returns immediately rather than
    /// blocking for the timeout.
    pub fn is_empty(&self) -> bool {
        self.pids.is_empty() && self.transcripts.is_empty()
    }

    pub fn is_pid_watched(&self, pid: u32) -> bool {
        self.pids.contains(&pid)
    }

    /// Register a PID for exit notification. Best-effort: a PID that cannot be
    /// registered falls back to the daemon's liveness check and staleness.
    pub fn watch_pid(&mut self, pid: u32) -> bool {
        if self.pids.contains(&pid) {
            return true;
        }
        if self
            .watcher
            .add_pid(
                pid as libc::pid_t,
                EventFilter::EVFILT_PROC,
                FilterFlag::NOTE_EXIT,
            )
            .is_err()
        {
            return false;
        }
        // watch() commits the whole changelist; if it fails, drop this PID so a
        // later watch() is not poisoned by a dead entry.
        if self.watcher.watch().is_err() {
            let _ = self
                .watcher
                .remove_pid(pid as libc::pid_t, EventFilter::EVFILT_PROC);
            return false;
        }
        self.pids.insert(pid);
        true
    }

    /// Register a transcript for write notification. Best-effort and idempotent;
    /// the file must exist (add_filename opens it and holds the fd).
    pub fn watch_transcript(&mut self, path: &Path) {
        if self.transcripts.contains(path) {
            return;
        }
        if self
            .watcher
            .add_filename(path, EventFilter::EVFILT_VNODE, FilterFlag::NOTE_WRITE)
            .is_err()
        {
            return;
        }
        if self.watcher.watch().is_err() {
            let _ = self
                .watcher
                .remove_filename(path, EventFilter::EVFILT_VNODE);
            return;
        }
        self.transcripts.insert(path.to_path_buf());
    }

    /// Drop transcript watches not in `keep`, closing their file descriptors, so
    /// the open-fd count stays bounded to live sessions.
    pub fn retain_transcripts(&mut self, keep: &HashSet<PathBuf>) {
        let stale: Vec<PathBuf> = self.transcripts.difference(keep).cloned().collect();
        for path in stale {
            let _ = self
                .watcher
                .remove_filename(&path, EventFilter::EVFILT_VNODE);
            self.transcripts.remove(&path);
        }
    }

    /// Block up to `timeout` for a reactive event. None on timeout. Must not be
    /// called when `is_empty()`, as an unstarted kqueue returns immediately.
    pub fn poll(&mut self, timeout: Duration) -> Option<Wake> {
        let event = self.watcher.poll(Some(timeout))?;
        match event.ident {
            Ident::Pid(pid) => {
                let pid = pid as u32;
                self.drop_pid(pid);
                Some(Wake::Exited(pid))
            }
            // Keep watching the transcript for later writes; do not drop it here.
            Ident::Filename(_, path) => Some(Wake::Wrote(PathBuf::from(path))),
            _ => None,
        }
    }

    fn drop_pid(&mut self, pid: u32) {
        if self.pids.remove(&pid) {
            let _ = self
                .watcher
                .remove_pid(pid as libc::pid_t, EventFilter::EVFILT_PROC);
        }
    }
}
