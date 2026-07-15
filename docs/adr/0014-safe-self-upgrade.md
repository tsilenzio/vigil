# ADR-0014: Install modes, safe binary replacement, and daemon self-upgrade

**Status:** Draft

**Date:** 2026-07-13

## Context

`vigil install --force` copied the new binary over the installed path with
`fs::copy`, which opens the destination with truncate and rewrites its bytes in
place. On 2026-07-13, running `install --force` while the daemon was executing
that path produced a binary that was SIGKILLed on every exec (observed exit 137).
macOS invalidates the code signature of a Mach-O whose bytes are rewritten while a
process maps it, and the kernel kills the next exec. Recovery required removing the
file first to force a fresh inode.

Three problems sit behind this and the broader upgrade story:

1. **Corruption.** Overwriting the running binary in place invalidates its
   signature. A property of the write technique, independent of the daemon.
2. **Freshness.** After a safe replacement, the old daemon keeps executing its old
   inode until it restarts, so new code is not live until then.
3. **Distribution.** vigil should install from a local build, from `cargo install`
   (which places the binary at `${CARGO_HOME:-~/.cargo}/bin/vigil`), or from
   Homebrew (which places each version under `<prefix>/Cellar/vigil/<version>/…` and
   points `<prefix>/bin/vigil` at it). The binary lands in a different, and for
   Homebrew a versioned, location per channel.

## Decision

### Install modes

`vigil install` classifies where it is running from and wires the hooks at the
appropriate stable path, rather than always copying to one location:

- **Homebrew** when `current_exe()` resolves under a `/Cellar/` path. The hooks are
  wired at the stable front door `<prefix>/bin/vigil` (the part of the path before
  `/Cellar/`, plus `bin/vigil`), and no copy is made. Homebrew owns the binary.
- **cargo** when `current_exe()` is `${CARGO_HOME:-~/.cargo}/bin/vigil`. The hooks
  are wired there, no copy. cargo owns the binary.
- **own-copy** otherwise (a `target/` build artifact, a manually placed binary, or
  an explicit `--dir` / `$VIGIL_INSTALL_DIR`). The binary is copied to
  `${VIGIL_INSTALL_DIR:-~/.local/share/vigil}/bin/vigil` with a `~/.local/bin/vigil`
  PATH symlink, and the hooks are wired there. This is the pre-existing behavior.

An explicit `--dir` or `$VIGIL_INSTALL_DIR` forces own-copy. For the managed modes,
the binary already sits on a stable, on-`PATH` path, so vigil neither copies it nor
creates its own symlink, and `vigil uninstall` removes only the hooks and runtime
state, leaving the binary for `cargo uninstall` / `brew uninstall`.

### 1. Atomic rename on install (own-copy)

`copy_self` writes the new binary to a temporary file in the destination directory,
sets its mode, then `fs::rename`s it over the destination. `rename` repoints the
directory entry to a new inode; it never rewrites the bytes of the inode a running
process holds. The old daemon keeps executing its old (now-unlinked) inode with a
valid signature, and any new exec of the path resolves to the new inode. The
temporary must share the destination's filesystem for the rename to be atomic. This
removes corruption as a possibility regardless of whether the daemon runs, and is
the floor the other mechanisms build on. It applies to the own-copy path; Homebrew
and cargo install their own binaries and never overwrite a running file in place.

### 2. Daemon self-upgrade via inode change

At startup the daemon computes its watch path: the Homebrew front door if it is
running from a Cellar, otherwise `current_exe()`. It records the `(device, inode)`
that path resolves to (`fs::metadata` follows symlinks) and re-stats it on the
slow-housekeeping cadence (`POWER_POLL_INTERVAL`). A changed pair means a new binary
is in place, so the daemon stops its caffeinate, releases its lock, spawns a fresh
detached daemon, and exits; the new daemon acquires the lock and re-acquires the
hold. The lock is released before the spawn so the successor acquires it without
racing the old daemon's exit, and a hook-spawned successor is equally valid.

Following symlinks on the watch path unifies all three channels. own-copy and cargo
replace a real file, so the resolved inode changes on `install --force` /
`cargo install`. Homebrew repoints the `<prefix>/bin/vigil` symlink, so re-resolving
the front door yields the new Cellar inode. In every case the inode the watch path
resolves to changes, and no version string is involved.

### 3. Reactive disable flag

A sentinel file (`${VIGIL_RUNTIME_DIR}/.disabled`) disables the daemon. The daemon
checks for it at startup (so a daemon spawned while disabled exits at once) and on
each loop pass. Because the flag lives in the log directory the daemon already
watches with `EVFILT_VNODE`/`NOTE_WRITE` (ADR-0012), creating it wakes the daemon
reactively, and it clean-exits (killing its caffeinate) rather than being signaled.
Removing the flag lets the next hook spawn a daemon again. This is a manual off
switch, separate from the upgrade path.

## Consequences

**Corruption removed.** Mechanism 1 makes own-copy `install --force` safe whether or
not the daemon runs, matching the atomic-write pattern already used for
`settings.json`. Homebrew and cargo do not overwrite in place, so they are safe by
construction.

**Native package integration.** Hooks point at the package manager's own path for
cargo and Homebrew, so a `cargo install` upgrade or a `brew upgrade` is picked up by
the self-upgrade watch without a manual step, and uninstalling vigil does not delete
a package-managed binary.

**Upgrades go live on their own.** Mechanism 2 applies the new binary within
`POWER_POLL_INTERVAL` of the replacement across all three channels, and the daemon
still spawns lazily (install does not orchestrate the daemon).

**Clean shutdown path.** Mechanisms 2 and 3 exit through the daemon's own cleanup
(caffeinate stopped, lock released), so neither orphans a caffeinate the way an
external SIGTERM does. This also addresses the orphaned-caffeinate failure mode of a
plain `pkill`.

**Homebrew end-to-end is unverified until a tap exists.** The Cellar path detection
and front-door derivation are unit-tested against synthetic paths, but no crate or
tap is published yet, so a real `brew install`/`brew upgrade` cannot be exercised.
Until then a Homebrew user falls back safely to own-copy (vigil copies the binary
out of the Cellar), which functions but is not native.

**Disable flag is sticky until removed.** While `.disabled` exists, every spawned
daemon exits at startup. Removing the file restores normal operation on the next
hook. A stranded flag does not survive a reboot, since the runtime dir is under
`/tmp`.

## SPEC impact

Applied to `SPEC.md` in the 2026-07-14 spec-update session:

- "CLI Interface" / install: describe the three install modes and their hook
  targets; note atomic-rename replacement for own-copy.
- "Single-instance (flock)" / "Self-exit & crash recovery": add the inode-change
  self-upgrade handoff and the `.disabled` flag as daemon exit paths.
- "Timeouts & Configuration": the binary-change check runs on `POWER_POLL_INTERVAL`.

## References

- `../SPEC.md` sections "Daemon", "Single-instance (flock)", "CLI Interface"
- Incident 2026-07-13: `install --force` over a running daemon produced a
  SIGKILLed binary (exit 137); recovered by removing the file first
- Homebrew Cellar/symlink layout (`<prefix>/Cellar/<formula>/<version>`, `<prefix>`
  is `/opt/homebrew` on Apple Silicon, `/usr/local` on Intel); `cargo install`
  rename-into-place at `${CARGO_HOME:-~/.cargo}/bin`
- ADR-0007 (daemon lifecycle and lazy spawn), ADR-0012 (directory watch reused for
  the disable flag), ADR-0002 (single daemon-owned caffeinate)
