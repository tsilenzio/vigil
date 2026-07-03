# ADR-0004: Thin hooks record raw events; the daemon interprets

**Status:** Draft

**Date:** 2026-07-02

## Context

Commit detection drives the extended timeout (ADR-0005). It could live in the
recorder, which has the hook JSON and could write a derived `committing: true`
flag, or in the daemon, which reads the logged `command`. The recorder runs on
every hook invocation and must stay fast and must never block a turn. The event log
is also intended as a general interface for future reactions beyond caffeinate.

## Decision

The recorder writes raw events only: `event`, and when present `tool`, `command`,
and `agent_id`. It performs no interpretation. The daemon derives all policy,
including commit detection, from the logged fields.

## Consequences

**What this keeps open.** The record path stays generic. A future reactor with
different needs reads the same untransformed events without a schema change driven
by today's caffeinate logic.

**Policy in one place.** Commit detection, timeout selection, and the decision to
hold or release the assertion all live in the daemon, so behavior changes are made
in one module and do not require touching the hook wiring.

**Recorder stays trivial.** The recorder parses stdin JSON, appends one line, and
ensures the daemon. It has no branching on tool contents, which keeps its failure
surface small and its latency low.

## References

- `../SPEC.md` sections "Event Log", "Hook Contract", "Commit-aware timeout"
- ADR-0003 (log schema), ADR-0005 (commit-aware timeout consumes the raw command)
