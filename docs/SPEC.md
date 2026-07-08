# vigil: Technical Specification

Last Updated: 2026-07-08

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
  turn is interrupted with Esc. A per-session idle timeout is the backstop for a
  session that stops logging with no other signal.
- The timeout applied to a session is extended while that session's most recent
  event is an in-flight `git commit`, which keeps the display awake through the
  Touch ID and password-fallback wait.

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
                                               │ per session: register its PID
                                               │ (EVFILT_PROC) and transcript
                                               │ (EVFILT_VNODE)
                                               ├─ process exit  ─▶ release session
                                               ├─ Esc interrupt ─▶ release session
                                               └─ timeout tick  ─▶ scan logs, staleness,
                                                     power, battery, self-exit
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
- `command` (string, optional): the command string, present when `tool` is `Bash`.
- `agent_id` (string or null, optional): non-null when the event came from a
  subagent's tool call (see Subagents).
- `pid` (u32, optional): the session's `claude` process id, captured by the recorder
  for liveness.
- `pid_start` (string, optional): that process's start time. The `(pid, start)` pair
  is the liveness key and survives PID reuse, since a reused PID carries a newer
  start time.
- `transcript` (string, optional): path to the session transcript, watched for the
  Esc-interrupt marker.

The recorder extracts `session_id`, `tool_name`, `tool_input.command`, `agent_id`,
and `transcript_path` from the hook JSON on stdin, and captures the `claude` process
identity by walking its own ancestry. Absent fields are omitted or null.

### Event Types

