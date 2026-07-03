# ADR-0008: Hardcoded timeout constants for the first version

**Status:** Draft

**Date:** 2026-07-02

## Context

The daemon has several tunables: the idle release threshold, the commit-extended
threshold, the poll interval, the exit grace, the GC threshold, and the caffeinate
safety cap. A configuration file would allow changing these without recompiling but
adds parsing, a file location and format decision, defaults handling, and a failure
mode when the file is malformed. The owner runs a single machine with a known
display-sleep setting (10 minutes on AC), so the values are stable.

## Decision

The constants are compiled in, collected in a `config` module. No configuration
file in this version. Values are changed by editing the module and rebuilding.
Initial values: `STANDARD_TIMEOUT` 120s, `COMMIT_TIMEOUT` 300s, `POLL_INTERVAL` 2s,
`EXIT_GRACE` 2 polls, `GC_THRESHOLD` 300s, `SAFETY_SECS` 1800s.

## Consequences

**Simplicity.** No config parsing, no invalid-config failure path, one source of
truth for the values. The daemon has no external inputs beyond the event logs.

**Recompile to tune.** Changing a value needs a rebuild. Acceptable for a
single-user tool with a `task build` step, and the values are grounded in the
machine's actual display-sleep timing.

**Revisit trigger.** If the values start needing per-machine or per-run changes, a
configuration file becomes worthwhile. Recorded as a future item, not built now.

## References

- `../SPEC.md` sections "Timeouts & Configuration", "Non-goals / Future"
- Display-sleep timing (`pmset -g`, 2026-07-02: 10 min AC, 2 min battery; lock 5 min
  after display off)
- ADR-0005 (commit timeout value), ADR-0007 (exit grace and safety cap)
