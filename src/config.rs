//! Compiled-in constants and the paths shared by the recorder and daemon. No
//! configuration file in this version (ADR-0008); values change by rebuilding.

use std::path::PathBuf;

/// Idle release threshold. Above the typical gap between tool events, well under
/// the 10-minute AC display-sleep timer.
pub const STANDARD_TIMEOUT: u64 = 120;

/// Applied while a commit is in flight: covers the Touch ID sheet plus any
/// password-fallback entry.
pub const COMMIT_TIMEOUT: u64 = 300;

/// Daemon poll cadence, seconds.
pub const POLL_INTERVAL: u64 = 2;

/// Consecutive idle polls before the daemon self-exits.
pub const EXIT_GRACE: u32 = 2;

/// Delete logs whose newest line is older than this, seconds.
pub const GC_THRESHOLD: u64 = 300;

/// caffeinate self-expiry backstop if the daemon dies without cleanup, seconds.
pub const SAFETY_SECS: u64 = 1800;

/// On battery, release the assertion once charge reaches this level and do not
/// re-acquire until AC. The death-prevention guard.
pub const BATTERY_FLOOR_PCT: u8 = 35;

/// Maximum continuous hold on battery before release, independent of charge,
/// seconds.
pub const BATTERY_MAX_HOLD: u64 = 10800;

/// How often the power source and charge level are re-read, seconds.
pub const POWER_POLL_INTERVAL: u64 = 30;

const VIGIL_DIR: &str = "/tmp/vigil";

pub fn vigil_dir() -> PathBuf {
    PathBuf::from(VIGIL_DIR)
}

pub fn log_path(session_id: &str) -> PathBuf {
    vigil_dir().join(format!("{session_id}.jsonl"))
}

pub fn lock_path() -> PathBuf {
    vigil_dir().join("daemon.lock")
}
