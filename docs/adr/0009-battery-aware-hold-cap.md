# ADR-0009: Battery-aware caffeinate hold cap

**Status:** Draft

**Date:** 2026-07-02

## Context

The host is an Apple Silicon MacBook that runs on battery as well as AC. Holding a
display-awake assertion is convenience for Touch ID signing, not a reason to drain
the battery on a long unattended battery session. The stated primary goal is that
vigil never lets the laptop die.

Three guard signals were considered: a maximum continuous hold time, an absolute
charge floor, and a percentage drop from the charge level at hold start (release
after the battery falls a fixed number of points, optionally OR'd with a floor).
The distinction that settled it: "do not let it die" is a statement about absolute
charge. A time cap and a drop budget both limit waste, but neither guarantees
survival. Starting a hold at 15% with a 20-point drop budget would run the battery
to zero. Only an absolute floor prevents death.

macOS exposes both signals through `pmset -g ps`: the first line names the source
(`AC Power` / `Battery Power`), and the battery line carries the charge percentage.

## Decision

The daemon reads the power source and charge level (`pmset -g ps`, cached and
refreshed every `POWER_POLL_INTERVAL` = 30s). On AC there is no cap. On battery,
two guards are applied, OR'd, whichever fires first:

- **Floor (primary):** release once charge reaches `BATTERY_FLOOR_PCT` (default 35,
  set conservatively for v1 as a stand-in for the deferred graduated response).
- **Max hold (secondary):** release once the continuous hold period exceeds
  `BATTERY_MAX_HOLD` (default 10800s), for the case where charge is still well above
  the floor.

Either trigger latches `battery_capped`, which clears when power returns to AC or
when the daemon goes idle and exits. `hold_since` marks the start of the hold period
and is not reset when the `caffeinate` child respawns on its `-t` safety expiry, so
the max-hold guard measures true continuous holding. The percentage-drop guard is
deferred.

## Consequences

**What this guarantees.** vigil stops holding the display awake at the floor, so a
long or unattended battery session cannot be driven to death by vigil. Charge only
falls on battery, so the floor is monotonic and needs no hysteresis.

**Accepted tradeoff.** A `git commit` attempted while capped and still on battery
can fail until AC is reconnected. Battery survival takes priority over signing
convenience once a guard trips.

**Why the drop was deferred.** A percentage-drop budget adds state (the charge at
hold start) and a third dimension without protecting against death better than the
floor. Its only unique property is bounding vigil's marginal cost per hold, which
floor plus max-hold already approximate. It can be added later if the pair proves
insufficient.

**Graduated response deferred.** A charge-tiered `max-hold` step function (be
generous when full, progressively stingier as charge falls, off at the floor) was
proposed. The current floor plus single max-hold is a two-tier instance of it, so
adding tiers is a clean later extension rather than a rework. Deferred to
`TODO.md` (TODO-001); v1 keeps the two-guard form with a conservative 35%
floor.

**Decoupled from the safety cap.** The `-t SAFETY_SECS` respawn (ADR-0007) operates
at the OS-process level and must not reset `hold_since`, or the max-hold guard would
never fire on a hold longer than 30 minutes.

## References

- `../SPEC.md` sections "Power source & battery cap", "Timeouts & Configuration",
  "Implementation Notes" (power source detection)
- `pmset -g ps` output, 2026-07-02 (on battery, 94%, 2:33 remaining)
- ADR-0002 (single daemon-owned caffeinate), ADR-0007 (safety `-t` and lifecycle)
