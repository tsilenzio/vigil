# vigil: Technical Specification

Last Updated: 2026-07-14

## Overview & Motivation

### The Problem

Commits in this environment are GPG-signed through `pinentry-touchid`, which
reads the signing passphrase from a Touch ID protected macOS keychain item.
`~/.gnupg/gpg-agent.conf` sets `default-cache-ttl 0` and `max-cache-ttl 0`, so
every signing operation requires a fresh Touch ID authentication with no caching.

When the machine is left unattended, the built-in display idle-sleeps (10 minutes
on AC, 2 on battery) and the lock screen engages 5 minutes later. A background
process such as `gpg-agent`, spawned from the terminal running Claude Code, cannot
present a Touch ID sheet on top of the lock screen, and macOS will not release a
biometric-protected keychain item while the screen is locked. The read fails and
git reports the signature as declined.

Claude Code runs its own `caffeinate` while working, but that assertion is
`PreventUserIdleSystemSleep` only. It stops system sleep and leaves display sleep
untouched, so the screen still sleeps and locks during long turns. Verified on
2026-07-02 with `pmset -g assertions`: the pre-existing caffeinate processes held
`PreventUserIdleSystemSleep` and no display assertion.

A first-generation fix (bash hooks at `~/Code/misc/scripts/claude-caffeinate-hooks/`)
starts a `caffeinate -di` on `UserPromptSubmit` and kills it on `Stop`. It works
for normal turns but has two gaps:

- **Esc / interrupt.** Claude Code fires no hook when the user interrupts a
  streaming response (documented: Stop hooks "do not fire on user interrupts";
  `StopFailure` fires only on API errors). The per-turn assertion then survives
  until the next completed turn reuses it, or the 12h safety cap expires. An Esc
  followed by walking away holds the display awake for up to 12 hours.
- **Blocked commit.** A `git commit` presents a Touch ID sheet and blocks. If the
  sheet times out and `pinentry-touchid` falls back to a password dialog, the turn
  is blocked with no further tool activity, so any activity-based scheme sees this
  as idle.

### The Solution

vigil replaces the per-turn model with an **event-log-driven supervisor**:

- Claude Code hooks invoke `vigil record <event>` on each lifecycle event. The
  recorder appends one line to a per-session event log, capturing the session's
  `claude` process identity so the daemon can track liveness, and ensures the daemon
  is running. It does no interpretation and spawns no `caffeinate`.
- A single `vigil daemon` reference-counts active sessions by reading the logs and
  owns exactly one `caffeinate -di` assertion. It keeps the assertion while any
  session is active and releases it when all sessions go idle.
- Release is reactive. The daemon runs a kqueue event loop that wakes the instant a
  session's `claude` process exits (including a `SIGKILL`, which fires no hook) or a
  turn is interrupted with Esc.
- Activity is turn-span (ADR-0013): a session is held from `UserPromptSubmit` until
  the turn ends, the process dies, the turn is interrupted, or the session is
  waiting on the user, with no activity timeout in between. An in-flight `git
  commit` is part of the turn, so the Touch ID and password-fallback wait is
  covered without a special case. A 12-hour safety cap on log age is the backstop
  for a turn whose end signal never arrives.

### Scope

In scope for the first version: the recorder, the event log, the daemon,
`caffeinate` management, and power-source awareness (a battery hold cap so the
assertion does not drain the battery on an unattended battery session). The event
log is the general interface. Additional reactions to the same event stream are
possible later and are out of scope now.

Non-goals: OLED external-monitor burn-in handling (a separate DDC/BetterDisplay
effort), any dependency on a user-interrupt hook (none exists), and a
configuration file (constants are compiled in for this version).

## Architecture

### Components

- **Recorder** (`vigil record <event>`). Invoked by each hook. Reads the hook JSON
  from stdin, appends one line to the session's log, ensures the daemon is
  running, exits immediately. Must never block a tool or a turn.
- **Event logs** (`/tmp/vigil/<session_id>.jsonl`). One append-only JSONL file per
  Claude Code session. The shared state between recorder and daemon.
- **Daemon** (`vigil daemon`). One instance per machine, guarded by an advisory
  lock. Runs a reactive kqueue event loop over the session logs and the processes
  they name, decides the desired assertion state, and owns one `caffeinate` child.
- **caffeinate**. A single `caffeinate -di` process, spawned and killed by the
  daemon. `-d` prevents display sleep, `-i` prevents system idle sleep.

