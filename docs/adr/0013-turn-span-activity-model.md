# ADR-0013: Turn-span activity model

**Status:** Draft

**Date:** 2026-07-13

## Context

The release model to date treats a session as active while the idle time since
its newest log line is under a timeout (ADR-0006, demoted to a backstop by
ADR-0010/0011 but still the freshness signal in `evaluate_sessions`). The timeout
is `STANDARD_TIMEOUT` (120s), or `COMMIT_TIMEOUT` (300s) while the newest line is
an in-flight commit (ADR-0005).

On 2026-07-13 a session on battery slept the display mid-turn. Session
`684e315d-aeb4-4a7b-a63c-404d96fae6fc` produced no hook events for 245s while the
model worked (a burst of reads at 10:36:06, then the next event at 10:40:11). The
120s timeout elapsed at 10:38:07, vigil released, powerd began its 2-minute
display-off grace, and the display turned off at 10:40:17, six seconds after vigil
re-acquired at 10:40:13. The pmset log records the release
(`PID 94896(caffeinate) ClientDied ... 10:38:07`) and the re-acquire
(`PID 1677(caffeinate) Created ... 10:40:13`). A `PreventUserIdleDisplaySleep`
assertion does not cancel a display-off already in progress, so the late
re-acquire did not relight the screen.

The stated goal is a hold that spans a whole turn: from `UserPromptSubmit` until
the turn ends or the session is waiting on the user, with no mid-turn release.

### Verification (2026-07-13)

Whether an incremental activity signal could carry a mid-turn hold was tested
directly.

- **Transcript writes are not a mid-turn signal.** During the incident's 245s hook
  gap the session transcript was also silent: one write at 10:36:06, the next at
  10:40:09 (243s). The earlier gap showed 139s of transcript silence. A headless
  `claude -p` prose session (1500-word essay, no tools) held its transcript at
  18323 bytes for 68s, then wrote the whole body in one append to 35369 bytes at
  completion. The transcript is written at message-block and tool completion, not
  during token streaming, so it goes quiet during the same long thinking or
  generation stretch that produces no hooks.
- **Turn boundaries are reliable.** Clean turn completion fired `Stop` in every
  observed case (two headless sessions, one interactive). `SessionEnd` fired on
  every session close, with `reason:"other"` on normal end and
  `reason:"prompt_input_exit"` on `/exit`.
- **`Notification` reports awaiting-input.** An interactive session showing a
  permission dialog fired `Notification` with
  `{notification_type:"permission_prompt", message:"Claude needs your permission"}`.
  The documented awaiting-input types are `permission_prompt`, `idle_prompt`,
  `agent_needs_input`, and the `elicitation_*` set (Claude Code docs, hooks
  reference, retrieved 2026-07-13).
- **Not reproduced live.** `idle_prompt` did not fire during a 75s wait while a
  permission dialog was already open. `StopFailure` (documented for API errors) was
  not induced. `PermissionRequest` wiring was added after the interactive probe ran.

The finding that no incremental signal survives a long thinking block removes the
option of proving liveness mid-turn. What remains reliable is the turn boundary
(`UserPromptSubmit` start, `Stop`/`StopFailure`/`SessionEnd` end), process
liveness (ADR-0010), and the interrupt marker (ADR-0011).

## Decision

Replace the timeout-driven freshness test with a turn-span activity test. A
session is active when all of the following hold:

- its log exists (the recorder creates it on `UserPromptSubmit` and deletes it on
  `Stop`/`StopFailure`/`SessionEnd`, so an existing log means a turn is in flight),
- its `claude` process is alive (ADR-0010),
- its transcript's newest line is not the interrupt marker (ADR-0011),
- its newest log line is not an awaiting-input `Notification`, and
- its log age is under a long safety cap.

The freshness timeout, the commit-extended timeout, and commit detection are
removed. An in-flight commit is a tool in flight like any other and is held for
the duration of the turn, so the Touch ID sheet is covered without a special case.

### Awaiting-input

`Notification` is wired as a seventh hook event. The recorder appends a line
carrying `notification_type`. A session whose newest line is one of
`permission_prompt`, `agent_needs_input`, or `elicitation_dialog` is treated as
awaiting the user and is not active. The next `PreToolUse` (after approval) or a
new `UserPromptSubmit` becomes the newest line and the session is active again.

Awaiting-input uses a short grace before release (target 90s) rather than
immediate release, so the display does not sleep while the user reads a dialog
they have not yet answered. An unattended session releases roughly 90s after the
prompt.

### Safety cap

A single long absolute cap on log age (target 12h) is the backstop for a turn that
never ends: a `Stop` that did not fire while the process stays alive with no
interrupt marker. No shorter cap is applied, because a long single tool (a build,
a test run) can leave the newest line unchanged for a long time while genuinely
working, and vigil cannot distinguish that from a turn whose `Stop` was missed. The
cap is derived from log age (file state), not held in daemon memory, so it survives
the daemon self-exit and respawn that a multi-turn `/loop` causes.

