# vigil TODO

Last Updated: 2026-07-02

Planned work and deferred decisions.

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
