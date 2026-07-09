# ADR-0012: Reactive log-directory watch for acquire and lifecycle release

**Status:** Draft

**Date:** 2026-07-09

## Context

ADR-0011 made the daemon a kqueue event loop with two reactive sources:
`EVFILT_PROC`/`NOTE_EXIT` for process death, and `EVFILT_VNODE`/`NOTE_WRITE` on each
transcript for the Esc-interrupt marker. Two session-lifecycle transitions were
left on the housekeeping tick: acquire (a new turn creates a session log) and
release on `Stop`/`SessionEnd` (the recorder deletes the log). The daemon receives
no kqueue event for either, because it does not watch the log directory, so it
noticed them only on the next tick, within `POLL_INTERVAL` (2s).

This session settled a principle for when to make a path reactive: use an
event-driven watch where the triggering event is rare or must be read at its
moment, and leave a periodic check where the trigger is constant and the latency
is immaterial. Process death is rare and fires once, so `EVFILT_PROC` earns its
watch. The Esc marker is rare among transcript writes but must be read when it is
written, since a later sample can find it superseded by a subsequent prompt, so the
transcript `EVFILT_VNODE` earns its watch. By the same test, a plain 2s tick was
kept for the staleness backstop, which detects the absence of any signal and cannot
be pushed reactively.

Log-directory changes fit the reactive case: a log created or deleted is rare, and
a directory `NOTE_WRITE` fires on entry create/delete, not on the appends to the
files inside it, so ordinary hook activity does not storm it. Verified 2026-07-09
with a `select.kqueue` probe: append to a file inside the directory did not fire;
creating and deleting a file did. The `kqueue` crate is edge-triggered by default
(`clear: true`), so a directory event does not re-fire in a busy loop.

## Decision

The daemon registers the `/tmp/vigil` log directory with `EVFILT_VNODE`/
`NOTE_WRITE`. A directory event (a session log created or deleted) wakes the daemon
to re-evaluate at once, making acquire and `Stop`/`SessionEnd` release reactive
rather than tick-latent. The housekeeping tick is retained for the staleness
backstop, power polling, battery timers, `caffeinate` respawn, and self-exit.

## Consequences

**Reactive acquire and lifecycle release.** A new log wakes the daemon to acquire;
a deleted log wakes it to release. Measured 2026-07-09: acquire 0.042s, `Stop`/
`SessionEnd` release 0.043s, against up to 2s on the tick.

**No storm, no busy-loop.** The directory watch fires on create/delete only, not on
the recorder's appends, and edge-triggering prevents re-fire. The daemon's own log
deletions wake it once to recompute, which converges.

**Acquire has a microsecond empty window.** The recorder creates a log then writes
its first line; a directory-driven evaluate landing between the two sees an empty
log and waits for the next tick. Negligible in the recorder, where create and write
are back-to-back syscalls, and bounded by the tick.

**The tick is not removed.** Staleness, power, battery, and self-exit still need a
periodic pass, so this removes tick latency from two paths rather than eliminating
the tick.

## SPEC impact

To be applied in a dedicated spec-update session:

- "Daemon / Supervisor loop": add the log-directory watch as a third reactive
  source; note that acquire and `Stop`/`SessionEnd` release are reactive.
- "Project Structure": the `watch` module gains `watch_dir` and the loop a
  `Wake::Dir` arm.

## References

- `../SPEC.md` sections "Daemon", "Supervisor loop"
- Directory `EVFILT_VNODE` probe, 2026-07-09 (append: no fire; create/delete: fire)
- ADR-0011 (reactive kqueue loop this extends), ADR-0006 (staleness backstop the
  tick still serves)
- Promoted from `../ENHANCEMENTS.md` ENH-003, which recorded this before promotion
