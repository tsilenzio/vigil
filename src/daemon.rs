//! The supervisor: single-instance lock, the reactive kqueue event loop,
//! reference counting, self-exit, power-source polling, and the battery hold cap.
//! Activity is turn-span (ADR-0013): a session is held from `UserPromptSubmit`
//! until `Stop`/death/interrupt/awaiting-input. Release on process death is
//! reactive (ADR-0011); the housekeeping tick handles power, battery timers,
//! caffeinate respawn, self-exit, and the safety-cap backstop.

use std::collections::HashSet;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

use crate::caffeinate::Caffeinate;
use crate::config;
use crate::error::Error;
use crate::event::{self, Event};
use crate::proc::{self, ProcId};
use crate::watch::{SessionWatch, Wake};

/// Spawn `vigil daemon` detached in a new session so it outlives the recorder.
/// Best-effort: the single-instance lock makes a redundant spawn a no-op, so
/// failures here never block a turn.
pub fn ensure_running() {
    if let Ok(exe) = std::env::current_exe() {
        spawn_daemon(&exe);
    }
}

/// Spawn `vigil daemon` from `exe`, detached in a new session with null stdio.
fn spawn_daemon(exe: &Path) {
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // SAFETY: pre_exec runs in the forked child before exec. setsid detaches the
    // daemon into its own session; it has no async-signal-unsafe dependencies.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = command.spawn();
}

/// The path whose inode signals that a new binary was installed: the Homebrew
/// front door when running from a Cellar (a `brew upgrade` repoints it), otherwise
/// this executable's own path (an `install --force` or `cargo install` renames a
/// new inode over it). The daemon respawns from this path so it follows a brew
/// symlink flip to the new version. ADR-0014.
fn self_watch_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(config::homebrew_front_door(&exe).unwrap_or(exe))
}

