//! The supervisor: single-instance lock, the reactive kqueue event loop,
//! reference counting, self-exit, power-source polling, and the battery hold cap.
//! Release on process death is reactive (ADR-0011); the housekeeping tick handles
//! power, battery timers, caffeinate respawn, self-exit, and the staleness
//! backstop.

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

use crate::caffeinate::Caffeinate;
use crate::config;
use crate::error::Error;
use crate::event::{self, Event};
use crate::proc::{self, ProcId};
use crate::watch::SessionWatch;

/// Spawn `vigil daemon` detached in a new session so it outlives the recorder.
/// Best-effort: the single-instance lock makes a redundant spawn a no-op, so
/// failures here never block a turn.
pub fn ensure_running() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

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

/// Run the supervisor loop. Acquires the single-instance lock or exits 0 if
/// another daemon already holds it.
pub fn run() -> Result<ExitCode, Error> {
    fs::create_dir_all(config::vigil_dir())?;

    // Held for the daemon's lifetime; the flock releases on drop at return.
    let lock = File::create(config::lock_path())?;
    if lock.try_lock().is_err() {
        return Ok(ExitCode::SUCCESS);
    }

    let mut watch = SessionWatch::new()?;
    let mut caffeinate = Caffeinate::default();
    let mut power = read_power().unwrap_or_else(PowerState::assume_ac);
    let mut last_power_poll = event::now_secs();
    let mut battery_capped = false;
    let mut hold_since: Option<u64> = None;
    let mut idle_ticks: u32 = 0;

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
                Some(dead_pid) => {
                    remove_logs_for_pid(dead_pid);
                    false
                }
                None => true,
            }
        };

        let now = event::now_secs();
        if now.saturating_sub(last_power_poll) >= config::POWER_POLL_INTERVAL {
            if let Some(fresh) = read_power() {
                power = fresh;
            }
            last_power_poll = now;
        }

        let active = evaluate_sessions(now, &mut watch);

        // Battery cap: two guards OR'd, whichever fires first. The latch clears
        // only on AC; on battery a fired guard stays latched until AC returns or
        // the daemon idle-exits.
        if power.on_ac {
            battery_capped = false;
        } else if power.charge <= config::BATTERY_FLOOR_PCT {
            battery_capped = true;
        } else if let Some(started) = hold_since
            && now.saturating_sub(started) > config::BATTERY_MAX_HOLD
        {
            battery_capped = true;
        }

        let want_hold = active && (power.on_ac || !battery_capped);

        if want_hold {
            if hold_since.is_none() {
                hold_since = Some(now);
            }
            caffeinate.ensure_running()?;
        } else {
            hold_since = None;
            caffeinate.stop();
        }

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
/// exit notification, drop a session whose process is already gone, GC logs past
/// the staleness backstop, and return whether any session is active.
fn evaluate_sessions(now: u64, watch: &mut SessionWatch) -> bool {
    let mut active = false;
    for path in session_logs() {
        let Some(last) = event::read_last_line(&path) else {
            continue;
        };

        // Liveness: register a new live PID; a PID already dead or reused at first
        // sight releases here instead of waiting for the staleness backstop.
        if let Some(pid) = last.pid
            && !watch.is_watched(pid)
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

        let idle = now.saturating_sub(last.ts);
        if idle < timeout_for(&last) {
            active = true;
        } else if idle > config::GC_THRESHOLD {
            let _ = fs::remove_file(&path);
        }
    }
    active
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

fn timeout_for(last: &Event) -> u64 {
    if event::is_unmatched_commit(last) {
        config::COMMIT_TIMEOUT
    } else {
        config::STANDARD_TIMEOUT
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
        let committing = event::is_unmatched_commit(&last);
        let active = idle < timeout_for(&last);
        any_active |= active;
        let sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        println!(
            "  {sid}  last={} idle={idle}s  {}{}",
            last.event,
            if active { "active" } else { "idle" },
            if committing { "  commit-in-flight" } else { "" },
        );
    }

    println!("active: {any_active}");
    println!("assertion held: {}", pgrep("caffeinate -di"));
    println!("daemon running: {}", pgrep("vigil daemon"));

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

    fn event(event: &str, command: Option<&str>) -> Event {
        Event {
            ts: 0,
            event: event.to_string(),
            tool: command.map(|_| "Bash".to_string()),
            command: command.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn commit_in_flight_extends_timeout() {
        let commit = event("PreToolUse", Some("git commit -m x"));
        assert_eq!(timeout_for(&commit), config::COMMIT_TIMEOUT);
    }

    #[test]
    fn ordinary_event_uses_standard_timeout() {
        let normal = event("PreToolUse", Some("git status"));
        assert_eq!(timeout_for(&normal), config::STANDARD_TIMEOUT);
        let prompt = event("UserPromptSubmit", None);
        assert_eq!(timeout_for(&prompt), config::STANDARD_TIMEOUT);
    }

    #[test]
    fn parse_charge_from_pmset_line() {
        let line = " -InternalBattery-0 (id=1234)  94%; discharging; 2:33 remaining present: true";
        assert_eq!(parse_charge(line), Some(94));
        assert_eq!(parse_charge("no percentage here"), None);
    }
}
