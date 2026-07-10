//! The per-session event log: the JSONL line schema, its file I/O, the recorder
//! entry point, and commit detection. The recorder writes raw events only; the
//! daemon derives all policy from them (ADR-0004).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::Error;
use crate::proc;

/// A newest-line tail read never needs more than this many bytes: log lines are
/// one small event each, so the last complete line lives well inside the window.
const TAIL_CHUNK: u64 = 8192;

/// The six lifecycle events wired as hooks. Terminal events end the turn or
/// session and delete the log instead of appending.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum EventKind {
    #[value(name = "UserPromptSubmit")]
    UserPromptSubmit,
    #[value(name = "PreToolUse")]
    PreToolUse,
    #[value(name = "PostToolUse")]
    PostToolUse,
    #[value(name = "Stop")]
    Stop,
    #[value(name = "StopFailure")]
    StopFailure,
    #[value(name = "SessionEnd")]
    SessionEnd,
}

impl EventKind {
    /// The six events an install wires as hooks, and the order they are listed.
    pub const ALL: [EventKind; 6] = [
        Self::UserPromptSubmit,
        Self::PreToolUse,
        Self::PostToolUse,
        Self::Stop,
        Self::StopFailure,
        Self::SessionEnd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::SessionEnd => "SessionEnd",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Stop | Self::StopFailure | Self::SessionEnd)
    }
}

/// One log line. `agent_id` is recorded for potential future use and is not
/// consulted by the caffeinate logic. `pid`/`pid_start` identify the session's
/// `claude` process for liveness (ADR-0010); `transcript` locates its transcript
/// for interrupt detection (ADR-0011). All three are session-constant but written
/// on every line so the daemon reads them from the newest line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub ts: u64,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

/// The raw hook JSON on stdin. Only the fields the recorder extracts are named;
/// the rest of the payload is ignored.
#[derive(Deserialize)]
struct HookPayload {
    session_id: String,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<ToolInput>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
}