/// The `(device, inode)` the watch path resolves to, following symlinks. `None` if
/// it cannot be stat'd.
fn binary_ident(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// Run the supervisor loop. Acquires the single-instance lock or exits 0 if
/// another daemon already holds it.
pub fn run() -> Result<ExitCode, Error> {
    fs::create_dir_all(config::vigil_dir())?;

    // Disabled: a sentinel file stands the daemon down (a manual off switch).
    // Checked here so a hook-spawned daemon exits at once, and in the loop so
    // creating the file releases reactively via the log-dir watch (ADR-0014).
    if config::disable_flag_path().exists() {
        return Ok(ExitCode::SUCCESS);
    }

    // Held for the daemon's lifetime; the flock releases on drop at return.
    let lock = File::create(config::lock_path())?;
    if lock.try_lock().is_err() {
        return Ok(ExitCode::SUCCESS);
    }

    let mut watch = SessionWatch::new()?;
    // Watch the log dir so a created or deleted log (a new turn, or a
    // Stop/SessionEnd release) wakes the daemon at once.
    watch.watch_dir(&config::vigil_dir());
    let mut caffeinate = Caffeinate::default();
    let mut power = read_power().unwrap_or_else(PowerState::assume_ac);
    let mut last_power_poll = event::now_secs();
    let mut battery_capped = false;
    let mut hold_since: Option<u64> = None;
    let mut idle_ticks: u32 = 0;

    // Identity of the binary at our watch path, to notice a self-upgrade.
    let watch_path = self_watch_path();
    let self_ident = watch_path.as_deref().and_then(binary_ident);

    loop {
        // Reactive wait: block up to POLL_INTERVAL for a watched process to exit.
        // With nothing registered the kqueue would return immediately, so sleep
        // instead to keep the housekeeping cadence steady.
        let interval = Duration::from_secs(config::POLL_INTERVAL);
        let timed_out = if watch.is_empty() {
            thread::sleep(interval);
            true
        } else {
            match watch.poll(interval) {
                Some(Wake::Exited(pid)) => {
                    remove_logs_for_pid(pid);
                    false
                }
                Some(Wake::Wrote(transcript)) => {
                    release_if_interrupted(&transcript);
                    false
                }
                // A log was created or deleted; the recompute below handles it.
                Some(Wake::Dir) => false,
                None => true,
            }
        };

        // The disable flag can appear at any time; creating it wakes the log-dir
        // watch, so this releases within a wake of the file landing.
        if config::disable_flag_path().exists() {
            caffeinate.stop();
            return Ok(ExitCode::SUCCESS);
        }

        let now = event::now_secs();
        if now.saturating_sub(last_power_poll) >= config::POWER_POLL_INTERVAL {
            if let Some(fresh) = read_power() {
                power = fresh;
            }
            last_power_poll = now;

            // Self-upgrade: a new binary at the watch path (a new inode there, or a
            // brew front-door symlink repointed to a new Cellar) means an install
            // happened, so hand off to it. Release the lock before spawning so the
            // successor acquires it without racing this exit (ADR-0014).
            if let (Some(orig), Some(path)) = (self_ident, watch_path.as_deref())
                && binary_ident(path).is_some_and(|cur| cur != orig)
            {
                caffeinate.stop();
                drop(lock);
                spawn_daemon(path);
                return Ok(ExitCode::SUCCESS);
            }
        }

        let active = evaluate_sessions(now, &mut watch);

        // Battery cap: two guards OR'd, whichever fires first. The latch clears
        // only on AC; on battery a fired guard stays latched until AC returns or
        // the daemon idle-exits. Applied as a veto over the activity result, never
        // folded into it, so a live turn still releases at the floor (ADR-0013).
        battery_capped = battery_cap_latch(&power, now, hold_since, battery_capped);
        let want_hold = hold_wanted(active, &power, battery_capped);

        if want_hold {
            caffeinate.ensure_running()?;
        } else {
            caffeinate.stop();
        }
        // hold_since marks the start of the continuous hold ON BATTERY, so the
        // max-hold guard counts battery time only. A long hold on AC does not
        // consume the battery budget before an unplug (ADR-0013).
        hold_since = next_hold_since(want_hold, power.on_ac, hold_since, now);

        // Self-exit advances only on housekeeping ticks, not on reactive death
        // wakeups, so the grace window stays ~EXIT_GRACE * interval.
        if active {
            idle_ticks = 0;
        } else if timed_out {
            idle_ticks += 1;
            if idle_ticks >= config::EXIT_GRACE {
                caffeinate.stop();
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
}

/// Scan every session log once: register any newly-seen live PID for reactive
/// exit notification, drop a session whose process is already gone or interrupted,
/// GC logs past the safety cap, and return whether any session is active.
fn evaluate_sessions(now: u64, watch: &mut SessionWatch) -> bool {
    let mut active = false;
    let mut live_transcripts = HashSet::new();
    for path in session_logs() {
        let Some(last) = event::read_last_line(&path) else {
            continue;
        };

        // Liveness: register a new live PID; a PID already dead or reused at first
        // sight releases here instead of waiting for the safety cap.
        if let Some(pid) = last.pid
            && !watch.is_pid_watched(pid)
        {
            let id = ProcId {
                pid,
                start: last.pid_start.clone().unwrap_or_default(),
            };
            if proc::is_alive(&id) {
                watch.watch_pid(pid);
            } else {
                let _ = fs::remove_file(&path);
                continue;
            }
        }

        // Watch the transcript so an Esc interrupt marker is seen reactively on its
        // next write, and check it now: a marker already written before this daemon
        // started has no future write to react to (ADR-0013 scan-time check).
        if let Some(transcript) = &last.transcript {
            let transcript = PathBuf::from(transcript);
            if event::is_interrupt_transcript(&transcript) {
                let _ = fs::remove_file(&path);
                continue;
            }
            watch.watch_transcript(&transcript);
            live_transcripts.insert(transcript);
        }

        // Turn-span: an existing, live, un-interrupted session is active for the
        // whole turn. A turn that never ends is released and GC'd at the cap.
        let age = now.saturating_sub(last.ts);
        if age > config::SAFETY_CAP {
            let _ = fs::remove_file(&path);
        } else if is_active(&last, now) {
            active = true;
        }
    }
    // Close transcript watches whose session log is gone, bounding open fds.
    watch.retain_transcripts(&live_transcripts);
    active
}

/// Whether a session, already checked alive and not interrupted, counts as active
/// under turn-span. An awaiting-input session is active only through its grace; any
/// other in-flight turn is active up to the safety cap (ADR-0013).
fn is_active(last: &Event, now: u64) -> bool {
    let age = now.saturating_sub(last.ts);
    if event::is_awaiting_input(last) {
        age < config::AWAITING_INPUT_GRACE
    } else {
        age < config::SAFETY_CAP
    }
}

/// Delete the log of the session whose recorded PID matches an exited process.
fn remove_logs_for_pid(pid: u32) {
    for path in session_logs() {
        if let Some(last) = event::read_last_line(&path)
            && last.pid == Some(pid)
        {
            let _ = fs::remove_file(&path);
        }
    }
}

/// A watched transcript was written. If its newest line is the interrupt marker,
/// the session's turn was aborted with no hook fired, so release it.
fn release_if_interrupted(transcript: &Path) {
    if !event::is_interrupt_transcript(transcript) {
        return;
    }
    for path in session_logs() {
        if let Some(last) = event::read_last_line(&path)
            && last.transcript.as_deref() == transcript.to_str()
        {
            let _ = fs::remove_file(&path);
        }
    }
}

fn session_logs() -> Vec<PathBuf> {
    let mut logs = Vec::new();
    if let Ok(entries) = fs::read_dir(config::vigil_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                logs.push(path);
            }
        }
    }
    logs
}

/// Power source and charge level, parsed from `pmset -g ps`.
struct PowerState {
    on_ac: bool,
    charge: u8,
    state: String,
    remaining: Option<String>,
}

impl PowerState {
    /// Fallback when `pmset` cannot be read at startup. AC means no battery cap,
    /// matching the common plugged-in case; a real read replaces it on the next
    /// power poll.
    fn assume_ac() -> Self {
        Self {
            on_ac: true,
            charge: 100,
            state: "unknown".to_string(),
            remaining: None,
        }
    }
}

fn read_power() -> Option<PowerState> {
    let output = Command::new("pmset").arg("-g").arg("ps").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();

    // "Now drawing from 'AC Power'" or "'Battery Power'".
    let on_ac = lines.next()?.contains("'AC Power'");

    // " -InternalBattery-0 (id=...)  94%; discharging; 2:33 remaining present: true"
    let battery = lines.next().unwrap_or_default();
    let charge = parse_charge(battery).unwrap_or(100);
    let mut segments = battery.split(';');
    let _ = segments.next();
    let state = segments
        .next()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let remaining = segments.next().and_then(|s| {
        let text = s.split("present").next().unwrap_or_default().trim();
        (!text.is_empty()).then(|| text.to_string())
    });

    Some(PowerState {
        on_ac,
        charge,
        state,
        remaining,
    })
}

fn parse_charge(line: &str) -> Option<u8> {
    line.split_whitespace().find_map(|token| {
        let idx = token.find('%')?;
        token[..idx].parse().ok()
    })
}

/// The battery-cap latch after one poll. On AC the latch clears. On battery it
/// latches at the floor or once the continuous hold exceeds the max, and otherwise
/// keeps its prior value (latched until AC returns or the daemon idle-exits).
fn battery_cap_latch(power: &PowerState, now: u64, hold_since: Option<u64>, capped: bool) -> bool {
    if power.on_ac {
        false
    } else if power.charge <= config::BATTERY_FLOOR_PCT
        || hold_since.is_some_and(|s| now.saturating_sub(s) > config::BATTERY_MAX_HOLD)
    {
        // Floor guard (death prevention) OR the max continuous-hold guard.
        true
    } else {
        capped
    }
}

/// The hold decision: hold only while a session is active and the battery cap is
/// not vetoing. The battery term is applied here, over the activity result, so an
/// active turn on battery still releases once capped (ADR-0013 invariant).
fn hold_wanted(active: bool, power: &PowerState, battery_capped: bool) -> bool {
    active && (power.on_ac || !battery_capped)
}

/// The hold-start timestamp after one tick. `hold_since` marks the start of the
/// continuous hold ON BATTERY, so the max-hold guard measures battery time only. It
/// is cleared whenever the hold is not wanted or the daemon is on AC, and set on the
/// first battery tick of a hold. This keeps a long hold on AC from consuming the
/// battery budget before an unplug (ADR-0013).
fn next_hold_since(want_hold: bool, on_ac: bool, hold_since: Option<u64>, now: u64) -> Option<u64> {
    if want_hold && !on_ac {
        hold_since.or(Some(now))
    } else {
        None
    }
}

/// Print current sessions, assertion state, and power state. Read-only.
pub fn status() -> Result<(), Error> {
    let now = event::now_secs();

    println!("sessions:");
    let logs = session_logs();
    if logs.is_empty() {
        println!("  (none)");
    }
    let mut any_active = false;
    for path in &logs {
        let Some(last) = event::read_last_line(path) else {
            continue;
        };
        let idle = now.saturating_sub(last.ts);
        let awaiting = event::is_awaiting_input(&last);
        let tool_in_flight = last.event == "PreToolUse";
        let alive = last.pid.map(|pid| {
            proc::is_alive(&ProcId {
                pid,
                start: last.pid_start.clone().unwrap_or_default(),
            })
        });
        let interrupted = last
            .transcript
            .as_deref()
            .is_some_and(|t| event::is_interrupt_transcript(Path::new(t)));

        // A session is active when its process is live, not interrupted, and the
        // turn is still in flight under turn-span (ADR-0013).
        let active = alive != Some(false) && !interrupted && is_active(&last, now);
        any_active |= active;

        let sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        let pid = match (last.pid, alive) {
            (Some(pid), Some(true)) => format!("pid={pid}(alive)"),
            (Some(pid), Some(false)) => format!("pid={pid}(dead)"),
            (Some(pid), None) => format!("pid={pid}"),
            (None, _) => "pid=?".to_string(),
        };
        println!(
            "  {sid}  last={} idle={idle}s  {pid}  {}{}{}{}",
            last.event,
            if active { "active" } else { "idle" },
            if tool_in_flight {
                "  tool-in-flight"
            } else {
                ""
            },
            if awaiting { "  awaiting-input" } else { "" },
            if interrupted { "  interrupted" } else { "" },
        );
    }

    println!("active: {any_active}");
    println!("assertion held: {}", pgrep("caffeinate -di"));
    println!("daemon running: {}", pgrep("vigil daemon"));
    if config::disable_flag_path().exists() {
        println!(
            "disabled: yes  (remove {} to re-enable)",
            config::disable_flag_path().display()
        );
    }

    match read_power() {
        Some(power) => {
            let source = if power.on_ac { "AC" } else { "battery" };
            print!("power: {source}  charge={}%  {}", power.charge, power.state);
            if let Some(remaining) = &power.remaining {
                print!("  ({remaining})");
            }
            println!();
            if !power.on_ac {
                let headroom = power.charge.saturating_sub(config::BATTERY_FLOOR_PCT);
                println!(
                    "  floor headroom: {headroom}% above {}%",
                    config::BATTERY_FLOOR_PCT
                );
                println!("  max continuous hold: {}s", config::BATTERY_MAX_HOLD);
            }
        }
        None => println!("power: unknown"),
    }

    Ok(())
}

fn pgrep(pattern: &str) -> bool {
    Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(charge: u8) -> PowerState {
        PowerState {
            on_ac: false,
            charge,
            state: "discharging".to_string(),
            remaining: None,
        }
    }

    fn on_ac() -> PowerState {
        PowerState {
            on_ac: true,
            charge: 100,
            state: "charged".to_string(),
            remaining: None,
        }
    }

    #[test]
    fn turn_span_stays_active_far_past_the_old_timeout() {
        let working = Event {
            ts: 0,
            event: "PostToolUse".to_string(),
            ..Default::default()
        };
        // A gap that the old 120s timeout would have released is still active.
        assert!(is_active(&working, 1_000));
        assert!(is_active(&working, config::SAFETY_CAP - 1));
        // Past the safety cap it is not active (and evaluate_sessions GCs it).
        assert!(!is_active(&working, config::SAFETY_CAP + 1));
    }

    #[test]
    fn awaiting_input_is_active_only_through_its_grace() {
        let awaiting = Event {
            ts: 0,
            event: "Notification".to_string(),
            notification_type: Some("permission_prompt".to_string()),
            ..Default::default()
        };
        assert!(is_active(&awaiting, config::AWAITING_INPUT_GRACE - 1));
        assert!(!is_active(&awaiting, config::AWAITING_INPUT_GRACE + 1));
    }

    #[test]
    fn battery_floor_vetoes_hold_while_active() {
        // The invariant: an active turn on battery at the floor is not held.
        let power = battery(config::BATTERY_FLOOR_PCT);
        let capped = battery_cap_latch(&power, 100, Some(0), false);
        assert!(capped);
        assert!(!hold_wanted(true, &power, capped));
    }

    #[test]
    fn ac_clears_the_cap_and_allows_an_active_hold() {
        let power = on_ac();
        let capped = battery_cap_latch(&power, 100, Some(0), true);
        assert!(!capped);
        assert!(hold_wanted(true, &power, capped));
    }

    #[test]
    fn max_hold_latches_above_the_floor() {
        let power = battery(90);
        assert!(battery_cap_latch(
            &power,
            config::BATTERY_MAX_HOLD + 1,
            Some(0),
            false
        ));
        assert!(!battery_cap_latch(
            &power,
            config::BATTERY_MAX_HOLD - 1,
            Some(0),
            false
        ));
    }

    #[test]
    fn hold_since_counts_battery_time_not_ac_time() {
        // Holding on AC keeps no battery clock, so a long AC hold never accrues.
        assert_eq!(next_hold_since(true, true, None, 100), None);
        assert_eq!(next_hold_since(true, true, Some(50), 100), None);
        // The clock starts on the first battery tick and then holds its start, so
        // the max-hold budget begins at the unplug, not at the original AC hold.
        assert_eq!(next_hold_since(true, false, None, 100), Some(100));
        assert_eq!(next_hold_since(true, false, Some(50), 100), Some(50));
        // Releasing clears it.
        assert_eq!(next_hold_since(false, false, Some(50), 100), None);
    }

    #[test]
    fn parse_charge_from_pmset_line() {
        let line = " -InternalBattery-0 (id=1234)  94%; discharging; 2:33 remaining present: true";
        assert_eq!(parse_charge(line), Some(94));
        assert_eq!(parse_charge("no percentage here"), None);
    }
}