### Data Flow

```
hook event ──▶ vigil record ──▶ append line to /tmp/vigil/<sid>.jsonl
                    │                       │
                    └── ensure daemon ──▶  vigil daemon (kqueue event loop)
                                               │ watch the log dir (EVFILT_VNODE),
                                               │ each session's PID (EVFILT_PROC)
                                               │ and transcript (EVFILT_VNODE)
                                               ├─ log created/deleted ─▶ re-evaluate
                                               ├─ process exit        ─▶ release session
                                               ├─ Esc interrupt       ─▶ release session
                                               └─ timeout tick        ─▶ safety cap, power,
                                                     battery, self-exit
                                  any active? ── yes ─▶ ensure caffeinate -di
                                               └─ no ──▶ kill caffeinate, exit
```

## Event Log

### Location & Naming

`/tmp/vigil/<session_id>.jsonl`, one file per session. `/tmp` is cleared on
reboot, which is correct for ephemeral session state. The daemon's lock file lives
at `/tmp/vigil/daemon.lock`. The recorder creates `/tmp/vigil/` if absent.

### Line Schema (JSONL)

One JSON object per line, appended in event order:

```json
{"ts":1751490000,"event":"PreToolUse","tool":"Bash","command":"git commit -m \"x\"","agent_id":null,"pid":18248,"pid_start":"Thu Jul 2 20:20:25 2026","transcript":"/Users/.../<sid>.jsonl"}
```

Fields:

- `ts` (u64, required): event time, epoch seconds.
- `event` (string, required): one of the event types below.
- `tool` (string, optional): tool name, present on `PreToolUse` / `PostToolUse`.
- `command` (string, optional): the command string, present when `tool` is `Bash`,
  truncated to 1 KiB at record time so every line stays inside the tail-read
  window. Recorded for future reactors; nothing reads it today.
- `agent_id` (string or null, optional): non-null when the event came from a
  subagent's tool call (see Subagents).
- `pid` (u32, optional): the session's `claude` process id, captured by the recorder
  for liveness.
- `pid_start` (string, optional): that process's start time. The `(pid, start)` pair
  is the liveness key and survives PID reuse, since a reused PID carries a newer
  start time.
- `transcript` (string, optional): path to the session transcript, watched for the
  Esc-interrupt marker.
- `notification_type` (string, optional): present on `Notification` events, the
  awaiting-input signal (see "Awaiting-input & the safety cap").

The recorder extracts `session_id`, `tool_name`, `tool_input.command`, `agent_id`,
`transcript_path`, and `notification_type` from the hook JSON on stdin, and
captures the `claude` process identity by walking its own ancestry. Absent fields
are omitted or null.

### Event Types

`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop`,
`StopFailure`, `SessionEnd`. `UserPromptSubmit` and the tool events mark a turn in
flight, `Notification` carries the awaiting-input signal, and the terminal three
(`Stop`, `StopFailure`, `SessionEnd`) delete the log.

### Atomicity

Each recorder invocation writes exactly one line, opened with `O_APPEND`. A single
`write` to a local file does not interleave with concurrent recorders (multiple
sessions, or a subagent and its parent). The 1 KiB `command` bound keeps every
line well inside the 8 KiB tail-read window even under worst-case JSON escaping,
so the newest line is always recoverable. The daemon reads the last complete line
and tolerates a trailing partial line by ignoring it.

### Lifecycle & Cleanup

- `Stop`, `StopFailure`, and `SessionEnd` cause the recorder to delete the
  session's log. The turn or session has ended and the session stops voting; the
  daemon notices the removal reactively through the log-directory watch.
- A session's `claude` process exiting, including a `SIGKILL` that fires no hook, is
  detected reactively through the PID registered with kqueue, and the daemon
  releases and removes that session at once.
- An Esc interrupt fires no hook and leaves the process alive. It writes an
  interrupt marker to the session transcript, which the daemon reads reactively on
  the transcript's next write, then releases the session.
- A session whose newest line is an awaiting-input `Notification` releases after a
  90-second grace. A turn whose end signal never arrives is released, and its log
  garbage-collected, once its log age passes the 12-hour safety cap.
- Reboot clears `/tmp` as the final backstop.

## Hook Contract

### Wired Events

