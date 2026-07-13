# ADR-0014: Safe binary replacement and daemon self-upgrade

**Status:** Draft

**Date:** 2026-07-13

## Context

`vigil install --force` copied the new binary over the installed path with
`fs::copy`, which opens the destination with truncate and rewrites its bytes in
place. On 2026-07-13, running `install --force` while the daemon was executing
that path produced a binary that was SIGKILLed on every exec (observed exit 137).
macOS invalidates the code signature of a Mach-O whose bytes are rewritten while a
process maps it, and the kernel kills the next exec. Recovery required removing the
file first (`rm` then copy) to force a fresh inode.

Two separate problems sit behind this:

1. **Corruption.** Overwriting the running binary in place invalidates its
   signature. This is a property of the write technique, independent of the daemon.
2. **Freshness.** Even after a safe replacement, the old daemon keeps executing its
   old inode until it restarts, so new code is not live until then.

A third concern is coordination during an upgrade: in the lazy-spawn model a hook
can spawn a daemon at any instant, so any "stop the daemon, then swap the binary"
sequence has a window where a fresh old-binary daemon can start mid-swap.

External installers behave differently. Homebrew installs each version into a new
Cellar directory and repoints `/opt/homebrew/bin/vigil`, leaving the running file
untouched (no corruption, but the launch-path inode does not change). `cargo
install` stages the build and renames it into `~/.cargo/bin/vigil`, which is
corruption-safe on Unix and does change the inode at a fixed path.

## Decision

Three mechanisms, applied in order.

### 1. Atomic rename on install

`copy_self` writes the new binary to a temporary file in the destination directory,
sets its mode, then `fs::rename`s it over the destination. `rename` repoints the
directory entry to a new inode; it never rewrites the bytes of the inode a running
process holds. The old daemon keeps executing its old (now-unlinked) inode with a
valid signature, and any new exec of the path resolves to the new inode. The
temporary must share the destination's filesystem for the rename to be atomic. This
removes corruption as a possibility regardless of whether the daemon is running, so
it is the floor the other two mechanisms build on.

### 2. Daemon self-upgrade via inode change

At startup the daemon records the `(device, inode)` of its own executable
(`current_exe()`). On the slow-housekeeping cadence (`POWER_POLL_INTERVAL`) it
re-stats that path. When the pair differs, a new binary has been installed at the
path, so the daemon stops its caffeinate, releases its lock, spawns a fresh detached
daemon (which execs the new inode), and exits. The inode change is the version
signal; no version directories or version strings are involved.

The handoff releases the lock before spawning, so the new daemon acquires it
immediately rather than racing the old daemon's exit. A hook-spawned daemon that
wins the lock first is an equally valid new-binary daemon, so the handoff is
self-healing.

Coverage by installer: native `install --force` (mechanism 1) and `cargo install`
both land a new inode at a fixed path and are caught. Homebrew repoints a symlink
and keeps the old Cellar inode, so the launch-path inode does not change and
auto-upgrade does not fire; freshness there comes from the daemon's idle-restart or
an explicit re-run of `vigil install`. Watching the front-door symlink to catch the
Homebrew case is a possible later refinement.

### 3. Reactive disable flag

A sentinel file (`${VIGIL_RUNTIME_DIR}/.disabled`) disables the daemon. The daemon
checks for it at startup (so a daemon spawned while disabled exits at once) and on
each loop pass. Because the flag lives in the log directory the daemon already
watches with `EVFILT_VNODE`/`NOTE_WRITE` (ADR-0012), creating it wakes the daemon
reactively, and it clean-exits (killing its caffeinate) rather than being signaled.
Removing the flag lets the next hook spawn a daemon again. This is a manual off
switch, separate from the upgrade path, which mechanism 2 handles on its own.

## Consequences

**Corruption removed.** Mechanism 1 makes `install --force` safe whether or not the
daemon runs, and matches the atomic-write pattern already used for `settings.json`.

**Upgrades go live on their own for the fixed-path installers.** Mechanism 2 makes a
native or cargo upgrade take effect within `POWER_POLL_INTERVAL` without a manual
restart, and keeps the lazy-spawn model (install does not orchestrate the daemon).

**Clean shutdown path.** Mechanisms 2 and 3 both exit through the daemon's own
cleanup (caffeinate stopped, lock released), so neither orphans a caffeinate the way
an external SIGTERM does. This also addresses the orphaned-caffeinate failure mode
of a plain `pkill`.

**Homebrew is safe but not auto-fresh.** A `brew upgrade` cannot corrupt the running
binary, but the daemon does not auto-restart on it. Documented as a nudge
(`vigil install` or a natural idle-restart), with the symlink-watch refinement left
open.

**Disable flag is sticky until removed.** While `.disabled` exists, every spawned
daemon exits at startup, so vigil holds nothing. Removing the file restores normal
operation on the next hook. A stranded flag does not survive a reboot, since the
runtime dir is under `/tmp`.

## SPEC impact

To be applied in a dedicated spec-update session:

- "Single-instance (flock)" / "Self-exit & crash recovery": add the inode-change
  self-upgrade handoff and the `.disabled` flag as daemon exit paths.
- "CLI Interface" / install: note that `install` replaces the binary via atomic
  rename.
- "Timeouts & Configuration": the binary-change check runs on the
  `POWER_POLL_INTERVAL` cadence.

## References

- `../SPEC.md` sections "Daemon", "Single-instance (flock)", "CLI Interface"
- Incident 2026-07-13: `install --force` over a running daemon produced a
  SIGKILLed binary (exit 137); recovered by removing the file first
- Homebrew Cellar/symlink layout; `cargo install` rename-into-place on Unix
- ADR-0007 (daemon lifecycle and lazy spawn), ADR-0012 (directory watch reused for
  the disable flag), ADR-0002 (single daemon-owned caffeinate)
