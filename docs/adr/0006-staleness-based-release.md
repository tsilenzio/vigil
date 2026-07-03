# ADR-0006: Staleness-based release and log garbage collection

**Status:** Draft

**Date:** 2026-07-02

## Context

Claude Code emits no hook when the user presses Esc to interrupt a streaming
response. This is documented (Stop hooks "do not fire on user interrupts") and
confirmed by testing: after an Esc, the v1 caffeinate stayed alive and was reused by
the next turn. `StopFailure` fires only on API errors, not interrupts. There is an
open upstream request for an interrupt hook (anthropics/claude-code#9516), but none
exists today.

A release scheme that waits for a stop event therefore cannot cover Esc. The
supervisor must detect "no longer active" without an explicit signal.

## Decision

Release is driven by staleness. A session is active while the idle time since its
newest log line is under the applicable timeout. The daemon holds the assertion
while any session is active and releases when none is. `Stop`, `StopFailure`, and
`SessionEnd` additionally delete the session log through the recorder. Logs whose
newest line exceeds `GC_THRESHOLD` are deleted by the daemon.

## Consequences

**What this covers.** Esc, crashes, and any other silent end are handled the same as
ordinary idle: activity stops, the log goes stale, the assertion releases. No
interrupt hook is required.

**Release latency.** After genuine idle the assertion is held up to the timeout
before release, and the OS display-sleep timer then runs. With `STANDARD_TIMEOUT`
at 120s and AC display sleep at 10 minutes, this is well within normal behavior.

**Leftover cleanup.** Deletion on stop events is the common path; GC of stale logs
handles the Esc path where no deletion event fires. Reboot clears `/tmp` as the
final backstop.

## References

- `../SPEC.md` sections "Lifecycle & Cleanup", "Reference counting & staleness"
- Esc hook-behavior test, 2026-07-02; anthropics/claude-code#9516
- ADR-0001 (supervisor architecture), ADR-0005 (commit timeout also bounded by
  staleness)