The recorder is wired to seven events in `~/.claude/settings.json`:
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop`,
`StopFailure`, `SessionEnd`. `UserPromptSubmit` starts the turn's log,
`Notification` carries the awaiting-input signal, the terminal three delete the
log, and the tool events keep the newest line current for `status` and future
reactors.

### settings.json wiring

Each event appends a hook group whose command is the vigil binary in record mode,
by absolute path, matching the existing hook style:

```json
"PreToolUse": [
  { "hooks": [{ "type": "command", "command": "/absolute/path/to/vigil record PreToolUse" }] }
]
```

The same shape is added for the other six events with the matching event name as
the argument. `vigil install` wires all seven programmatically (see CLI
Interface).

### Subagents

Verified on 2026-07-02: a subagent's tool calls fire `PreToolUse` and
`PostToolUse` under the **parent** session_id, tagged with a non-null `agent_id`.
The main agent's own tools carry the parent session_id with `agent_id: null`.
Consequence: subagent activity refreshes the parent session's log automatically,
and vigil needs no subagent-specific hooks. `agent_id` is recorded for potential
future use and is not consulted by the caffeinate logic.

## Daemon

### Single-instance (flock)

The daemon acquires an advisory exclusive lock on `/tmp/vigil/daemon.lock`
(`flock(2)`, through `std::fs::File::try_lock`) and holds it for its lifetime. A
second daemon that fails to acquire the lock exits immediately. The recorder relies
on this: it attempts to spawn a daemon after every event, and a redundant spawn is a
no-op.

### Session liveness (process identity)

Log age alone cannot distinguish a killed session from a working one, and
turn-span holds an existing log up to the safety cap. To release a dead session
at once, each event carries
the session's `claude` process identity. A hook runs as a descendant of that
process, so the recorder walks its own ancestry (via `ps`) to the nearest `claude`
ancestor and records that process's `pid` and start time. The `(pid, start)` pair is
the liveness key: a reused PID carries a newer start time and reads as dead, so a
recycled PID never keeps the display awake. The daemon registers a live PID for
reactive exit notification, and releases a session whose PID is already dead the
first time it scans that log.

### Reactive event sources

Release and acquire decisions are driven by kqueue rather than by polling:

- Each session's `claude` PID is registered with `EVFILT_PROC` / `NOTE_EXIT`. The
  kernel delivers the exit event even for a `SIGKILL` that runs no shutdown code and
  fires no hook, and the registration binds to the process, so it is immune to PID
  reuse.
- Each session's transcript is registered with `EVFILT_VNODE` / `NOTE_WRITE`. An Esc
  interrupt writes a marker line (`[Request interrupted by user`) to the transcript
  and fires no hook. The write wakes the daemon, which reads the newest transcript
  line and releases the session if it is the marker. The same check runs at scan
  time, so a marker written before the daemon started is caught on the next pass.
  The read is fail-open: an unreadable or reshaped transcript reads as
  not-interrupted and falls through to the turn-boundary and liveness signals.
- The `/tmp/vigil` log directory is registered with `EVFILT_VNODE` / `NOTE_WRITE`. A
  created log (a new turn) or a deleted one (a `Stop` / `SessionEnd`) fires the
  event, so acquire and clean release are reactive too. A directory write fires on
  entry create and delete, not on the appends to the files inside, so ordinary
  activity does not storm it.

The housekeeping tick is left for what cannot be pushed reactively: the
awaiting-input grace and the safety cap (both are the absence of a signal),
alongside the periodic power, battery, self-upgrade, and self-exit work.

### Supervisor loop

The daemon blocks in the kqueue, waking on a reactive event or, failing that, on a
housekeeping timeout of `POLL_INTERVAL`. Reactive wakeups release a session as soon
as its process exits or its turn is interrupted, and re-evaluate when a log is
created or deleted. The timeout drives the periodic work that cannot be pushed
reactively: the safety-cap and awaiting-input checks, power polling, the battery
timers, the self-upgrade binary check, `caffeinate` respawn, and self-exit.

```
loop:
    event = kqueue_wait(timeout = POLL_INTERVAL)
    match event:
        process-exit(pid):        # EVFILT_PROC / NOTE_EXIT, fires even on SIGKILL
            remove the log of the session that recorded this pid
        transcript-write(path):   # EVFILT_VNODE / NOTE_WRITE
            if newest transcript line is the interrupt marker:
                remove that session's log
        log-dir-write:            # EVFILT_VNODE / NOTE_WRITE on /tmp/vigil
            pass                  # a log was created or deleted; the scan below handles it
        timeout:
            pass                  # fall through to housekeeping

    if disable_flag_exists():                # .disabled: manual off switch (ADR-0014)
        stop_caffeinate(); journal_exit("disabled"); release_lock_and_exit()

    now = epoch_seconds()
    refresh_power_source_if_due()            # pmset -g ps, cached POWER_POLL_INTERVAL
    if binary_inode_changed():               # same cadence: self-upgrade (ADR-0014)
        stop_caffeinate(); journal_exit("self-upgrade")
        release_lock(); spawn_daemon(watch_path); exit()

    active = false
    for path in glob("/tmp/vigil/*.jsonl"):
        last = read_last_line(path)          # None if empty/partial-only
        if last is None: continue
        if last.pid is newly seen:
            register it (EVFILT_PROC) if alive, else remove the log and continue
        if newest transcript line is the interrupt marker:   # scan-time check
            remove the log and continue      # covers a marker written pre-daemon
        register last.transcript (EVFILT_VNODE) if newly seen
        age = now - last.ts                  # turn-span activity (ADR-0013)
        if age > SAFETY_CAP:
            remove(path)                     # a turn whose end signal never came
        else if awaiting_input(last):
            active |= age < AWAITING_INPUT_GRACE
        else:
            active = true                    # held for the whole turn, no timeout

    # Battery cap: two guards, OR'd, whichever fires first; the latch clears on
    # AC. hold_since marks the start of the continuous hold ON BATTERY (cleared on
    # AC, not reset by a caffeinate -t respawn), so the max-hold guard counts
    # battery time only and each unplug starts a fresh budget (ADR-0013).
    if on_ac:
        battery_capped = false               # plugging in clears the cap
    else if battery_pct <= BATTERY_FLOOR_PCT:
        battery_capped = true                # floor: never hold this low (death guard)
    else if hold_since is not None and (now - hold_since) > BATTERY_MAX_HOLD:
        battery_capped = true                # time budget on a continuous battery hold

    # The cap vetoes the activity result, never folds into it, so a live turn
    # still releases at the floor (ADR-0013 invariant).
    want_hold = active and (on_ac or not battery_capped)

    if want_hold:
        ensure_caffeinate_running()          # start, or respawn if -t fired
    else:
        stop_caffeinate()
    hold_since = (hold_since or now) if (want_hold and not on_ac) else None

    journal_decision()                       # on change, plus a 60s heartbeat (TODO-003)

    # Self-exit advances only on housekeeping timeouts, not reactive wakeups, so the
    # grace window stays ~EXIT_GRACE * POLL_INTERVAL.
    if active:
        idle_ticks = 0
    else if event is timeout:
        idle_ticks += 1
        if idle_ticks >= EXIT_GRACE:
            stop_caffeinate()
            journal_exit("idle")
            release_lock_and_exit()          # battery_capped resets with the daemon
```

### Reference counting & turn-span activity

A session counts as active under the turn-span test (ADR-0013): its log exists
(the recorder creates it on the turn's first event and deletes it on
`Stop`/`StopFailure`/`SessionEnd`), its `claude` process is alive, its
transcript's newest line is not the interrupt marker, its newest log line is not
an awaiting-input `Notification` past the grace, and its log age is under the
safety cap. There is no idle timeout between those signals: a long silent stretch
(model thinking, a long single tool) stays held for the whole turn. The assertion
is held while any session is active, which covers concurrent sessions and
subagents without special cases, since each is (or refreshes) a session log.

### Awaiting-input & the safety cap

A session whose newest line is a `Notification` with one of `permission_prompt`,
`agent_needs_input`, `elicitation_dialog` (the user must act), `idle_prompt` (the
user has gone idle at the prompt), or `agent_completed` (the turn finished) is not
actively working. It stays active through `AWAITING_INPUT_GRACE`, so the display
does not sleep while the user reads a dialog, then releases. `auth_success` and
the `elicitation_complete`/`_response` types mean work is resuming and do not
release. The next appended event becomes the newest line and the session is
active again.

`SAFETY_CAP` on log age is the backstop for a turn whose end signal never arrived
(a missed `Stop` while the process stays alive with no interrupt marker). No
shorter cap applies, because a long single tool (a build, a test run) is
indistinguishable from a missed `Stop`, and releasing mid-tool is the failure
turn-span exists to remove. The cap derives from file state, so it survives
daemon restarts. An in-flight `git commit` needs no special case: the blocking
Touch ID sheet sits inside the turn, which is held throughout. The commit-aware
timeout and its command parser are dissolved (ADR-0005, superseded by ADR-0013).

### caffeinate management

The daemon owns one `caffeinate -di -t <SAFETY_SECS>` child process
(`std::process::Command`), holding the `Child` handle in memory.
`ensure_caffeinate_running` checks the child with `try_wait()` and starts a new one
if there is no live child, either because none was started or because the
`-t SAFETY_SECS` cap fired during a hold longer than 30 minutes. `stop_caffeinate`
kills the child. The daemon never runs more than one.

Respawning after a `-t` expiry does not reset `hold_since`. The `-t` cap is a
crash backstop at the OS-process level, while `hold_since` tracks the logical hold
period on battery for the battery cap, so a continuous hold that outlives the
safety cap is still one period.

### Self-exit & crash recovery

The daemon exits after `EXIT_GRACE` consecutive idle housekeeping ticks, killing its
caffeinate child first. Advancing the counter only on the timeout tick, not on
reactive wakeups, keeps the grace window at roughly `EXIT_GRACE * POLL_INTERVAL` even
under a burst of reactive events. The window also absorbs the race where a recorder
appends a fresh line and spawns a daemon just as the current daemon decides to exit.
On abnormal termination (SIGKILL, no cleanup), the `-t SAFETY_SECS` cap causes the
orphaned caffeinate to self-expire, and the next recorder respawns a fresh daemon.

Two further clean exits (ADR-0014). The `.disabled` sentinel in the runtime dir
stands the daemon down: checked at startup so a daemon spawned while disabled
exits at once, and on each loop pass, where the runtime-dir watch makes its
creation reactive. Removing the file re-enables on the next hook, and
`vigil uninstall` uses it to stop the daemon cleanly. Separately, the daemon
re-stats its watch path (the Homebrew front door when running from a Cellar,
otherwise its own executable path) every `POWER_POLL_INTERVAL`, and a changed
`(device, inode)` means a new binary was installed, so it releases the lock,
spawns a fresh daemon from the watch path, and exits. Every clean exit writes a
journal line with its reason.

### Power source & battery cap

The machine is an Apple Silicon MacBook, so holding the display awake on battery
would drain it over a long unattended session. The primary goal is that vigil never
lets the laptop die. The daemon reads both the power source and the charge level and
applies two guards on battery, OR'd, whichever fires first.

Power source and charge level are read with `pmset -g ps` (see Implementation Notes
for parsing), cached and refreshed every `POWER_POLL_INTERVAL`. On AC there is no
cap. On battery the daemon latches `battery_capped`, releases the assertion, and
does not re-acquire while on battery and capped, when either:

- charge reaches `BATTERY_FLOOR_PCT` (the death-prevention guard; the effective
  goal-driven limit), or
- the current continuous hold period exceeds `BATTERY_MAX_HOLD` (a time budget for
  a long hold while charge is still above the floor).

Once capped, the display sleeps and locks normally, which stops the drain (a `git
commit` after that point can fail until AC is reconnected, an accepted tradeoff).
The absolute floor, not time or a percentage drop, is what guarantees survival:
charge only decreases on battery, so the floor is monotonic and needs no
hysteresis. A percentage-drop budget was considered and deferred (see ADR-0009).

The cap clears when power returns to AC, or naturally when the daemon goes idle and
exits (a fresh daemon starts uncapped). `hold_since` is cleared while on AC and set
on the first battery tick of a hold (ADR-0013), so the max-hold guard counts
battery time only: a multi-hour hold on AC does not consume the budget before an
unplug, and each unplug starts a fresh one. A brief idle gap that releases the
assertion likewise resets `hold_since`, so the cap measures only genuinely
continuous battery-powered holding, not normal work with pauses between turns.

## Timeouts & Configuration

Compiled-in constants for this version (a `config` module):

- `SAFETY_CAP` = 43200s (12 hours). Absolute cap on log age. Turn-span holds a
  session for the whole turn, so this is the backstop for a turn whose end signal
  never arrives, and the GC threshold for its log.
- `AWAITING_INPUT_GRACE` = 90s. How long an awaiting-input session stays active
  after the `Notification`, so the display does not sleep while the user reads
  the dialog.
- `POLL_INTERVAL` = 2s. The kqueue wait timeout and the housekeeping cadence: the
  daemon blocks up to this long for a reactive event, then runs the safety-cap
  and awaiting-input checks and the power, battery, and self-exit housekeeping.
- `EXIT_GRACE` = 2 housekeeping ticks.
- `SAFETY_SECS` = 1800s (30 minutes). caffeinate self-expiry backstop.
- `BATTERY_FLOOR_PCT` = 35. On battery, the assertion is released once the charge
  reaches this level and is not re-acquired until AC. This is the death-prevention
  guard: below the floor, normal power management (display sleep, then low-battery
  system sleep) is allowed to run. Set conservatively for v1 as a stand-in for the
  deferred graduated response (see `TODO.md`); lower it to use more battery.
- `BATTERY_MAX_HOLD` = 10800s (3 hours). Maximum continuous battery-powered hold
  before the assertion is released, independent of charge level. Bounds a long
  hold while charge is still well above the floor. Time on AC does not count
  against it.
- `POWER_POLL_INTERVAL` = 30s. How often the power source and charge level are
  re-read, and the cadence of the self-upgrade binary check.
- `JOURNAL_HEARTBEAT` = 60s. Decision-journal heartbeat cadence while the state
  is unchanged.
- `JOURNAL_MAX_BYTES` = 1 MiB. The journal rotates aside past this size.

The two battery guards are OR'd: on battery, the assertion is released when charge
reaches `BATTERY_FLOOR_PCT` or a continuous battery hold exceeds
`BATTERY_MAX_HOLD`, whichever comes first. To disable one, set its constant to a
no-op (floor 0, or a very large max-hold).

No configuration file in this version. Timing values are changed by recompiling.
Path locations honor `$VIGIL_RUNTIME_DIR`, `$VIGIL_INSTALL_DIR`,
`$CLAUDE_CONFIG_DIR`, and `$XDG_DATA_HOME` (ENH-005), so an install can be
relocated or sandboxed without a rebuild.

## CLI Interface

### record

```
vigil record <EVENT>
```

Reads hook JSON from stdin, appends a line to the session log, ensures the daemon
is running, exits 0. `Stop`, `StopFailure`, and `SessionEnd` delete the log
instead of appending. Exits 0 even on internal error so a hook never blocks a
turn (errors go to stderr).

### daemon

```
vigil daemon
```

Runs the supervisor loop. Acquires the single-instance lock or exits 0 if another
daemon holds it. Normally spawned detached by `record` (new session via `setsid`,
stdio to `/dev/null`); can be run in the foreground for debugging. Writes its
decisions to the journal at `${VIGIL_RUNTIME_DIR}/daemon.log` (TODO-003): a line
per decision change, a heartbeat while quiet, and start/exit lines with reasons.

### status

```
vigil status
```

Prints active session logs, each session's newest event and idle time, whether a
tool is in flight or the session is awaiting input, the daemon's own last
journaled decision with its reason and a staleness verdict (a stale entry from a
live daemon reads as wedged, one from a dead daemon is its final decision),
whether a caffeinate assertion is currently held, and the power source and charge
level with the battery guards' headroom. For debugging and manual verification.
Read-only.

### install / uninstall

```
vigil install [--dir P] [--force] [-y]
vigil uninstall [-y]
```

`install` classifies where it is running from and wires the hooks at the
appropriate stable path (ADR-0014): a Homebrew Cellar binary wires the
`<prefix>/bin/vigil` front door, a cargo binary wires
`${CARGO_HOME:-~/.cargo}/bin/vigil`, and anything else is copied to
`${VIGIL_INSTALL_DIR:-~/.local/share/vigil}/bin/vigil` with a `~/.local/bin`
PATH symlink. Own-copy replacement is by atomic rename, never an in-place
overwrite, which would invalidate the running binary's Mach-O signature.
Settings edits are surgical and backed up: only vigil's own hook entries are
added or removed, and a partial install repairs to the location the existing
hooks reference. Bare `vigil` with no subcommand reports install state, or
offers to install or repair.

`uninstall` reverses it: strips the hooks, removes the binary and symlink for
own-copy installs (managed binaries are left to `cargo uninstall` /
`brew uninstall`), stands the daemon down via the `.disabled` sentinel and
confirms the exit by probing the single-instance lock (pkill only as a wedged
fallback), and removes the runtime dir.

### Exit codes

`0` success. `2` usage error (unknown event, bad arguments). `record` returns `0`
even when the append fails, to protect the turn.

## Project Structure

### Module Responsibilities

- `main.rs`: `main() -> ExitCode` delegating to `run() -> Result<ExitCode, Error>`.
- `cli.rs`: `clap` derive definitions (`record <EVENT>`, `daemon`, `status`,
  `install`, `uninstall`).
- `error.rs`: `thiserror` `Error` enum with `exit_code()` and `From` conversions.
- `event.rs`: the line schema (serde types), append with the command bound,
  read-last-line, delete, the awaiting-input check, and the transcript
  interrupt-marker check.
- `proc.rs`: capture of the session's `claude` process identity (PID and start time)
  by ancestry walk, and the liveness check that guards against PID reuse.
- `watch.rs`: the kqueue wrapper, registering PIDs (`EVFILT_PROC`), transcripts, and
  the log directory (`EVFILT_VNODE`) and returning reactive wake events.
- `daemon.rs`: single-instance lock, the reactive event loop, turn-span activity,
  self-exit, self-upgrade, power-source polling, the battery hold cap, and
  `status`.
- `journal.rs`: the decision journal (entries, on-change/heartbeat cadence,
  rotation, read-last).
- `install.rs`: install modes, surgical settings wiring, binary placement, and
  uninstall with the daemon stand-down.
- `caffeinate.rs`: spawn and kill the one caffeinate child.
- `config.rs`: the timeout constants, journal parameters, and paths with their
  environment overrides.

Dependencies follow the rune-keychain baseline plus JSON and detachment: `clap`
(derive), `thiserror`, `serde` + `serde_json`, `kqueue` (the reactive event loop),
and `libc` (for `setsid` and PID registration). The single-instance lock uses the
standard library's `File::try_lock` rather than a separate flock crate.

## Implementation Notes

Details a fresh session would otherwise rediscover. Captured from live hook
payloads and testing on 2026-07-02.

### Hook payload shapes

The recorder reads the raw hook JSON from stdin (the JSON Claude Code passes to any
command hook). `session_id` is top-level on every event. Tool events add
`tool_name`, `tool_input`, and, only for subagent tool calls, `agent_id`. A real
`PreToolUse` for a `Bash` call from the main agent:

```json
{
  "session_id": "e994ec41-2e5b-4e12-b9f9-25fbda39c543",
  "transcript_path": "/Users/.../e994ec41-....jsonl",
  "cwd": "/private/tmp",
  "prompt_id": "eb1c13b4-...",
  "permission_mode": "auto",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "echo hi", "description": "..." },
  "tool_use_id": "toolu_01..."
}
```

`PostToolUse` has the same fields plus `duration_ms` and `tool_response`. Field
extraction for a log line:

- `session_id` = `.session_id` (present on all six wired events).
- `tool` = `.tool_name` (present on `PreToolUse` / `PostToolUse`).
- `command` = `.tool_input.command` (present when `tool_name == "Bash"`).
- `agent_id` = `.agent_id` if present, else null. Present (a string) only when the
  tool ran inside a subagent, absent for the main agent. Recorded, not used by the
  caffeinate logic.
- `transcript` = `.transcript_path` (present on tool events; watched by the daemon
  for the Esc-interrupt marker).

`UserPromptSubmit`, `Stop`, `StopFailure`, and `SessionEnd` carry `session_id` at
top level, which is all the recorder needs from them (the event name comes from the
`record <EVENT>` argument, not the payload). `Notification` additionally carries
`notification_type` (captured 2026-07-13: `permission_prompt` with a
"Claude needs your permission" message) and a `message`, which the recorder does
not log.

### Detaching the daemon

`record` spawns `vigil daemon` in a new session so it outlives the recorder. Use a
`pre_exec` that calls `libc::setsid()`, with stdio to `/dev/null`:

```rust
use std::os::unix::process::CommandExt;
// unsafe: pre_exec runs in the forked child before exec
unsafe {
    Command::new(current_exe()?)
        .arg("daemon")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .pre_exec(|| { libc::setsid(); Ok(()) })
        .spawn()?;
}
```

The single-instance flock makes a redundant spawn a no-op, so `record` can always
attempt it.

### Process identity capture

The recorder resolves the session's `claude` process by running `ps` once and
walking parent links from its own PID up to the nearest ancestor whose command
contains `claude`, bounded to a small depth. The match is a substring so an
app-forked session whose command is a versioned binary path still resolves. The
recorded start time is compared against a fresh `ps -o lstart=` read for the liveness
check, normalizing whitespace so a single-digit day (which `ps` pads with a double
space) still matches.

### Power source detection

Parse the first line of `pmset -g ps`:

```
Now drawing from 'Battery Power'
 -InternalBattery-0 (id=...)  94%; discharging; 2:33 remaining present: true
