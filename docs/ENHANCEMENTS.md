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

## ENH-003: Reactive log-directory watch (promoted to ADR-0012)

Promoted to a first-class architecture decision on 2026-07-09. Full record in
`adr/0012-reactive-log-directory-watch.md`.
