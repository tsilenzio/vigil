# vigil

Keeps my Mac's display awake while Claude Code is actually working, so signing a
commit with Touch ID doesn't fail when I've stepped away for a minute.

## The problem

I sign every git commit with GPG through `pinentry-touchid`, and gpg-agent is set
to cache nothing, so every commit asks for a fresh Touch ID. If I wander off and
the screen locks, the background gpg-agent can't raise the Touch ID prompt over the
lock screen and the commit just gets declined. Claude Code already runs
`caffeinate` while it's busy, but only against system sleep, so the display still
goes dark and locks on its own.

Holding a display-wake lock is the obvious fix, but a naive one has two ways to go
wrong. It leaks, because Claude Code fires no hook when you press Esc, or it drops
the lock at the worst moment, because a commit sitting on the Touch ID prompt looks
idle. vigil is the careful version.

## How it works

Claude Code hooks call `vigil record <event>` on each lifecycle event. That appends
a line to a per-session log in `/tmp/vigil` and makes sure the daemon is up. A
single background daemon reads those logs, keeps one `caffeinate` alive while any
session is active, and releases it once things have been quiet for a while. A few
details it gets right:

- It holds on a little longer around a `git commit`, so the Touch ID prompt always
  has a lit screen to appear on.
- One lock covers every session and subagent, counted by reference, so running
  several Claude sessions at once doesn't stack up processes.
- On battery it caps how long it will hold and lets go before the charge runs low,
  so it never flattens the laptop.

## Status

The design is settled and the implementation is next. macOS on Apple Silicon only.

## Development

Toolchain is managed by mise, tasks by go-task:

```
mise install      # rust, task, lefthook, cocogitto
lefthook install  # git hooks
task              # list tasks
task lint         # rustfmt check + clippy
task test
task build        # release binary
```
