# ADR-0002: Single daemon-owned caffeinate, reference-counted

**Status:** Draft

**Date:** 2026-07-02

## Context

The machine runs multiple Claude Code sessions at once (observed during testing: a
second session in `~/Code/misc/projects/homelab` held its own assertion
alongside the working session), and each session spawns subagents. A per-session
`caffeinate` keyed by session id, as in v1, produces one process per session and
leaves orphaned processes when a stop event does not fire. The goal is a single
assertion held while any session is active, regardless of session or subagent
count.

The display-sleep assertion is system-wide (`PreventUserIdleDisplaySleep` is a
global counter), so more than one `caffeinate -di` is redundant. What matters is
that at least one is held while work is happening and none is held when it is not.

## Decision

The daemon owns exactly one `caffeinate -di -t <SAFETY_SECS>` child. It holds the
assertion while any session log is active and kills the child when all sessions go
idle. A machine-wide advisory lock (`flock` on `/tmp/vigil/daemon.lock`) guarantees
a single daemon; a second daemon that cannot acquire the lock exits.

## Consequences

**What this covers.** One caffeinate and one daemon regardless of session and
subagent count. Ending one session does not release the assertion while another is
active, because reference counting reads all session logs.

**Single-instance requirement.** Correctness depends on the flock. Two daemons
would double-manage the caffeinate. The recorder spawns a daemon after every event,
so redundant spawns must be cheap no-ops, which the lock provides.

**Startup race.** A recorder can append a fresh line and spawn a daemon just as the
current daemon decides to exit. An exit grace window of two polls absorbs this (see
ADR-0007).

## References

- `../SPEC.md` sections "Daemon", "caffeinate management", "Single-instance (flock)"
- `pmset -g assertions` evidence, 2026-07-02 (Claude's own caffeinate is
  system-sleep only)
- ADR-0001 (supervisor architecture), ADR-0007 (daemon lifecycle)