```

On AC the first line reads `Now drawing from 'AC Power'`. Match the quoted source:
`AC Power` is on AC, `Battery Power` is on battery. The second line carries the
charge level the floor guard needs, the integer before `%` (`94` here); parse it
with a `NN%` match on that line. The charge state (`discharging` / `charging` /
`charged`) and time remaining are also on that line if `status` wants them. Shelling
`pmset` every `POWER_POLL_INTERVAL` and caching between avoids spawning it on every
2s tick.

### Test methodology

The existing `log-event.sh` hook (already wired for every event in
`~/.claude/settings.json`) logs each event's raw payload to
`/tmp/hooks/<Event>.jsonl` when `/tmp/hooks/.logs` exists, wrapping the raw payload
under `.event`:

```
mkdir -p /tmp/hooks && touch /tmp/hooks/.logs      # enable
# ... trigger events (send a prompt, run a tool, Esc, etc.) ...
jq '.event' /tmp/hooks/PreToolUse.jsonl            # inspect raw payloads
rm -f /tmp/hooks/.logs /tmp/hooks/*.jsonl          # disable + clean
```

Assertion and process state:

- `pmset -g assertions | grep PreventUserIdleDisplaySleep` (top counter is 1 while
  held). The per-process line can lag the counter by up to ~1s after spawn.
- `pgrep -fl caffeinate` and `pgrep -fl 'vigil daemon'` should show exactly one
  each under load.
- BSD `grep` has no `\s`; use `[[:space:]]` or `grep -E ' +'`.

Drive the scenario matrix with real turns while watching these, and unplug AC to
exercise the battery cap.

## Migration from the bash hooks

Completed 2026-07-11. The v1 bash hooks at
`~/Code/misc/scripts/claude-caffeinate-hooks/` were replaced by `vigil install`
wiring the hook events to `vigil record <event>`. The v1 scripts are retained as
historical reference only.

## Testing & Verification

### Unit tests

- Line schema round-trip (serialize then parse), and the command bound: an
  oversized command truncates on a char boundary and the worst-case line stays
  inside the tail window.
- `read_last_line` on empty, single-line, multi-line, and trailing-partial files.
- Turn-span activity and the awaiting-input grace given a synthetic newest line.
- The battery invariants: the floor vetoes an active hold, and `hold_since`
  accrues battery time only.
- Interrupt-marker detection on a transcript's newest line.
- Process-identity ancestry walk and the start-time liveness comparison.
- Journal cadence (decision on change, heartbeat when quiet), rotation, and
  read-last.
- Settings surgery: insert/strip round-trips external hooks, stale-path
  detection, managed-mode noops.

### Scenario matrix (manual, end-to-end)

1. Normal turn: assertion held for the whole turn, including a tool-free gap
   longer than two minutes, released on `Stop`.
2. Esc mid-turn then walk away: assertion released reactively on the interrupt
   marker, with the scan-time marker check as the backstop.
3. Commit then AFK: the turn stays held through the Touch ID / password wait,
   with no timeout to outlast.
4. Subagent tool use: a single caffeinate stays held during subagent work, none
   leaked afterward.
5. Two concurrent sessions: one caffeinate total; ending one session does not
   release while the other is active.
6. Battery cap: on battery with continuous activity, the assertion releases when
   charge reaches `BATTERY_FLOOR_PCT` or the hold exceeds `BATTERY_MAX_HOLD`,
   whichever first; reconnecting AC clears the cap and re-acquires while active.

### Manual checklist

- `pmset -g assertions` shows `PreventUserIdleDisplaySleep 1` while active.
- `vigil status` reflects sessions, tool-in-flight and awaiting-input states, the
  daemon's journaled decision, and assertion state.
- Exactly one caffeinate and one daemon under load (`pgrep -fl caffeinate`,
  `pgrep -fl 'vigil daemon'`).

## Non-goals / Future

- Additional reactors on the same event stream (notifications, activity metrics).
  The event log is the seam; no reactor framework is built now.
- External OLED burn-in handling via DDC or BetterDisplay, tracked separately.
- A configuration file, if compiled-in constants become limiting.
- Graduated battery response: a charge-tiered `max-hold` step function that
  generalizes the current floor plus single max-hold. Deferred, see `TODO.md`.
