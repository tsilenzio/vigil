//! Compiled-in constants and the paths shared by the recorder and daemon. No
//! configuration file in this version (ADR-0008); values change by rebuilding.
//! Path locations honor a small set of environment overrides so an install can
//! be relocated without a recompile (`$VIGIL_RUNTIME_DIR`, `$VIGIL_INSTALL_DIR`,
//! `$CLAUDE_CONFIG_DIR`, `$XDG_DATA_HOME`).

use std::env;
use std::path::{Path, PathBuf};

/// Absolute hold cap on a single turn, seconds. Turn-span holds from
/// `UserPromptSubmit` until `Stop`/interrupt/death/awaiting-input, so a session
/// with none of those signals (a `Stop` that never fired while the process stays
/// alive with no interrupt marker) is released and GC'd only here. Sized well above
/// any real turn; on battery the floor and max-hold guards bound the case first.
/// Derived from log age, so it survives a daemon restart. ADR-0013.
pub const SAFETY_CAP: u64 = 43_200;

/// Grace before an awaiting-input session releases, seconds. A session whose newest
/// line is a permission or elicitation `Notification` is waiting on the user; this
/// holds the display briefly so it does not sleep while the user reads the dialog,
/// then releases if no answer comes. ADR-0013.
pub const AWAITING_INPUT_GRACE: u64 = 90;

/// Housekeeping tick cadence, seconds, and the kqueue poll timeout. The daemon
/// blocks up to this long for a reactive event, then does power, battery, and
/// self-exit housekeeping. Reactive releases (death, interrupt) do not depend on
/// it, but the non-reactive ones do: a `Stop`/`SessionEnd` log deletion and the
/// staleness backstop are noticed on the next tick, within this interval.
pub const POLL_INTERVAL: u64 = 2;

/// Consecutive idle polls before the daemon self-exits.
pub const EXIT_GRACE: u32 = 2;

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

/// Decision-journal heartbeat cadence, seconds. A quiet daemon writes a
/// heartbeat this often so `status` can tell it from a wedged one (TODO-003).
pub const JOURNAL_HEARTBEAT: u64 = 60;

/// Rotate the decision journal aside once it passes this size, bytes. On-change
/// volume is a few KB per day, so rotation is rare.
pub const JOURNAL_MAX_BYTES: u64 = 1_048_576;

/// Runtime dir for session logs and the daemon lock, `$VIGIL_RUNTIME_DIR`
/// overrides it. `/tmp` is cleared on reboot, the final backstop for orphaned
/// session state.
const DEFAULT_RUNTIME_DIR: &str = "/tmp/vigil";

/// Installed binary name, and the leaf the PATH symlink uses.
pub const BIN_NAME: &str = "vigil";

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// XDG data home, ignoring a non-absolute value per the spec.
fn xdg_data_home() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local").join("share"))
}

pub fn vigil_dir() -> PathBuf {
    env::var_os("VIGIL_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_DIR))
}

pub fn log_path(session_id: &str) -> PathBuf {
    vigil_dir().join(format!("{session_id}.jsonl"))
}

pub fn lock_path() -> PathBuf {
    vigil_dir().join("daemon.lock")
}

/// Sentinel that disables the daemon while present. Lives in the watched runtime
/// dir so creating it wakes the daemon reactively (ADR-0014). Reboot-cleared.
pub fn disable_flag_path() -> PathBuf {
    vigil_dir().join(".disabled")
}

/// The daemon's decision journal (TODO-003). Not `.jsonl`, which the daemon
/// scans as session logs.
pub fn journal_path() -> PathBuf {
    vigil_dir().join("daemon.log")
}

/// Install root. `$VIGIL_INSTALL_DIR` overrides `${XDG_DATA_HOME}/vigil`.
pub fn install_dir() -> PathBuf {
    env::var_os("VIGIL_INSTALL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg_data_home().join(BIN_NAME))
}

/// Absolute path the hooks invoke.
pub fn install_bin_path() -> PathBuf {
    install_dir().join("bin").join(BIN_NAME)
}

/// PATH symlink to the installed binary.
pub fn symlink_path() -> PathBuf {
    home().join(".local").join("bin").join(BIN_NAME)
}

/// Where `cargo install` places the binary: `${CARGO_HOME:-~/.cargo}/bin/vigil`.
pub fn cargo_bin_path() -> PathBuf {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cargo"))
        .join("bin")
        .join(BIN_NAME)
}

/// The Homebrew front-door path for a binary running from a Cellar, or `None` if
/// `exe` is not under a `/Cellar/` path. Homebrew installs each version under
/// `<prefix>/Cellar/vigil/<version>/bin/vigil` and points `<prefix>/bin/vigil` at
/// it, so the stable path is the prefix (everything before `/Cellar/`) plus
/// `bin/vigil`. Pure string surgery, so no `brew` shell-out (ADR-0014).
pub fn homebrew_front_door(exe: &Path) -> Option<PathBuf> {
    let s = exe.to_str()?;
    let idx = s.find("/Cellar/")?;
    Some(Path::new(&s[..idx]).join("bin").join(BIN_NAME))
}

/// Claude Code config dir. `$CLAUDE_CONFIG_DIR` overrides `~/.claude`.
pub fn claude_config_dir() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude"))
}

pub fn settings_path() -> PathBuf {
    claude_config_dir().join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homebrew_front_door_from_cellar_path() {
        // Apple Silicon prefix.
        assert_eq!(
            homebrew_front_door(Path::new("/opt/homebrew/Cellar/vigil/0.1.0/bin/vigil")),
            Some(PathBuf::from("/opt/homebrew/bin/vigil"))
        );
        // Intel prefix.
        assert_eq!(
            homebrew_front_door(Path::new("/usr/local/Cellar/vigil/0.1.0/bin/vigil")),
            Some(PathBuf::from("/usr/local/bin/vigil"))
        );
        // Not a Cellar path.
        assert_eq!(
            homebrew_front_door(Path::new("/Users/x/.cargo/bin/vigil")),
            None
        );
        assert_eq!(
            homebrew_front_door(Path::new("/Users/x/.local/share/vigil/bin/vigil")),
            None
        );
    }
}
