# vigil ENHANCEMENTS

Last Updated: 2026-07-08

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

## ENH-003: Reactive log-directory watch for acquire and Stop/SessionEnd release

**Spec relation:** extends the reactive sources of ADR-0011 (`EVFILT_PROC` for
death, `EVFILT_VNODE` on transcripts for interrupts) with an `EVFILT_VNODE`/
`NOTE_WRITE` watch on the `/tmp/vigil` log directory.

`Stop`/`SessionEnd` delete a session log through the recorder, and a new turn
creates one, but the daemon receives no kqueue event for either, so it noticed
them only on the next housekeeping tick (within `POLL_INTERVAL`). Watching the log
directory makes both reactive: a log created or deleted wakes the daemon at once.
A directory `NOTE_WRITE` fires on entry create/delete, not on appends to the files
inside it, verified on 2026-07-09 (append: no fire; create/delete: fire), so
ordinary hook activity does not wake it. The crate is edge-triggered
(`clear: true`), so there is no busy-loop, and the daemon's own log deletions wake
it once to recompute, which converges.

The housekeeping tick still runs for the staleness backstop, power polling,
battery timers, and self-exit; this only removes the tick latency from the
acquire and Stop/SessionEnd release paths.

**Reversibility:** isolated to `SessionWatch::watch_dir` and the `Wake::Dir` arm
in `daemon::run`; removing them returns those paths to tick latency without other
change.
