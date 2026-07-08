# ADR-0011: Reactive kqueue event loop

**Status:** Draft

**Date:** 2026-07-06

## Context

The daemon (ADR-0007) polls every `POLL_INTERVAL`. Release decisions are evaluated
on each poll, so release latency is bounded by the interval and the daemon wakes
even when nothing has changed. The intended direction is a general reactor platform
on the event stream, where a reaction can fire the instant a session changes state
rather than on the next poll.

macOS exposes reactive process-exit notification through kqueue `EVFILT_PROC` with
`NOTE_EXIT`. The notification is delivered by the kernel, so it fires even on
SIGKILL, works for non-child same-user processes (the independent `claude`
processes vigil cannot `wait()` on), and binds to the process at registration time,
so it is not subject to PID reuse. Verified 2026-07-06: a kqueue registered on a
live PID blocked and woke in 1.506s at the instant the process was sent SIGKILL,
delivering an event with `NOTE_EXIT` set. The wait returned at the moment of death,
not at a poll boundary and not at the 10s wait timeout.

File writes are observable reactively through `EVFILT_VNODE` with `NOTE_WRITE`,
which allows detecting the Esc interrupt marker in a session's transcript (a `user`
message whose text begins `[Request interrupted by user`) without polling the file.

## Decision

The daemon loop is kqueue-driven. It registers each session's `claude` PID with
`EVFILT_PROC`/`NOTE_EXIT` and each session's transcript with
`EVFILT_VNODE`/`NOTE_WRITE`. It blocks in `kevent()` with a timeout set to the next
housekeeping tick.

- `NOTE_EXIT` releases that session's hold and drops its registration.
- Transcript `NOTE_WRITE` triggers a last-line check for the interrupt marker,
  fail-open: if the marker cannot be read or the format has changed, the session
  falls through to the timeout rather than releasing or holding incorrectly.
- The timeout wakeup handles housekeeping only: power-source poll
  (`POWER_POLL_INTERVAL`), battery hold timers, `caffeinate -t` respawn, and the
  self-exit check. It no longer drives release.

kqueue is accessed through the `kqueue` crate (1.2.0, MIT), which wraps the FFI with
a safe `Watcher` for both `EVFILT_PROC` and `EVFILT_VNODE`. This keeps the flock-era
reasoning consistent: prefer a safe audited wrapper over hand-rolled `unsafe` when
one exists, as with the standard-library file lock that replaced `fs2`/`fs4`. The
standard library provides no kqueue equivalent, so the choice is crate versus raw
`libc` FFI rather than crate versus stdlib.

The idle staleness timeout (ADR-0006) is demoted to a fail-open backstop for the
residue that no hook, liveness probe, or interrupt marker covers.

## Consequences

**Real-time release.** Death and interrupt are handled at the instant they occur.
The measured `NOTE_EXIT` latency was 1.506s in a test whose kill was scheduled at
1.5s, so the wake tracked the death rather than a poll cadence.

**Single wait.** `kevent()` unifies event waiting and the periodic housekeeping tick
in one blocking call, so there is one wakeup source rather than a watcher thread
plus a timer. This resolves the two-wakeup-source concern that argued against file
watching for release in earlier design discussion.

**Foundation for reactors.** A future reactor consumes the same reactive
state transitions (working, awaiting-input, interrupted, dead) without adding its
own polling loop. The wakelock becomes one reactor among possible others.

**SIGKILL without a hook.** `NOTE_EXIT` is kernel-delivered, so the termination
class that fires no hook (SIGKILL, OOM, panic) is still handled reactively, not
only by the timeout backstop.

**Added complexity and one dependency.** The daemon manages per-session kqueue
registrations (register on first hook that carries a PID, drop on `NOTE_EXIT`) and
depends on the `kqueue` crate. The mechanism is macOS-specific, which vigil already
is. Recorded in `ENHANCEMENTS.md` per the dependency policy.

## SPEC impact

To be applied in a dedicated spec-update session:

- "Daemon / Supervisor loop": replace the sleep-poll pseudocode with a
  `kevent`-driven loop (blocking wait with a housekeeping timeout; event handlers
  for `NOTE_EXIT` and transcript `NOTE_WRITE`).
- "Commit-aware timeout" and "Timeouts & Configuration": the idle timeout is a
  fail-open backstop, not the release driver; note the reactive sources and the
  `kqueue` dependency.
- "Lifecycle & Cleanup": release and GC are driven by `NOTE_EXIT` and `SessionEnd`,
  with staleness GC as backstop.
- "Project Structure": add a `watch` module for the kqueue registrations.
- "Dependencies": add `kqueue`.

## References

- `../SPEC.md` sections "Daemon", "Supervisor loop", "Timeouts & Configuration",
  "Project Structure"
- kqueue `EVFILT_PROC`/`NOTE_EXIT` evidence, 2026-07-06 (woke in 1.506s on a
  SIGKILL scheduled at 1.5s)
- `kqueue` crate 1.2.0 (https://crates.io/crates/kqueue)
- ADR-0010 (liveness signal this loop reacts to), ADR-0006 (staleness, demoted to
  backstop), ADR-0007 (daemon lifecycle), ADR-0005 (commit-aware timeout retained
  for the Touch ID hold)
