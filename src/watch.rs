//! Reactive session-death detection (ADR-0011). Wraps a kqueue `Watcher` that
//! registers each session's `claude` PID with `EVFILT_PROC`/`NOTE_EXIT`, so the
//! kernel reports a process exit the instant it happens, including SIGKILL, which
//! fires no hook.

use std::collections::HashSet;
use std::time::Duration;

use kqueue::{EventFilter, FilterFlag, Ident, Watcher};

use crate::error::Error;

pub struct SessionWatch {
    watcher: Watcher,
    registered: HashSet<u32>,
}

impl SessionWatch {
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            watcher: Watcher::new()?,
            registered: HashSet::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.registered.is_empty()
    }

    pub fn is_watched(&self, pid: u32) -> bool {
        self.registered.contains(&pid)
    }

    /// Register a PID for exit notification. Best-effort: a PID that cannot be
    /// registered (already exited, or a transient kqueue error) is left
    /// unregistered and the daemon falls back to its liveness check and the
    /// staleness backstop. Returns whether the PID is now watched.
    pub fn watch_pid(&mut self, pid: u32) -> bool {
        if self.registered.contains(&pid) {
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
        self.registered.insert(pid);
        true
    }

    /// Block up to `timeout` for a watched process to exit. Returns the exited
    /// PID, or None on timeout. Only exit events are requested, so any process
    /// event is an exit. The caller must not rely on this when `is_empty()`, as
    /// an unstarted kqueue returns immediately rather than blocking.
    pub fn poll(&mut self, timeout: Duration) -> Option<u32> {
        let event = self.watcher.poll(Some(timeout))?;
        if let Ident::Pid(pid) = event.ident {
            let pid = pid as u32;
            self.drop_pid(pid);
            Some(pid)
        } else {
            None
        }
    }

    fn drop_pid(&mut self, pid: u32) {
        if self.registered.remove(&pid) {
            let _ = self
                .watcher
                .remove_pid(pid as libc::pid_t, EventFilter::EVFILT_PROC);
        }
    }
}
