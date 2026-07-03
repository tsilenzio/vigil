# ADR-0007: Daemon lifecycle, hook-spawned and self-exiting

**Status:** Draft

**Date:** 2026-07-02

## Context

The daemon is needed only while sessions are active, which is bursty. It could be a
persistent `launchd` agent (a managed, always-resident service) or a process
spawned on demand. The work also happens on `/Volumes/Code` and a laptop that
sleeps and reboots, so a resident service adds lifecycle that the workload does not
require. The daemon must be guaranteed single-instance and must survive a recorder
process exiting, and its owned `caffeinate` must not leak if the daemon is killed.

## Decision

The daemon is spawned on demand by the recorder, detached (new session via
`setsid`, stdio to `/dev/null`), and not waited on. Single-instance is enforced by
an advisory `flock` on `/tmp/vigil/daemon.lock`. The daemon self-exits after
`EXIT_GRACE` consecutive idle polls, killing its `caffeinate` child first. Crash
recovery is the `caffeinate -t SAFETY_SECS` cap plus the next recorder respawning a
fresh daemon. No `launchd` agent.

## Consequences

**Self-contained.** No install step beyond the hook wiring, no always-on resident
process, nothing to register or unregister. The daemon exists exactly when work is
happening.

**Race handling.** A recorder can append a fresh line and spawn a daemon just as the
running daemon decides to exit. `EXIT_GRACE` (two polls, ~4s) gives the fresh line
time to be seen before exit, and the flock makes the redundant spawn a no-op.

**Crash bound.** If the daemon is SIGKILLed without cleanup, its `caffeinate`
self-expires within `SAFETY_SECS` (30 minutes) rather than leaking until reboot, and
the next event restarts the daemon.

**Detachment requirement.** The daemon must not die with the recorder. `setsid` plus
null stdio detaches it; observed in v1 that a `nohup`-detached child survives the
hook process.

## References

- `../SPEC.md` sections "Single-instance (flock)", "Self-exit & crash recovery",
  "CLI Interface"
- v1 detachment observation, 2026-07-02
- ADR-0002 (single-instance caffeinate), ADR-0006 (idle detection drives exit)