`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `StopFailure`,
`SessionEnd`. The daemon treats them uniformly for freshness. `PreToolUse` and its
`command` are additionally inspected for commit detection.

### Atomicity

Each recorder invocation writes exactly one line, opened with `O_APPEND`. POSIX
guarantees append writes below `PIPE_BUF` are atomic on a local filesystem, so
concurrent recorders (multiple sessions, or a subagent and its parent) do not
interleave partial lines. The daemon reads the last complete line and tolerates a
trailing partial line by ignoring it.

### Lifecycle & Cleanup

- `Stop`, `StopFailure`, and `SessionEnd` cause the recorder to delete the
  session's log. The turn or session has ended and the session stops voting; the
  daemon notices the removal on its next housekeeping tick.
- A session's `claude` process exiting, including a `SIGKILL` that fires no hook, is
  detected reactively through the PID registered with kqueue, and the daemon
  releases and removes that session at once.
- An Esc interrupt fires no hook and leaves the process alive. It writes an
  interrupt marker to the session transcript, which the daemon reads reactively on
  the transcript's next write, then releases the session.
- A session whose log goes stale with no other signal is caught by the idle-timeout
  backstop, and its log is garbage-collected once its newest line ages past the GC
  threshold.
- Reboot clears `/tmp` as the final backstop.

## Hook Contract

### Wired Events

The recorder is wired to six events in `~/.claude/settings.json`:
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `StopFailure`,
`SessionEnd`. `PreToolUse` is the heartbeat immediately before each tool, so the
assertion is fresh at the moment a `git commit` runs. `PostToolUse` marks
commit-end and keeps logs fresh through long tool sequences.

### settings.json wiring

Each event appends a hook group whose command is the vigil binary in record mode,
by absolute path, matching the existing hook style:

```json
"PreToolUse": [
  { "hooks": [{ "type": "command", "command": "/absolute/path/to/vigil record PreToolUse" }] }
]
```

The same shape is added for the other five events with the matching event name as
the argument.

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

Staleness alone cannot distinguish a killed session from an idle one, and it delays
release by the full timeout. To release a dead session at once, each event carries
the session's `claude` process identity. A hook runs as a descendant of that
process, so the recorder walks its own ancestry (via `ps`) to the nearest `claude`
ancestor and records that process's `pid` and start time. The `(pid, start)` pair is
the liveness key: a reused PID carries a newer start time and reads as dead, so a
recycled PID never keeps the display awake. The daemon registers a live PID for
reactive exit notification, and releases a session whose PID is already dead the
first time it scans that log.

### Reactive event sources

Release decisions are driven by kqueue rather than by polling:

- Each session's `claude` PID is registered with `EVFILT_PROC` / `NOTE_EXIT`. The
  kernel delivers the exit event even for a `SIGKILL` that runs no shutdown code and
  fires no hook, and the registration binds to the process, so it is immune to PID
  reuse.
- Each session's transcript is registered with `EVFILT_VNODE` / `NOTE_WRITE`. An Esc
  interrupt writes a marker line (`[Request interrupted by user`) to the transcript
  and fires no hook. The write wakes the daemon, which reads the newest transcript
  line and releases the session if it is the marker. The read is fail-open: an
  unreadable or reshaped transcript reads as not-interrupted and falls through to the
  staleness backstop.

A new turn (a created log) and a `Stop` / `SessionEnd` (a deleted log) are noticed on
the housekeeping tick, within `POLL_INTERVAL`.

### Supervisor loop

The daemon blocks in the kqueue, waking on a reactive event or, failing that, on a
housekeeping timeout of `POLL_INTERVAL`. Reactive wakeups release a session as soon
as its process exits or its turn is interrupted. The timeout drives the periodic work
that cannot be pushed reactively: the staleness scan, power polling, the battery
timers, `caffeinate` respawn, and self-exit.

```
loop:
    event = kqueue_wait(timeout = POLL_INTERVAL)
    match event:
        process-exit(pid):        # EVFILT_PROC / NOTE_EXIT, fires even on SIGKILL
            remove the log of the session that recorded this pid
        transcript-write(path):   # EVFILT_VNODE / NOTE_WRITE
            if newest transcript line is the interrupt marker:
                remove that session's log
        timeout:
            pass                  # fall through to housekeeping

    now = epoch_seconds()
    refresh_power_source_if_due()            # pmset -g ps, cached POWER_POLL_INTERVAL

    active = false
    for path in glob("/tmp/vigil/*.jsonl"):
        last = read_last_line(path)          # None if empty/partial-only
        if last is None: continue
        if last.pid is newly seen:
            register it (EVFILT_PROC) if alive, else remove the log and continue
        register last.transcript (EVFILT_VNODE) if newly seen
        idle = now - last.ts
        limit = COMMIT_TIMEOUT if is_unmatched_commit(last) else STANDARD_TIMEOUT
        if idle < limit:
            active = true
        else if idle > GC_THRESHOLD:
            remove(path)

    # Battery cap: hold_since marks the start of the current continuous hold
    # PERIOD (it does NOT reset when the caffeinate child respawns on -t expiry).
    # Two guards, OR'd, whichever fires first; the latch clears on AC.
    if on_ac:
        battery_capped = false               # plugging in clears the cap
    else if battery_pct <= BATTERY_FLOOR_PCT:
        battery_capped = true                # floor: never hold this low (death guard)
    else if hold_since is not None and (now - hold_since) > BATTERY_MAX_HOLD:
        battery_capped = true                # time budget on a continuous hold

    want_hold = active and not (on_battery and battery_capped)

    if want_hold:
        if hold_since is None: hold_since = now      # begin a hold period
        ensure_caffeinate_running()                  # start, or respawn if -t fired
    else:
        hold_since = None                            # end the hold period
        stop_caffeinate()

    # Self-exit advances only on housekeeping timeouts, not reactive wakeups, so the
    # grace window stays ~EXIT_GRACE * POLL_INTERVAL.
    if active:
        idle_ticks = 0
    else if event is timeout:
        idle_ticks += 1
        if idle_ticks >= EXIT_GRACE:
            stop_caffeinate()
            release_lock_and_exit()          # battery_capped resets with the daemon
```

### Reference counting & the staleness backstop

A session counts as active when the idle time since its newest log line is under
the applicable timeout. The assertion is held while any session is active. This
covers concurrent sessions and subagents without special cases, since each is (or
refreshes) a session log. Staleness is no longer the primary release path, reactive
process-exit and interrupt handle the common cases, but it remains the backstop for
a session that stops logging with no exit and no interrupt to react to.

### Commit-aware timeout

`is_unmatched_commit(last)` is true when `last.event == "PreToolUse"`, `last.tool
== "Bash"`, and `last.command` matches a git-commit invocation. Recommended match:

```
(^|\s|;|&&|\|)\s*git(\s+-C\s+\S+|\s+-[^\s]+)*\s+commit(\s|$)
```

This catches `git commit`, `git -C <dir> commit`, and `git ... && git commit`, and
rejects `git log`. A commit run through a shell alias is not detected; the raw
logged command is the input, and alias detection is out of scope.

While a session's newest line is an in-flight commit (the `PreToolUse` with no
following `PostToolUse` yet), the session uses `COMMIT_TIMEOUT`. This holds the
display awake through the blocking Touch ID sheet and any password-fallback wait.
When `PostToolUse` arrives, the newest line changes and the session reverts to
`STANDARD_TIMEOUT`.

### caffeinate management

The daemon owns one `caffeinate -di -t <SAFETY_SECS>` child process
(`std::process::Command`), holding the `Child` handle in memory.
`ensure_caffeinate_running` checks the child with `try_wait()` and starts a new one
if there is no live child, either because none was started or because the
`-t SAFETY_SECS` cap fired during a hold longer than 30 minutes. `stop_caffeinate`
kills the child. The daemon never runs more than one.

Respawning after a `-t` expiry does not reset `hold_since`. The `-t` cap is a
crash backstop at the OS-process level; `hold_since` tracks the logical hold period
for the battery cap, so a continuous hold that outlives the safety cap is still one
period.

### Self-exit & crash recovery

The daemon exits after `EXIT_GRACE` consecutive idle housekeeping ticks, killing its
caffeinate child first. Advancing the counter only on the timeout tick, not on
reactive wakeups, keeps the grace window at roughly `EXIT_GRACE * POLL_INTERVAL` even
under a burst of reactive events. The window also absorbs the race where a recorder
appends a fresh line and spawns a daemon just as the current daemon decides to exit.
On abnormal termination (SIGKILL, no cleanup), the `-t SAFETY_SECS` cap causes the
orphaned caffeinate to self-expire, and the next recorder respawns a fresh daemon.

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
exits (a fresh daemon starts uncapped). A brief idle gap that releases the
assertion ends the hold period and resets `hold_since`, so the cap measures only
genuinely continuous battery-powered holding, not normal work with pauses between
turns.

## Timeouts & Configuration

Compiled-in constants for this version (a `config` module):

- `STANDARD_TIMEOUT` = 120s. Idle release threshold. Above the typical gap between
  tool events, and well under the 10-minute AC display-sleep timer, so a brief
  lapse does not sleep the display mid-turn.
- `COMMIT_TIMEOUT` = 300s. Applied while a commit is in flight. Covers the Touch
  ID sheet plus password-fallback entry.
- `POLL_INTERVAL` = 2s. The kqueue wait timeout and the housekeeping cadence: the
  daemon blocks up to this long for a reactive event, then runs the staleness scan
  and the power, battery, and self-exit housekeeping.
- `EXIT_GRACE` = 2 housekeeping ticks.
- `GC_THRESHOLD` = 300s. Delete logs whose newest line is older than this.
- `SAFETY_SECS` = 1800s (30 minutes). caffeinate self-expiry backstop.
- `BATTERY_FLOOR_PCT` = 35. On battery, the assertion is released once the charge
  reaches this level and is not re-acquired until AC. This is the death-prevention
  guard: below the floor, normal power management (display sleep, then low-battery
  system sleep) is allowed to run. Set conservatively for v1 as a stand-in for the
  deferred graduated response (see `TODO.md`); lower it to use more battery.
- `BATTERY_MAX_HOLD` = 10800s (3 hours). Maximum continuous hold on battery before
  the assertion is released, independent of charge level. Bounds a long hold while
  charge is still well above the floor. Generous because Apple Silicon lasts a long
  time on battery.
- `POWER_POLL_INTERVAL` = 30s. How often the power source and charge level are
  re-read.

The two battery guards are OR'd: on battery, the assertion is released when charge
reaches `BATTERY_FLOOR_PCT` or a continuous hold exceeds `BATTERY_MAX_HOLD`,
whichever comes first. To disable one, set its constant to a no-op (floor 0, or a
very large max-hold).

No configuration file in this version. Values are changed by recompiling.

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
stdio to `/dev/null`); can be run in the foreground for debugging.

### status

```
vigil status
```

Prints active session logs, each session's newest event and idle time, whether a
commit is in flight, whether a caffeinate assertion is currently held, the power
source and charge level, and (on battery) how close each guard is to tripping (time
left on the hold and headroom above the floor). For debugging and manual
verification. Read-only.

### Exit codes

`0` success. `2` usage error (unknown event, bad arguments). `record` returns `0`
even when the append fails, to protect the turn.

## Project Structure

### Module Responsibilities

- `main.rs`: `main() -> ExitCode` delegating to `run() -> Result<ExitCode, Error>`.
- `cli.rs`: `clap` derive definitions (`record <EVENT>`, `daemon`, `status`).
- `error.rs`: `thiserror` `Error` enum with `exit_code()` and `From` conversions.
- `event.rs`: the line schema (serde types), append, read-last-line, delete, commit
  detection, and the transcript interrupt-marker check.
- `proc.rs`: capture of the session's `claude` process identity (PID and start time)
  by ancestry walk, and the liveness check that guards against PID reuse.
- `watch.rs`: the kqueue wrapper, registering PIDs (`EVFILT_PROC`) and transcripts
  (`EVFILT_VNODE`) and returning reactive wake events.
- `daemon.rs`: single-instance lock, the reactive event loop, reference counting,
  self-exit, power-source polling, and the battery hold cap.
- `caffeinate.rs`: spawn and kill the one caffeinate child.
- `config.rs`: the timeout constants and paths.

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
`record <EVENT>` argument, not the payload).

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

The v1 bash hooks at `~/Code/misc/scripts/claude-caffeinate-hooks/` stay wired in
`~/.claude/settings.json` until vigil passes the scenario matrix below. The swap
repoints the six hook events from the bash scripts to `vigil record <event>` and
removes the v1 groups. The v1 scripts and their README are retained until the swap
is confirmed stable, then retired.

## Testing & Verification

### Unit tests

- Commit detection: `git commit`, `git -C <dir> commit`, `git add -A && git
  commit`, `git commit --amend` match; `git log`, `git status` do not.
- Line schema round-trip (serialize then parse).
- `read_last_line` on empty, single-line, multi-line, and trailing-partial files.
- Staleness and timeout selection given a synthetic newest line.
- Interrupt-marker detection on a transcript's newest line.
- Process-identity ancestry walk and the start-time liveness comparison.

### Scenario matrix (manual, end-to-end)

1. Normal turn: assertion held during the turn, released after idle timeout.
2. Esc mid-turn then walk away: assertion released reactively on the interrupt
   marker, with `STANDARD_TIMEOUT` as the backstop.
3. Commit then AFK: the commit's `PreToolUse` holds `COMMIT_TIMEOUT`, and the
   display stays awake through the Touch ID / password wait.
4. Subagent tool use: a single caffeinate stays held during subagent work, none
   leaked afterward.
5. Two concurrent sessions: one caffeinate total; ending one session does not
   release while the other is active.
6. Battery cap: on battery with continuous activity, the assertion releases when
   charge reaches `BATTERY_FLOOR_PCT` or the hold exceeds `BATTERY_MAX_HOLD`,
   whichever first; reconnecting AC clears the cap and re-acquires while active.

### Manual checklist

- `pmset -g assertions` shows `PreventUserIdleDisplaySleep 1` while active.
- `vigil status` reflects sessions, commit-in-flight, and assertion state.
- Exactly one caffeinate and one daemon under load (`pgrep -fl caffeinate`,
  `pgrep -fl 'vigil daemon'`).

## Non-goals / Future

- Additional reactors on the same event stream (notifications, activity metrics).
  The event log is the seam; no reactor framework is built now.
- External OLED burn-in handling via DDC or BetterDisplay, tracked separately.
- A configuration file, if compiled-in constants become limiting.
- Graduated battery response: a charge-tiered `max-hold` step function that
  generalizes the current floor plus single max-hold. Deferred, see `TODO.md`.