### Battery floor invariant

The battery cap (ADR-0009) is applied as a veto over the hold decision, not as part
of the activity test. `want_hold = active AND (on_ac OR NOT battery_capped)`. The
turn-span change alters only the `active` term. On battery, once charge reaches
`BATTERY_FLOOR_PCT`, `battery_capped` is set from the live `pmset` read and the hold
is released regardless of how long the turn has been active. A mega-turn held for
hours on battery is released at the floor. The floor re-derives from the current
charge on every poll, so it re-latches after a daemon restart.

The battery-cap decision is extracted into a pure function and unit-tested,
including the case where a session is active, on battery, and at or below the floor,
and the hold is not taken. This locks the invariant against regression during the
turn-span change.

### Scan-time interrupt check

The interrupt marker is checked when a session is first evaluated, alongside the
liveness check, in addition to the existing reactive transcript-write check. Under
the timeout model a missed marker released after 120s. Under turn-span there is no
short timeout, so a marker that is a transcript's last line with no following write
(a daemon that starts after the interrupt) is caught at scan time rather than held
to the safety cap.

## Consequences

**Covers the incident.** Both gaps in session `684e315d` were mid-turn with the log
present, the process alive, and no interrupt marker. Under turn-span the hold spans
the gap.

**Failure direction inverts.** Under the timeout model a missed signal false-releases,
bounded at 120s. Under turn-span a missed end signal false-holds, bounded by the
safety cap on AC and by the battery guards on battery. `Stop` was reliable in every
observed clean end, so the missed-`Stop` case is rare; its cost is a held display
until the user returns (their next input or prompt resolves it) or the cap fires.

**Overnight, one mega-turn.** A single long instruction is one `UserPromptSubmit`
and one `Stop`, held throughout. An un-preauthorized permission prompt releases
(awaiting-input) and the display sleeps until the user returns and approves, which
is the intended behavior for a genuinely blocked run.

**Overnight, many turns (`/loop`).** Each iteration ends with `Stop`, releases, and
the display sleeps during the gap. Once the display sleeps and the lock engages,
Touch ID keychain reads fail until the user returns regardless of vigil, so this
matches the existing headless workflow (unsigned commits between iterations).

**Battery unchanged.** A long turn on battery was already held continuously under
the timeout model (tool events refreshed it), so the floor was already the sole
guard against draining an unattended battery session. It remains so, now enforced
by an explicit invariant and a test.

**Commit apparatus removed.** `COMMIT_TIMEOUT`, `is_unmatched_commit`, and the
git-commit command parser are deleted. ADR-0005 dissolves. The commit hold is a
consequence of turn-span rather than a detection heuristic.

**Deferred.** A shorter cap keyed on the newest event type (a build sitting on
`PreToolUse` held long, a between-tools state held for a medium interval) is not
built. It keys on a reliable signal (event type) rather than the refuted transcript
signal, so it is a viable later refinement, gated on real usage showing that
missed-`Stop` leaks occur. `idle_prompt` and `StopFailure` behavior are confirmed
by documentation but not by live capture.

## SPEC impact

To be applied in a dedicated spec-update session:

- "Reference counting & the staleness backstop" and "Commit-aware timeout": replace
  the timeout-driven activity test with turn-span (log present, alive, not
  interrupted, not awaiting-input, under the safety cap). Remove commit detection.
- "Timeouts & Configuration": remove `STANDARD_TIMEOUT` and `COMMIT_TIMEOUT`; add
  the awaiting-input grace and the safety cap; note the battery-floor invariant.
- "Hook Contract / Wired Events": add `Notification` as a seventh event; record the
  awaiting-input `notification_type` set.
- "Event Log / Line Schema": add `notification_type` (optional).
- "Daemon / Supervisor loop": the activity test changes; the battery veto and the
  self-exit logic are unchanged.
- "CLI status": report per-session state (working, tool-in-flight, awaiting-input).

## References

- `../SPEC.md` sections "Reference counting & the staleness backstop",
  "Commit-aware timeout", "Power source & battery cap", "Hook Contract"
- Incident 2026-07-13, session `684e315d-aeb4-4a7b-a63c-404d96fae6fc`; pmset log
  release 10:38:07, re-acquire 10:40:13, display off 10:40:17
- Verification 2026-07-13: transcript-silence measurement, headless batched-write
  measurement, `Notification` permission_prompt capture
- Claude Code hooks reference (retrieved 2026-07-13): `Notification` types,
  `StopFailure`, `PermissionRequest`
- ADR-0006 (staleness release, superseded by this ADR), ADR-0005 (commit-aware
  timeout, dissolved), ADR-0010 (process liveness, retained), ADR-0011 (reactive
  loop and interrupt marker, retained), ADR-0009 (battery cap, retained as the hold
  veto), ADR-0012 (reactive log-directory watch, retained)
