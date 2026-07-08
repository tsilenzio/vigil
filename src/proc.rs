//! Process identity for session liveness (ADR-0010). A hook runs as a descendant
//! of the session's `claude` process, so the recorder can walk the ancestry to it
//! and record its PID plus start time. The start time survives PID reuse: a reused
//! PID carries a newer start time.

use std::collections::HashMap;
use std::process::Command;

/// PID and start time of a session's `claude` process. The pair is the liveness
/// key: alive iff the PID is up and its current start time still matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcId {
    pub pid: u32,
    pub start: String,
}

struct Entry {
    ppid: u32,
    start: String,
    comm: String,
}

/// The maximum ancestry depth to search. The hook shell sits one or two hops
/// below `claude`; the bound guards against a cycle or a detached tree.
const MAX_DEPTH: usize = 8;

/// Walk this process's ancestry to the nearest `claude` ancestor and return its
/// identity. None if no such ancestor is found (the daemon then falls back to the
/// staleness backstop for that session).
pub fn capture_claude() -> Option<ProcId> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,lstart=,comm="])
        .output()
        .ok()?;
    let table = parse_table(&String::from_utf8_lossy(&output.stdout));
    walk_to_claude(&table, std::process::id())
}

fn parse_table(output: &str) -> HashMap<u32, Entry> {
    let mut table = HashMap::new();
    for line in output.lines() {
        // Fields: pid ppid <lstart is exactly 5 whitespace tokens> comm...
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 7 {
            continue;
        }
        let (Ok(pid), Ok(ppid)) = (tokens[0].parse(), tokens[1].parse()) else {
            continue;
        };
        let start = tokens[2..7].join(" ");
        let comm = tokens[7..].join(" ");
        table.insert(pid, Entry { ppid, start, comm });
    }
    table
}

fn walk_to_claude(table: &HashMap<u32, Entry>, start_pid: u32) -> Option<ProcId> {
    let mut pid = start_pid;
    for _ in 0..MAX_DEPTH {
        let entry = table.get(&pid)?;
        if entry.comm.contains("claude") {
            return Some(ProcId {
                pid,
                start: entry.start.clone(),
            });
        }
        if entry.ppid == 0 || entry.ppid == pid {
            return None;
        }
        pid = entry.ppid;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fields as `ps -axo pid=,ppid=,lstart=,comm=` emits them: lstart is 5 tokens.
    const SAMPLE: &str = "\
  501     1 Thu Jul  2 20:20:25 2026 claude
15294 18248 Mon Jul  6 15:48:57 2026 zsh
18248   501 Thu Jul  2 20:20:25 2026 claude
  999   999 Wed Jul  1 09:00:00 2026 launchd";

    #[test]
    fn walks_hook_shell_up_to_claude() {
        let table = parse_table(SAMPLE);
        // vigil would be a child of the zsh hook shell (15294).
        let found = walk_to_claude(&table, 15294).unwrap();
        assert_eq!(found.pid, 18248);
        // The claude process's own start time, not the intervening hook shell's.
        assert_eq!(found.start, "Thu Jul 2 20:20:25 2026");
    }

    #[test]
    fn comm_with_version_path_still_matches() {
        // App-forked sessions show the version binary path, which contains "claude".
        let table = parse_table(
            "42 1 Thu Jul  2 20:20:25 2026 /Users/x/.local/share/claude/versions/2.1.154",
        );
        assert_eq!(walk_to_claude(&table, 42).unwrap().pid, 42);
    }

    #[test]
    fn no_claude_ancestor_returns_none() {
        let table =
            parse_table("100 1 Thu Jul  2 20:20:25 2026 zsh\n1 0 Wed Jul  1 09:00:00 2026 launchd");
        assert_eq!(walk_to_claude(&table, 100), None);
    }
}
