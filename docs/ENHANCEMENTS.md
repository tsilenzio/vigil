# vigil ENHANCEMENTS

Last Updated: 2026-07-15

Implementation choices that extend or diverge from the SPEC. Each entry records the
change, how it relates to the spec, and how to reverse it.

## ENH-001: File locking via the standard library instead of fs2/fs4

**Spec relation:** SPEC "Project Structure" names `fs2` (or an equivalent flock)
as a dependency.

The single-instance daemon lock uses `std::fs::File::try_lock()` (stabilized in
Rust 1.89; the toolchain here is 1.95) rather than the `fs2`/`fs4` crate the SPEC
named. std provides the same advisory `flock(2)`-backed exclusive lock with the
same release-on-drop semantics, so the crate was redundant. Neither approach uses
`unsafe`.

**Reversibility:** re-add `fs4` and call `FileExt::try_lock`; the lock is confined
to `daemon::run`.

## ENH-002: kqueue crate for reactive session-death detection

**Spec relation:** extends the daemon design per ADR-0010 and ADR-0011 (reactive
liveness replaces staleness as the primary release signal).

Added `kqueue = "1.2.0"` to register each session's `claude` PID with
`EVFILT_PROC`/`NOTE_EXIT`, so a process exit, including SIGKILL which fires no
hook, releases the assertion reactively rather than on a timeout. Measured release
latency was 0.02s on a SIGKILL in testing on 2026-07-08. The crate wraps the
`kevent` FFI with a safe `Watcher`, keeping the unsafe inside an audited crate;
the standard library has no kqueue equivalent, so the choice was crate versus raw
`libc` FFI.

**Reversibility:** the kqueue use is isolated in `src/watch.rs`; it could be
replaced with raw `libc` `kevent` FFI, or the daemon could rely on the staleness
backstop alone, without touching other modules.

## ENH-003: Reactive log-directory watch (promoted to ADR-0012)

Promoted to a first-class architecture decision on 2026-07-09. Full record in
`adr/0012-reactive-log-directory-watch.md`.

## ENH-004: `install`/`uninstall` subcommands wiring the hooks programmatically

**Spec relation:** SPEC "Hook Contract / settings.json wiring" describes wiring the
six events by hand, by absolute path. SPEC "CLI Interface" defines `record`,
`daemon`, and `status`. This adds programmatic install as new subcommands, and
leaves the hot-path commands unchanged. Landed in commit `1f7d1d6`, `src/install.rs`.

`vigil install` copies the running binary to `${install_dir}/bin/vigil` with a
`~/.local/bin/vigil` symlink, and merges one hook group per event into
`~/.claude/settings.json`. `vigil uninstall` reverses both. Bare `vigil` with no
subcommand runs a health check: a consistent install prints its state and does
nothing, an absent or partial one offers to install or repair. Install detection
covers all six hooks pointing at one path plus the binary present at that path, so
a partial install repairs to the location the existing hooks reference rather than
the current default.

Settings edits are surgical. A vigil hook entry is matched by program basename
`vigil` invoked as `record`, so only vigil's own entries are added or removed and
other hooks (for example a `log-event.sh` logger) are preserved. Uninstall strips
from the live file and never restores a backup, so edits made after install
survive. Writes are atomic (temp file then rename) and back up the prior file to
`settings.json.bak-<ts>`, pruned to the two most recent. Verified end-to-end on
2026-07-09 against a sandboxed `CLAUDE_CONFIG_DIR`: fresh install preserving an
external hook and an unrelated key, idempotent re-run with no rewrite, repair of a
manually removed hook and symlink, and uninstall restoring the file to its
pre-install content.

Two supporting changes rode in the same commit. `serde_json` gained the
`preserve_order` feature (adds `indexmap`) so an edit does not reorder the user's
settings keys into a large diff. `main` resets `SIGPIPE` to `SIG_DFL`, since Rust
ignores it at startup and a closed stdout (`vigil status | head`) otherwise panics
with EPIPE.

**Reversibility:** `install.rs` is self-contained and the two subcommands are
additive. Removing the module and the `Install`/`Uninstall` variants reverts to
wiring the hooks by hand per SPEC. The `preserve_order` feature and the
`SIGPIPE` reset are independent one-line changes.

**Promoted:** described in SPEC "CLI Interface / install / uninstall" since the
2026-07-14 spec reconciliation, which also folded in the ADR-0014 install modes
and the disable-flag stand-down. This entry stays as the original rationale and
verification record.

## ENH-005: Environment overrides for path locations

**Spec relation:** ADR-0008 compiles constants in, "values change by rebuilding,"
and SPEC "Event Log / Location" hardcodes `/tmp/vigil`. The timeout constants stay
compiled in; this makes only the path locations overridable, so an install can be
relocated or sandboxed without a recompile. Landed in commit `1f7d1d6`,
`src/config.rs`.

The path resolvers read, in order, an explicit override then a default:
`$VIGIL_RUNTIME_DIR` for the session-log and lock directory (default `/tmp/vigil`),
`$VIGIL_INSTALL_DIR` for the binary (default `${XDG_DATA_HOME:-~/.local/share}/vigil`),
and `$CLAUDE_CONFIG_DIR` for the settings directory (default `~/.claude`).
`$CLAUDE_CONFIG_DIR` is Claude Code's own override, confirmed present in the
installed `claude` binary (`2.1.205`) on 2026-07-09. The install/uninstall tests
drive all three to sandbox the run.

**Reversibility:** the reads are confined to the path functions in `config.rs`.
Reverting each `env::var_os(...).unwrap_or_else(...)` to its constant restores the
compiled-in paths without touching callers.

**Promoted:** recorded in SPEC "Timeouts & Configuration" since the 2026-07-14
spec reconciliation. This entry stays as the original rationale record.
