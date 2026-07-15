# vigil

Keeps my Mac's display awake while Claude Code is actually working, so signing a
commit with Touch ID doesn't fail when I've stepped away for a minute.

[![CI](https://github.com/tsilenzio/vigil/actions/workflows/ci.yml/badge.svg)](https://github.com/tsilenzio/vigil/actions/workflows/ci.yml) ![macOS on Apple Silicon](https://img.shields.io/badge/macOS-Apple%20Silicon-black?logo=apple&logoColor=white)

## The problem

I sign every git commit with GPG through `pinentry-touchid`, and gpg-agent is set
to cache nothing, so every commit asks for a fresh Touch ID. If I wander off and
the screen locks, the background gpg-agent can't raise the Touch ID prompt over
the lock screen, and macOS won't hand out a biometric-protected keychain item
while the screen is locked. The commit just gets declined. Claude Code already
runs `caffeinate` while it's busy, but only against system sleep, so the display
still goes dark and locks on its own during a long turn.

Holding a display-wake lock is the obvious fix, but a naive one goes wrong two
ways. It leaks, because Claude Code fires no hook when you hit Esc to interrupt a
turn, so a per-turn lock outlives the interrupt and can pin the screen on for
hours. Or it lets go at the worst possible moment, because a `git commit` parked
on the Touch ID sheet does no tool work and looks idle to anything watching for
activity. vigil is the careful version.

## How it works

Claude Code hooks call `vigil record <event>` on each lifecycle event. That
appends a line to a per-session log in `/tmp/vigil` and makes sure the daemon is
up. A single background daemon reads those logs, keeps one `caffeinate -di` alive
while any session is active, and releases it once everything's gone quiet.

```
hook event ──▶ vigil record ──▶ append /tmp/vigil/<session>.jsonl
                    │                        │
                    └── ensure daemon ──▶  vigil daemon
                                              reads logs, reacts to kqueue events,
                                              holds one caffeinate -di while active
```

The whole thing is really an event-log supervisor that happens to manage a
`caffeinate`. Keeping the display awake is the first job wired up to that log, and
there's room to hang others off it later.

## The reactive part

The daemon doesn't poll for changes. It parks in a single `kqueue` and the kernel
wakes it the moment a session actually does something:

- A session killed with `SIGKILL` runs no shutdown code and fires no hook, but the
  kernel still reports the exit through `EVFILT_PROC`, so the lock drops about 20ms
  after the kill instead of waiting out a timeout.
- An Esc interrupt fires no hook either. It does write a marker line to the session
  transcript though, and a `NOTE_WRITE` watch on that file catches it the moment it
  lands.
- A new log file means a turn started, and a deleted one means a clean stop. A
  watch on the log directory picks up both. Directory events fire on create and
  delete but not on writes to the files inside, so a busy turn doesn't spam it.

There's still a slow 2-second tick underneath, but it's housekeeping: power and
battery checks, the grace period for a session that's waiting on you, a 12-hour
safety cap for a turn whose end signal never arrives, and the daemon shutting
itself off once nothing's active.

## A few details it gets right

It holds for the whole turn. A session is held from the moment you submit a
prompt until the turn ends, the process dies, or you interrupt it, with no
activity timeout in between. A long silent stretch of thinking can't drop the
lock mid-turn, and a `git commit` parked on the Touch ID sheet is just part of
the turn, so it sits through the biometric wait and any password fallback
without special casing. When Claude stops to ask you something, or you've gone
idle at the prompt, the lock lingers about 90 seconds and then lets the display
sleep.

Reference counting comes for free. Every session and subagent writes its own log,
and the daemon holds the lock while any log is fresh, so five Claude sessions at
once still boil down to one `caffeinate`. Close one and the screen stays lit for
the rest.

Liveness has to survive PID reuse, so the recorder saves more than a PID. It walks
its own process tree up to the session's `claude` process and records that PID
alongside its start time. If the number later gets recycled to some other process,
the start time won't match and vigil treats the session as dead instead of keeping
the screen on for a stranger.

On battery it knows when to quit. The lock releases, and won't come back, once the
charge hits 35% or a continuous hold has spent three hours on battery, whichever
comes first. Charge only ever drops while unplugged, so that floor is all it takes to
keep the laptop from dying with the screen pinned on. Plug in and the cap clears.

And it stays out of its own way. A hook starts the daemon, an advisory `flock`
keeps it down to one, and it shuts itself off once nothing's active. If it ever
dies without cleaning up, the `caffeinate -t` cap expires the lock by itself.

## Install

```
cargo build --release
./target/release/vigil install
```

That copies the binary to `~/.local/share/vigil/bin/vigil`, drops a
`~/.local/bin/vigil` symlink, and adds its seven hooks to `~/.claude/settings.json`.
Running it again is safe: it backs the file up first, only ever touches its own
entries, and leaves the rest of your hooks alone. A half-finished install gets
repaired, and `--force` refreshes the binary after a rebuild. `vigil uninstall`
takes it all back out.

Paths can be redirected with `VIGIL_INSTALL_DIR`, `VIGIL_RUNTIME_DIR`, and Claude
Code's own `CLAUDE_CONFIG_DIR`.

## Commands

| Command | What it does |
| --- | --- |
| `vigil` | Installs if it needs to, otherwise prints where things stand |
| `vigil install [--dir P] [--force] [-y]` | Install or repair |
| `vigil uninstall [-y]` | Pull out the hooks, binary, and runtime state |
| `vigil status` | Live sessions, the daemon's last decision, power and charge |
| `vigil record <event>` | Log one lifecycle event (this is what the hooks call) |
| `vigil daemon` | Run the supervisor loop (started for you) |

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

CI runs rustfmt, clippy, and the tests on macOS, and the commit messages and
branch names get checked against conventional-commit rules on every PR.

## Built with

Rust, leaning on `clap`, `serde`, `kqueue`, and `libc`. No async runtime. The
reactive loop is just one blocking `kevent`.

## License

MIT. See [LICENSE](LICENSE).
