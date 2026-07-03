# ADR-0001: Event-log-driven supervisor architecture

**Status:** Draft

**Date:** 2026-07-02

## Context

The first-generation fix keeps the display awake for GPG Touch ID signing with two
bash hooks: `UserPromptSubmit` starts a `caffeinate -di`, `Stop` (plus
`StopFailure` and `SessionEnd`) kills it. It works for normal turns but two failure
modes surfaced during testing:

- Esc fires no hook, so an interrupted-then-abandoned turn holds the assertion
  until the next completed turn reuses it or the 12h safety cap expires.
- A blocked `git commit` (Touch ID sheet, or the password dialog it falls back to)
  produces no tool activity while blocked, so any activity-based scheme reads it as
  idle.

Two simpler designs were considered and rejected. A per-turn hold with a shorter
safety cap bounds the Esc leak but drops the assertion mid-turn on long single
turns. A plain heartbeat (refresh a short-lived `caffeinate` on each activity
event) bounds the Esc leak and survives long turns, but still expires during a
blocked commit because the block looks like idle.

## Decision

Adopt an event-log-driven supervisor. Hooks invoke `vigil record <event>`, which
appends one line to a per-session JSONL log and ensures a daemon is running. A
single `vigil daemon` reference-counts active sessions by reading the logs and owns
exactly one `caffeinate -di`. Release is driven by staleness rather than a stop
event, and the per-session timeout is extended while that session's newest event is
an in-flight commit.

## Consequences

**What this covers.** Esc (no hook needed, staleness releases), blocked commits
(commit-aware timeout), long turns (each tool event refreshes the log), and
multiple concurrent sessions and subagents (reference counting), all in one model.

**What it costs.** A long-running daemon with a lifecycle to manage: single-instance
guarantee, self-exit, and crash recovery. This is more surface than two short
scripts, and daemon bugs are subtler than a stale pidfile.

**Migration.** The v1 bash hooks stay wired until the daemon passes the scenario
matrix, then the six hook events repoint to `vigil record`.

## References

- `../SPEC.md` sections "Overview & Motivation", "Architecture"
- `~/Code/misc/scripts/claude-caffeinate-hooks/` (v1 hooks and README, retained
  until swap)
- ADR-0002 (single daemon-owned caffeinate), ADR-0005 (commit-aware timeout),
  ADR-0006 (staleness-based release)
