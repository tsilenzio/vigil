# ADR-0005: Commit-aware timeout extension via last-line inspection

**Status:** Draft

**Date:** 2026-07-02

## Context

A `git commit` presents a Touch ID sheet and blocks the tool call. If the sheet
times out, `pinentry-touchid` falls back to a password dialog that waits
indefinitely. While blocked, no further tool events fire, so a uniform idle timeout
would treat the wait as idle and release the assertion, which is exactly when the
display must stay awake.

The `PreToolUse` for the commit is written before the tool runs, and its
`PostToolUse` cannot fire until the commit returns. So during the entire blocked
wait, the session's newest log line remains the commit `PreToolUse`.

## Decision

The daemon applies a longer `COMMIT_TIMEOUT` (300s) to a session whose newest log
line is an in-flight commit, and the shorter `STANDARD_TIMEOUT` (120s) otherwise.
An in-flight commit is a newest line with `event == PreToolUse`, `tool == Bash`, and
`command` matching a git-commit invocation. When `PostToolUse` arrives, the newest
line changes and the session reverts to the standard timeout.

## Consequences

**What this covers.** The display stays awake through the Touch ID sheet and any
password-fallback entry, closing the blocked-commit gap that a plain heartbeat
leaves open.

**Detection is heuristic.** The recommended regex matches `git commit`, `git -C
<dir> commit`, and `git ... && git commit`, and rejects `git log`. A commit run
through a shell alias is not detected; the raw logged command is the only input.

**Bounded even on abandonment.** If the user Escs during the blocked commit, no
`PostToolUse` fires, but the log goes stale and releases after `COMMIT_TIMEOUT`
rather than holding indefinitely.

## References

- `../SPEC.md` sections "Commit-aware timeout", "Timeouts & Configuration"
- ADR-0004 (daemon reads the raw command), ADR-0006 (staleness release bounds the
  abandoned-commit case)