#[derive(Deserialize)]
struct ToolInput {
    #[serde(default)]
    command: Option<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse the hook payload and either append an event line or, for a terminal
/// event, delete the session log.
pub fn record(kind: EventKind, hook_json: &str) -> Result<(), Error> {
    let payload: HookPayload = serde_json::from_str(hook_json)?;

    if kind.is_terminal() {
        delete(&payload.session_id)?;
        return Ok(());
    }

    let identity = proc::capture_claude();
    let event = Event {
        ts: now_secs(),
        event: kind.as_str().to_string(),
        tool: payload.tool_name,
        command: payload.tool_input.and_then(|t| t.command),
        agent_id: payload.agent_id,
        pid: identity.as_ref().map(|id| id.pid),
        pid_start: identity.map(|id| id.start),
        transcript: payload.transcript_path,
    };
    append(&payload.session_id, &event)
}

fn append(session_id: &str, event: &Event) -> Result<(), Error> {
    fs::create_dir_all(config::vigil_dir())?;
    let mut line = serde_json::to_string(event)?;
    line.push('\n');

    // O_APPEND: one small line per write stays atomic against concurrent
    // recorders (multiple sessions, or a subagent and its parent).
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(config::log_path(session_id))?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn delete(session_id: &str) -> Result<(), Error> {
    match fs::remove_file(config::log_path(session_id)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Read the newest complete line of a file as a string, ignoring a trailing
/// partial append. Returns `None` on an empty file or a partial-only file.
fn tail_last_line(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }

    let start = len.saturating_sub(TAIL_CHUNK);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;

    last_complete_line(&buf).map(str::to_string)
}

/// Read the newest complete line of a session log. `None` on an empty file, a
/// partial-only file, or a parse failure.
pub fn read_last_line(path: &Path) -> Option<Event> {
    serde_json::from_str(&tail_last_line(path)?).ok()
}

/// The marker Claude Code writes to a session transcript when the user interrupts
/// a turn (Esc). Matched as a substring to tolerate the `... for tool use]`
/// variant and future suffixes.
const INTERRUPT_MARKER: &str = "[Request interrupted by user";

/// True when a transcript's newest line is the user-interrupt marker (ADR-0011).
/// Fail-open: an unreadable or reshaped transcript reads as false, so the session
/// falls through to the staleness backstop rather than releasing on a bad read.
pub fn is_interrupt_transcript(path: &Path) -> bool {
    tail_last_line(path).is_some_and(|line| line.contains(INTERRUPT_MARKER))
}

/// The last newline-terminated line in `buf`, trimmed. Content after the final
/// newline is an in-progress append and is ignored.
fn last_complete_line(buf: &[u8]) -> Option<&str> {
    let last_nl = buf.iter().rposition(|&b| b == b'\n')?;
    let line_start = buf[..last_nl]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let line = std::str::from_utf8(&buf[line_start..last_nl]).ok()?.trim();
    (!line.is_empty()).then_some(line)
}

/// True when the session's newest line is an in-flight commit: a `PreToolUse`
/// for a `Bash` `git commit` with no `PostToolUse` after it yet.
pub fn is_unmatched_commit(last: &Event) -> bool {
    last.event == "PreToolUse"
        && last.tool.as_deref() == Some("Bash")
        && last.command.as_deref().is_some_and(is_commit_command)
}

/// True when `command` is a `git commit` invocation. Splits on shell separators
/// so `git add -A && git commit` matches, and skips leading `git` option tokens
/// so `git -C <dir> commit` matches. A commit run through a shell alias is not
/// detected. Mirrors the behavioral spec in SPEC.md "Commit-aware timeout".
pub fn is_commit_command(command: &str) -> bool {
    command.split([';', '|', '&']).any(is_git_commit_segment)
}

fn is_git_commit_segment(segment: &str) -> bool {
    let mut tokens = segment.split_whitespace();
    if tokens.next() != Some("git") {
        return false;
    }

    while let Some(token) = tokens.next() {
        if token == "commit" {
            return true;
        }
        // `-C <dir>` and `-c <name=value>` take a following value; skip it.
        if token == "-C" || token == "-c" {
            tokens.next();
            continue;
        }
        // Any other pre-subcommand option flag.
        if token.starts_with('-') {
            continue;
        }
        // A different subcommand (`log`, `status`, ...).
        return false;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_commands_match() {
        assert!(is_commit_command("git commit"));
        assert!(is_commit_command("git commit -m \"x\""));
        assert!(is_commit_command("git -C /path commit"));
        assert!(is_commit_command("git add -A && git commit"));
        assert!(is_commit_command("git commit --amend"));
        assert!(is_commit_command("git -c user.name=x commit -m y"));
    }

    #[test]
    fn non_commit_commands_reject() {
        assert!(!is_commit_command("git log"));
        assert!(!is_commit_command("git status"));
        assert!(!is_commit_command("echo git commit"));
        assert!(!is_commit_command("git commit-tree"));
    }

    #[test]
    fn unmatched_commit_needs_pretooluse_bash() {
        let commit = |event: &str| Event {
            ts: 1,
            event: event.to_string(),
            tool: Some("Bash".to_string()),
            command: Some("git commit -m x".to_string()),
            ..Default::default()
        };
        assert!(is_unmatched_commit(&commit("PreToolUse")));
        // A PostToolUse for the same command means the commit returned.
        assert!(!is_unmatched_commit(&commit("PostToolUse")));

        let non_bash = Event {
            tool: Some("Edit".to_string()),
            ..commit("PreToolUse")
        };
        assert!(!is_unmatched_commit(&non_bash));
    }

    #[test]
    fn event_round_trips() {
        let event = Event {
            ts: 1751490000,
            event: "PreToolUse".to_string(),
            tool: Some("Bash".to_string()),
            command: Some("git commit -m \"x\"".to_string()),
            ..Default::default()
        };
        let line = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&line).unwrap();
        assert_eq!(event, back);
        // agent_id serializes even when null; tool/command omitted when absent.
        assert!(line.contains("\"agent_id\":null"));
    }

    #[test]
    fn interrupt_marker_matches_only_the_marker_line() {
        let interrupted = b"{\"type\":\"assistant\"}\n{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user for tool use]\"}]}}\n";
        assert!(
            last_complete_line(interrupted)
                .unwrap()
                .contains(INTERRUPT_MARKER)
        );

        let normal = b"{\"type\":\"assistant\",\"text\":\"still working\"}\n";
        assert!(
            !last_complete_line(normal)
                .unwrap()
                .contains(INTERRUPT_MARKER)
        );
    }

    #[test]
    fn last_complete_line_cases() {
        assert_eq!(last_complete_line(b""), None);
        assert_eq!(last_complete_line(b"only\n"), Some("only"));
        assert_eq!(last_complete_line(b"partial"), None);
        assert_eq!(last_complete_line(b"one\ntwo\n"), Some("two"));
        assert_eq!(last_complete_line(b"one\ntwo\npart"), Some("two"));
    }
}
