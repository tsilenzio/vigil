//! The daemon's decision journal (TODO-003): an always-on append-only log at
//! `${VIGIL_RUNTIME_DIR}/daemon.log` recording each hold decision. One line per
//! decision change, a sparse heartbeat so a quiet daemon is distinguishable from
//! a wedged one, and start/exit lines so exit reasons survive the daemon.
//! Appends do not fire the runtime-dir watch (ADR-0012), so the daemon never
//! wakes itself, and the page cache preserves every line written before a
//! SIGKILL. Writes are best-effort: journaling must never take the daemon down.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::event;

/// One journal line. `state` is present on decision and heartbeat lines,
/// `reason` on exit lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub ts: u64,
    pub kind: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The hold decision and its inputs, as computed on one loop pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub active: bool,
    pub want_hold: bool,
    pub battery_capped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_since: Option<u64>,
    pub on_ac: bool,
    pub charge: u8,
}

/// What one loop pass owes the journal.
enum Due {
    Decision,
    Heartbeat,
    Nothing,
}

/// A changed state is a decision line. An unchanged one is a heartbeat once
/// `JOURNAL_HEARTBEAT` has passed since the last write, else nothing.
fn due(state: &State, last_state: Option<&State>, now: u64, last_write: u64) -> Due {
    if last_state != Some(state) {
        Due::Decision
    } else if now.saturating_sub(last_write) >= config::JOURNAL_HEARTBEAT {
        Due::Heartbeat
    } else {
        Due::Nothing
    }
}

/// The journal writer. Constructed only by the lock-holding daemon, which keeps
/// the file single-writer.
pub struct Journal {
    path: PathBuf,
    pid: u32,
    last_state: Option<State>,
    last_write: u64,
}

impl Journal {
    /// Open the daemon's journal and write the start line.
    pub fn start(now: u64) -> Self {
        Self::at(config::journal_path(), now)
    }

    fn at(path: PathBuf, now: u64) -> Self {
        let journal = Self {
            path,
            pid: std::process::id(),
            last_state: None,
            last_write: now,
        };
        journal.write(now, "start", None, None, None);
        journal
    }

    /// Journal one loop pass: a decision line when the state changed, a
    /// heartbeat when quiet past the interval, else nothing.
    pub fn record(&mut self, now: u64, wake: &str, state: State) {
        let kind = match due(&state, self.last_state.as_ref(), now, self.last_write) {
            Due::Decision => "decision",
            Due::Heartbeat => "heartbeat",
            Due::Nothing => return,
        };
        self.write(now, kind, Some(wake.to_string()), Some(state), None);
        self.last_state = Some(state);
        self.last_write = now;
    }

    /// Write the exit line: why the daemon is leaving (idle, disabled,
    /// self-upgrade).
    pub fn exit(&self, now: u64, reason: &str) {
        self.write(now, "exit", None, None, Some(reason.to_string()));
    }

    fn write(
        &self,
        ts: u64,
        kind: &str,
        wake: Option<String>,
        state: Option<State>,
        reason: Option<String>,
    ) {
        append(
            &self.path,
            &Entry {
                ts,
                kind: kind.to_string(),
                pid: self.pid,
                wake,
                state,
                reason,
            },
        );
    }
}

/// Append one line, rotating the file aside first once it passes the size cap.
/// Best-effort: errors are swallowed, so a full or broken disk degrades the
/// journal rather than the daemon.
fn append(path: &Path, entry: &Entry) {
    let Ok(mut line) = serde_json::to_string(entry) else {
        return;
    };
    line.push('\n');

    if fs::metadata(path).is_ok_and(|m| m.len() >= config::JOURNAL_MAX_BYTES) {
        // The rename fires the dir watch once, which is rare and converges.
        let _ = fs::rename(path, path.with_extension("log.old"));
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = file.write_all(line.as_bytes());
}

/// The newest journal entry. `None` when the file is absent, empty, or its
/// newest line does not parse.
pub fn read_last(path: &Path) -> Option<Entry> {
    serde_json::from_str(&event::tail_last_line(path)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            active: true,
            want_hold: true,
            battery_capped: false,
            hold_since: None,
            on_ac: true,
            charge: 100,
        }
    }

    fn entry(ts: u64) -> Entry {
        Entry {
            ts,
            kind: "decision".to_string(),
            pid: 42,
            wake: Some("tick".to_string()),
            state: Some(state()),
            reason: None,
        }
    }

    #[test]
    fn entry_round_trips() {
        let entry = entry(1);
        let line = serde_json::to_string(&entry).unwrap();
        let back: Entry = serde_json::from_str(&line).unwrap();
        assert_eq!(entry, back);
        // Absent optionals are omitted from the line.
        assert!(!line.contains("\"reason\""));
    }

    #[test]
    fn due_is_decision_on_change_heartbeat_when_quiet() {
        let s = state();
        // First sight and any change are decision lines.
        assert!(matches!(due(&s, None, 0, 0), Due::Decision));
        let mut drained = s;
        drained.charge = 90;
        assert!(matches!(due(&drained, Some(&s), 10, 0), Due::Decision));
        // Unchanged: nothing until the heartbeat interval, then a heartbeat.
        assert!(matches!(
            due(&s, Some(&s), config::JOURNAL_HEARTBEAT - 1, 0),
            Due::Nothing
        ));
        assert!(matches!(
            due(&s, Some(&s), config::JOURNAL_HEARTBEAT, 0),
            Due::Heartbeat
        ));
    }

    #[test]
    fn append_read_last_and_rotation() {
        let dir = std::env::temp_dir().join(format!("vigil-journal-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.log");
        let _ = fs::remove_file(&path);

        append(&path, &entry(1));
        append(&path, &entry(2));
        assert_eq!(read_last(&path), Some(entry(2)));

        // Past the cap the file rotates aside and the new line starts fresh.
        fs::write(&path, vec![b'x'; config::JOURNAL_MAX_BYTES as usize]).unwrap();
        append(&path, &entry(3));
        assert_eq!(read_last(&path), Some(entry(3)));
        assert!(path.with_extension("log.old").exists());

        fs::remove_dir_all(&dir).unwrap();
    }
}
