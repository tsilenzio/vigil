# ADR-0010: Session liveness detection via process identity

**Status:** Draft

**Date:** 2026-07-06

## Context

Staleness-based release (ADR-0006) treats a session as no longer active once its
log's newest line ages past a timeout. It was adopted because Claude Code fires no
hook on a user interrupt, so no explicit "stopped" signal exists. It has two
limits: it cannot distinguish a killed session from an idle one, and it delays
release by the full timeout.

Testing on 2026-07-06 clarified which terminations fire hooks. Method: the wired
`log-event.sh` logger was enabled and throwaway headless `claude` sessions were
terminated three ways, counting `SessionEnd` entries.

| Termination | `SessionEnd` fires | reason |
|---|---|---|
| Clean exit | yes | `other` |
| SIGTERM (default `kill`) | yes | `other` |
| SIGKILL (`kill -9`) | no | — |

Clean exit and SIGTERM fire `SessionEnd` (Claude Code installs a shutdown handler).
SIGKILL, OOM-kill, and panic fire nothing, because the process runs no code on the
way out. Separately, an Esc interrupt fires no hook and does not end the session:
the process stays alive and the log's newest line simply stops advancing.

`lsof` on the transcript was evaluated as a liveness signal and rejected. Across 55
live sessions on 2026-07-06, zero held their transcript open (`lsof` on the
transcript returned no rows; the process appends and closes per write). An open
handle therefore cannot indicate that an idle-but-alive session is running, which
is the state most sessions are in most of the time.

A signal is needed that reports whether a session's underlying process is alive,
independent of any hook.

## Decision

The recorder captures the Claude Code process identity at hook time. A hook runs as
a descendant of the session process (observed ancestry: `claude` to the hook shell
to `vigil`, two hops). The recorder walks the ancestry to the `claude` process and
records its PID and start time in the session log.

The daemon treats a session as alive when the recorded PID is alive and its current
start time matches the recorded one. Liveness is probed with `kill(pid, 0)`
(signal 0 sends no signal and reports existence) in the polling form, or observed
reactively (ADR-0011). The start time defends against PID reuse: a reused PID
carries a newer start time and reads as dead.

## Consequences

**Deterministic death detection.** SIGKILL, OOM-kill, and crash, none of which fire
a hook, are detected by the process being gone. Verified 2026-07-06: `kill -0` on a
process flips from success to failure the instant it is sent SIGKILL, and reports a
nonexistent PID as dead.

**Replaces what staleness approximated.** A killed session releases on liveness
rather than after a timeout, and a killed session is distinguishable from an idle
one.

**Relies on descendant capture, not argv.** Correctness depends on the recorder
being a descendant of the `claude` process, which holds for hooks. Some sessions
expose their `session_id` in argv (`claude --resume <uuid>`) and some do not (a
fresh session shows bare `claude`), so ancestry capture at hook time is the
reliable source of the PID, not argv matching.

**Timeout demoted, not removed.** The idle timeout is retained as a fail-open
backstop for the residue that leaves no live process and fires no event (power
loss, panic, a missed PID capture). It is no longer the primary release path.

## SPEC impact

To be applied in a dedicated spec-update session:

- "Event Log / Line Schema": add `pid` (u64) and `pid_start` (process start marker)
  fields, and `transcript` (path, for ADR-0011).
- "Reference counting & staleness": a session is active when its process is alive
  (per this ADR) and, if alive, not idle-awaiting-input; add liveness alongside
  staleness.
- "Lifecycle & Cleanup": death-driven release and log GC supplement the stale-line
  GC.
- "CLI status": report per-session liveness (alive/dead, PID).

## References

- `../SPEC.md` sections "Event Log", "Daemon", "Reference counting & staleness"
- Termination and `lsof` evidence, 2026-07-06 (55 live sessions, 0 transcripts held
  open; SIGTERM fires `SessionEnd`, SIGKILL does not)
- ADR-0006 (staleness release, amended by this ADR), ADR-0011 (reactive loop
  consumes this signal), ADR-0007 (daemon lifecycle)
