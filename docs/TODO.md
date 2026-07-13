# vigil TODO

Last Updated: 2026-07-13

Planned work and deferred decisions.

## TODO-002: Turn-span activity model (phase 1)

**Status:** Planned. Design in ADR-0013. Supersedes the timeout-driven release
from ADR-0006 and dissolves the commit-aware timeout from ADR-0005. Motivated by
the 2026-07-13 mid-turn display-sleep incident (session `684e315d`), where a 245s
gap with no hook events elapsed the 120s timeout and released the hold.

Replace the daemon's activity test with turn-span: a session is active while its
log exists, its process is alive, its transcript's newest line is not the interrupt
marker, its newest log line is not an awaiting-input `Notification`, and its log age
is under a long safety cap. Verification on 2026-07-13 established that no
incremental signal (hooks or transcript) survives a long thinking block, so the
turn boundary is the only reliable span marker (full evidence in ADR-0013).

### Phase 1 work

- **Recorder / event schema.** Wire `Notification` as a seventh hook event; append a
  line carrying `notification_type`. Add the optional `notification_type` field to
  the `Event` schema (`event.rs`). Keep `Stop`/`StopFailure`/`SessionEnd` as
  log-deleting terminal events.
- **Activity test.** Rewrite the freshness check in `evaluate_sessions` (`daemon.rs`)
  to turn-span. A session whose newest line is one of `permission_prompt`,
  `agent_needs_input`, `elicitation_dialog` is awaiting-input and not active, with a
  ~90s grace before release.
- **Remove the commit apparatus.** Delete `COMMIT_TIMEOUT`, `STANDARD_TIMEOUT`,
  `is_unmatched_commit`, `is_commit_command`, `is_git_commit_segment`, and their
  tests. The commit hold falls out of turn-span.
- **Safety cap.** A single long absolute cap on log age (target 12h), derived from
  file state so it survives daemon restarts. Replaces `GC_THRESHOLD` as the primary
  backstop, or sits alongside it.
- **Scan-time interrupt check.** Check the interrupt marker when a session is first
  evaluated, alongside the liveness check, so a missed marker is caught at scan time
  rather than held to the safety cap.
- **Battery-floor invariant.** Keep the battery cap a veto over the hold decision
  (`want_hold = active AND (on_ac OR NOT battery_capped)`). Extract the cap decision
  into a pure function and unit-test the active-on-battery-at-floor release case, so
  the invariant cannot silently regress. This extraction also serves TODO-001.
- **status.** Report per-session state (working, tool-in-flight, awaiting-input).

### Acceptance

- Unit tests: turn-span activity given a synthetic newest line (each state),
  awaiting-input release, battery-floor veto while active, safety-cap release.
- `cargo test`, `clippy -D warnings`, `fmt --check` green.
- Manual: re-run the incident shape (a turn with a >120s tool-free gap) and confirm
  the hold spans it; confirm a permission prompt releases within the grace; confirm
  on battery at the floor the hold releases mid-turn.

### Deferred to phase 2 (only if warranted)

- A shorter cap keyed on the newest event type (long for `PreToolUse` tool-in-flight,
  medium for between-tools states), gated on real usage showing missed-`Stop` leaks.
- Live confirmation of `idle_prompt` and `StopFailure` (documented, not captured).

### Related

- ADR-0013 (`adr/0013-turn-span-activity-model.md`)
- ADR-0006 (superseded), ADR-0005 (dissolved), ADR-0009 (battery cap, retained)

## TODO-001: Graduated battery response (charge-tiered max-hold)

**Status:** Deferred. v1 ships the two-guard form (floor + single max-hold) from
SPEC "Power source & battery cap" and ADR-0009.

Generalize the battery logic from two guards into a charge-tiered `max-hold` step
function: be generous with the hold when the battery is full, progressively
stingier as it drains, and off at the floor. The v1 floor plus single max-hold is a
two-tier instance of this, so this is an extension, not a rewrite.

### Model

An ordered table of `(min_charge_pct, max_hold_secs)` tiers, highest charge first.
Each poll, look up the tier for the current charge and use its `max_hold` in the
existing continuous-hold comparison. The floor is the terminal tier with
`max_hold = 0` (never hold). Example tiers (adjust to taste):

```
>= 75%   -> 3h
50-75%   ->  1h
35-50%   ->  5m
<  35%   ->  0   (off; the floor)
```

### Behavior notes

- The tier is re-evaluated every poll against current charge, and compared to
  `now - hold_since`. As charge falls into a stingier tier, the shrinking limit can
  trip a release on an ongoing hold. That is the intended graduated behavior.
- `hold_since` still marks the continuous hold period and is not reset by a
  `caffeinate -t` respawn, same as v1.
- On AC, no tiers apply (no cap), same as v1.
- The latch and its reset (AC or daemon idle-exit) are unchanged.

### Work

- Replace `BATTERY_FLOOR_PCT` and `BATTERY_MAX_HOLD` with the tier table in
  `config.rs` (keep the floor as the terminal `max_hold = 0` tier).
- Update the daemon's battery-cap block to look up the tier and compare.
- Add scenario tests crossing each boundary (75, 50, 35) during a continuous hold.
- Update SPEC "Power source & battery cap", "Timeouts & Configuration", the test
  matrix, and ADR-0009 (or a superseding ADR) when built.

### Related

- SPEC `SPEC.md` "Power source & battery cap"
- ADR-0009 (`adr/0009-battery-aware-hold-cap.md`)
