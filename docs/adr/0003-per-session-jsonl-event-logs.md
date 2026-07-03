# ADR-0003: Per-session JSONL event logs keyed by session_id

**Status:** Draft

**Date:** 2026-07-02

## Context

The recorder (invoked by hooks) and the daemon are separate processes and need
shared state: which sessions exist, when each was last active, and whether a commit
is in flight. The state must be writable by many short-lived recorder processes
concurrently and readable by the daemon on each poll.

Subagent behavior was verified on 2026-07-02: a subagent's tool calls fire
`PreToolUse` and `PostToolUse` under the parent session_id with a non-null
`agent_id`; the main agent's tools carry the parent session_id with `agent_id`
null. So keying by session_id makes subagent activity refresh the parent session's
state with no special handling.

## Decision

One append-only JSONL file per session at `/tmp/vigil/<session_id>.jsonl`. Each line
is `{ts, event, tool?, command?, agent_id?}`. Recorders append a single line with
`O_APPEND`. The daemon reads each file's last complete line. `/tmp` is used because
the state is ephemeral and reboot-clearing is acceptable.

## Consequences

**What this enables.** Appendable and tailable state with atomic single-line writes,
readable without coordination beyond the append. The format is extensible: future
reactors can consume the same stream, and new fields can be added without breaking
older readers.

**Subagents are free.** No subagent-specific hooks are needed; their activity lands
in the parent session's log.

**Concurrency bound.** Correctness relies on `O_APPEND` single-line atomicity below
`PIPE_BUF`. Lines are kept small (one event each). The daemon ignores a trailing
partial line.

**Growth.** A long session's log grows unbounded within the session. The daemon
reads only the last line, so growth does not affect the hot path, and the log is
deleted on session end or reboot.

## References

- `../SPEC.md` section "Event Log"
- Subagent hook-event test, 2026-07-02 (parent session_id + agent_id)
- ADR-0004 (thin hooks), ADR-0002 (reference counting reads these logs)
