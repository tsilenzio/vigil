# vigil code analysis

Last Updated: 2026-07-14

Findings from a full read of the repository at commit `c75c62a` on branch
`feat/turn-span-model`: all nine `src/` modules, `docs/SPEC.md`, ADR-0001 through
ADR-0014, `docs/TODO.md`, `docs/ENHANCEMENTS.md`, the README, and the build, CI,
and tooling configs. Line references are against that commit and drift as the code
changes. Entries are removed once addressed, with the removal recorded in the
session notes of the session that closed them.

## Open findings

### AN-004: `pgrep -f` / `pkill -f` substring matching

**Severity:** low.

`pgrep -f "caffeinate -di"` in `status` matches any process whose full command
line contains the substring, including a caffeinate spawned by another tool, so
`assertion held: true` can be a false positive. The decision journal (TODO-003,
built 2026-07-14) reports the daemon's own decision alongside it, which covers
the introspection need. Uninstall's `pkill -f 'vigil daemon'` survives only as
the wedged-daemon fallback behind the disable-flag stand-down (AN-002 fix,
2026-07-14).

### AN-005: battery floor fails open on unparseable pmset output

**Severity:** low. Noted as a failure-direction observation, not a bug.

`read_power` (`src/daemon.rs:326`) falls back to `charge: 100` when the battery
line does not parse, which disables the floor guard that ADR-0009 designates as
the death-prevention mechanism. A pmset output format change would be silent. The
3h `BATTERY_MAX_HOLD` still bounds the hold.

### AN-006: CI cost nits

**Severity:** low.

The conventions workflow runs `cargo install cocogitto --locked` from source on an
uncached ubuntu runner on every PR event, several minutes per run. A cache step or
prebuilt binary reduces it to seconds. Separately, the standalone `cargo check`
job in `ci.yml` is subsumed by the clippy job (clippy performs a full check), so
one of the four macOS runners is redundant.

### AN-007: install paths containing spaces

**Severity:** low. Reachable only via `--dir` or `$VIGIL_INSTALL_DIR` with a
space.

`command_is_vigil` and `hook_bins` (`src/install.rs:301`, `src/install.rs:311`)
split hook commands on whitespace, and the generated hook command does not quote
the binary path. An install root with a space produces hook commands that break in
the shell and that uninstall cannot recognize as vigil's own. The default paths
cannot hit this.

## Checked and found handled

Recorded so future reviews do not re-derive them. Verified at `c75c62a`:

- PID reuse: liveness keyed on `(pid, lstart)` with whitespace normalization for
  single-digit days (`src/proc.rs:73`), tested.
- kqueue changelist hygiene: every registration rolls back its add when `watch()`
  fails, so a dead entry cannot poison later commits (`src/watch.rs:97`).
- Zombie reaping: `Caffeinate::stop` kills then waits, `ensure_running` reaps via
  `try_wait` (`src/caffeinate.rs`).
- Recorder-spawn vs daemon-exit race: absorbed by `EXIT_GRACE`, which advances
  only on housekeeping timeouts (`src/daemon.rs:176`).
- Daemon restart mid-turn: activity state re-derives from log files, and the
  scan-time interrupt check (`src/daemon.rs:221`) runs every pass, so a marker
  written before daemon start releases within a tick even when the transcript
  watch failed to register.
- Self-upgrade handoff: flock dropped before spawning the successor
  (`src/daemon.rs:149`), a hook-spawned successor is equally valid.
- Settings edits: shape-validated read, surgical strip/insert round-trip tested,
  atomic temp+rename writes, backups pruned to two (`src/install.rs`).
- Battery invariants: floor vetoes an active hold, `hold_since` accrues battery
  time only, each unplug starts a fresh budget. Pure functions with tests
  (`src/daemon.rs:356-387`).
