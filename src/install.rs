//! Install and uninstall: place the binary, wire the Claude Code lifecycle hooks into
//! `settings.json`, and reverse both. All settings edits are surgical, only
//! vigil's own hook entries are added or removed, so hooks and settings the user
//! added stay intact. Install is idempotent: a fully consistent install is a
//! noop, and a partial one is repaired to the location the existing hooks expect.
//! Uninstall works on the live file and never restores a backup, so external
//! changes made since install survive.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config;
use crate::error::Error;
use crate::event::{self, EventKind};

/// Bare `vigil`: report state when installed and consistent, otherwise offer to
/// install or repair interactively.
pub fn bootstrap() -> Result<(), Error> {
    let settings = read_settings()?;
    let (desired_bin, mode) = resolve_target(None, &settings);
    let plan = build_plan(&settings, &desired_bin, mode, false);

    if plan.is_noop() {
        let n = EventKind::ALL.len();
        println!("vigil is installed at {}", desired_bin.display());
        println!("  hooks:   {n} of {n} wired");
        match mode {
            InstallMode::OwnCopy => {
                println!("  symlink: {}", config::symlink_path().display())
            }
            InstallMode::Cargo => println!("  managed: cargo"),
            InstallMode::Homebrew => println!("  managed: homebrew"),
        }
        println!(
            "  daemon:  {}",
            if daemon_running() {
                "running"
            } else {
                "idle (spawns on the next hook)"
            }
        );
        println!(
            "\nRun `vigil status` for live session and power state, or `vigil uninstall` to remove."
        );
        return Ok(());
    }

    print_install_plan(&settings, &plan);
    let prompt = if hook_bins(&settings).is_empty() {
        "Install now?"
    } else {
        "Repair now?"
    };
    if !confirm(prompt) {
        println!("aborted; nothing changed");
        return Ok(());
    }
    apply(&plan)?;
    println!("vigil installed. New Claude Code sessions will pick up the hooks.");
    Ok(())
}

/// `vigil install`: install or repair to a consistent state. Noop when already
/// consistent unless `force` overwrites the binary.
pub fn install(dir: Option<PathBuf>, force: bool, assume_yes: bool) -> Result<(), Error> {
    let settings = read_settings()?;
    let (desired_bin, mode) = resolve_target(dir.as_deref(), &settings);
    let plan = build_plan(&settings, &desired_bin, mode, force);

    if plan.is_noop() {
        let n = EventKind::ALL.len();
        println!("vigil is already installed at {}", desired_bin.display());
        match mode {
            InstallMode::OwnCopy => {
                println!("  {n} hooks wired, binary present, symlink ok");
                println!("  (re-run with --force to overwrite the binary after a rebuild)");
            }
            InstallMode::Cargo => println!("  {n} hooks wired, cargo-managed binary"),
            InstallMode::Homebrew => println!("  {n} hooks wired, homebrew-managed binary"),
        }
        return Ok(());
    }

    print_install_plan(&settings, &plan);
    if !assume_yes && !confirm("Proceed?") {
        println!("aborted; nothing changed");
        return Ok(());
    }
    apply(&plan)?;
    println!("vigil installed. New Claude Code sessions will pick up the hooks.");
    Ok(())
}

/// `vigil uninstall`: strip vigil's hooks from the live settings, remove the
/// binary, symlink, and runtime state, and stop the daemon.
pub fn uninstall(assume_yes: bool) -> Result<(), Error> {
    let settings = read_settings()?;
    let mode = detect_mode();
    let bin = consistent_hook_bin(&settings).unwrap_or_else(config::install_bin_path);

    let stripped = {
        let mut s = settings.clone();
        strip_vigil(&mut s);
        s
    };
    let remove_hooks = stripped != settings;
    // Managed installs (cargo/brew) own the binary and its PATH entry; vigil leaves
    // both for `cargo uninstall` / `brew uninstall` and removes only its own state.
    let manage_binary = !mode.is_managed();
    let bin_present = manage_binary && binary_present(&bin);
    let link = config::symlink_path();
    let link_is_ours = manage_binary && symlink_ok(&bin);
    let runtime = config::vigil_dir();
    let runtime_present = runtime.exists();

    if !remove_hooks && !bin_present && !link_is_ours && !runtime_present {
        println!("vigil is not installed; nothing to remove");
        return Ok(());
    }

    println!("Uninstall plan:\n");
    if remove_hooks {
        println!(
            "  remove hooks   {}  (only vigil entries; others preserved)",
            config::settings_path().display()
        );
    }
    if bin_present {
        println!("  remove binary  {}", bin.display());
    }
    if link_is_ours {
        println!("  remove symlink {}", link.display());
    }
    if mode.is_managed() {
        let pm = if mode == InstallMode::Cargo {
            "cargo uninstall vigil"
        } else {
            "brew uninstall vigil"
        };
        println!("  keep binary    {}  (remove with `{pm}`)", bin.display());
    }
    println!(
        "  stop daemon    stand down via {} (pkill if wedged)",
        config::disable_flag_path().display()
    );
    if runtime_present {
        println!("  clean runtime  {}", runtime.display());
    }
    println!();

    if !assume_yes && !confirm("Proceed?") {
        println!("aborted; nothing changed");
        return Ok(());
    }

    // Stop the daemon first so it does not recreate runtime state mid-clean.
    stop_daemon();

    if remove_hooks {
        write_settings_backed_up(&stripped)?;
    }
    if bin_present {
        let _ = fs::remove_file(&bin);
        cleanup_install_dirs(&bin);
    }
    if link_is_ours {
        let _ = fs::remove_file(&link);
    }
    // Rechecked rather than keyed on the plan: stop_daemon creates the disable
    // flag inside the runtime dir, and a leftover flag would block the next
    // install's daemon until reboot.
    if runtime.exists() {
        let _ = fs::remove_dir_all(&runtime);
    }

    println!("vigil uninstalled.");
    Ok(())
}

/// How vigil was obtained, which decides binary placement and hook target
/// (ADR-0014). Managed modes leave the binary to the package manager.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InstallMode {
    /// Copy self to vigil's own dir and manage a PATH symlink.
    OwnCopy,
    /// Running from cargo's bin; hooks point there, cargo owns the binary.
    Cargo,
    /// Running from a Homebrew Cellar; hooks point at the brew bin symlink.
    Homebrew,
}

impl InstallMode {
    /// A package-managed binary vigil neither copies nor removes.
    fn is_managed(self) -> bool {
        matches!(self, Self::Cargo | Self::Homebrew)
    }
}

/// Classify how the running binary was installed. An explicit `$VIGIL_INSTALL_DIR`
/// forces own-copy; otherwise the running binary's location selects the mode.
fn detect_mode() -> InstallMode {
    if env::var_os("VIGIL_INSTALL_DIR").is_some() {
        return InstallMode::OwnCopy;
    }
    if let Ok(exe) = env::current_exe() {
        if config::homebrew_front_door(&exe).is_some() {
            return InstallMode::Homebrew;
        }
        if canon_eq(&exe, &config::cargo_bin_path()) {
            return InstallMode::Cargo;
        }
    }
    InstallMode::OwnCopy
}

/// The set of changes an install needs. Empty (`is_noop`) means fully consistent.
struct Plan {
    mode: InstallMode,
    desired_bin: PathBuf,
    desired_settings: Value,
    write_settings: bool,
    copy_binary: bool,
    fix_symlink: bool,
    /// Existing hooks point at a different path, this is a move/repoint.
    stale: bool,
}

impl Plan {
    fn is_noop(&self) -> bool {
        !self.write_settings && !self.copy_binary && !self.fix_symlink
    }
}

fn build_plan(settings: &Value, desired_bin: &Path, mode: InstallMode, force: bool) -> Plan {
    let desired_settings = desired_settings(settings, desired_bin);
    let write_settings = &desired_settings != settings;
    // Managed installs (cargo/brew) own the binary and are already on PATH, so
    // vigil neither copies the binary nor manages a PATH symlink for them.
    let copy_binary = !mode.is_managed() && (force || !binary_present(desired_bin));
    let fix_symlink = !mode.is_managed() && !symlink_ok(desired_bin);
    let stale = consistent_hook_bin(settings).is_some_and(|p| p != desired_bin);

    Plan {
        mode,
        desired_bin: desired_bin.to_path_buf(),
        desired_settings,
        write_settings,
        copy_binary,
        fix_symlink,
        stale,
    }
}

/// The hook target and install mode: an explicit `--dir` or `$VIGIL_INSTALL_DIR`
/// forces own-copy at that location; a cargo or Homebrew binary points the hooks at
/// its own stable path; otherwise own-copy to wherever the existing hooks point, or
/// the default (ADR-0014).
fn resolve_target(cli_dir: Option<&Path>, settings: &Value) -> (PathBuf, InstallMode) {
    if let Some(dir) = cli_dir {
        return (dir.join("bin").join(config::BIN_NAME), InstallMode::OwnCopy);
    }
    let mode = detect_mode();
    let target = match mode {
        InstallMode::Homebrew => env::current_exe()
            .ok()
            .and_then(|e| config::homebrew_front_door(&e))
            .unwrap_or_else(config::install_bin_path),
        InstallMode::Cargo => config::cargo_bin_path(),
        InstallMode::OwnCopy if env::var_os("VIGIL_INSTALL_DIR").is_some() => {
            config::install_bin_path()
        }
        InstallMode::OwnCopy => {
            consistent_hook_bin(settings).unwrap_or_else(config::install_bin_path)
        }
    };
    (target, mode)
}

fn apply(plan: &Plan) -> Result<(), Error> {
    if plan.copy_binary {
        copy_self(&plan.desired_bin)?;
    }
    if plan.write_settings {
        write_settings_backed_up(&plan.desired_settings)?;
    }
    if plan.fix_symlink {
        fix_symlink(&plan.desired_bin)?;
    }
    Ok(())
}

// --- settings.json manipulation (pure, unit-tested) ---------------------------

/// The command string a hook invokes for one event.
fn vigil_command(bin: &Path, ev: EventKind) -> String {
    format!("{} record {}", bin.display(), ev.as_str())
}

/// Whether a hook `command` string is one vigil wrote: program basename `vigil`
/// invoked as `record`. Independent of the path, so a moved install is matched.
fn command_is_vigil(cmd: &str) -> bool {
    let mut tokens = cmd.split_whitespace();
    let Some(prog) = tokens.next() else {
        return false;
    };
    let is_vigil = Path::new(prog).file_name().and_then(|n| n.to_str()) == Some(config::BIN_NAME);
    is_vigil && tokens.next() == Some("record")
}

/// The binary paths every vigil hook entry references, across all wired events.
fn hook_bins(settings: &Value) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return out;
    };
    for ev in EventKind::ALL {
        let Some(arr) = hooks.get(ev.as_str()).and_then(Value::as_array) else {
            continue;
        };
        for group in arr {
            let Some(inner) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for hook in inner {
                if let Some(cmd) = hook.get("command").and_then(Value::as_str)
                    && command_is_vigil(cmd)
                    && let Some(prog) = cmd.split_whitespace().next()
                {
                    out.push(PathBuf::from(prog));
                }
            }
        }
    }
    out
}

/// The single path the hooks reference, if they all agree. `None` when absent or
/// inconsistent.
fn consistent_hook_bin(settings: &Value) -> Option<PathBuf> {
    let bins = hook_bins(settings);
    let first = bins.first()?.clone();
    bins.iter().all(|b| *b == first).then_some(first)
}

/// Remove every vigil hook entry from the wired event arrays, pruning any group,
/// event array, or the `hooks` object left empty.
fn strip_vigil(settings: &mut Value) {
    let Some(obj) = settings.as_object_mut() else {
        return;
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };

    for ev in EventKind::ALL {
        if let Some(arr) = hooks.get_mut(ev.as_str()).and_then(Value::as_array_mut) {
            arr.retain_mut(
                |group| match group.get_mut("hooks").and_then(Value::as_array_mut) {
                    Some(inner) => {
                        inner.retain(|hook| {
                            !hook
                                .get("command")
                                .and_then(Value::as_str)
                                .is_some_and(command_is_vigil)
                        });
                        !inner.is_empty()
                    }
                    None => true,
                },
            );
        }
    }

    let empty: Vec<String> = EventKind::ALL
        .iter()
        .map(|ev| ev.as_str())
        .filter(|key| {
            hooks
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(|a| a.is_empty())
        })
        .map(String::from)
        .collect();
    for key in empty {
        hooks.remove(&key);
    }

    let hooks_empty = hooks.is_empty();
    if hooks_empty {
        obj.remove("hooks");
    }
}

/// Append a fresh vigil hook group to each wired event, creating the
/// `hooks` object and event arrays as needed.
fn insert_vigil(settings: &mut Value, bin: &Path) {
    let obj = settings.as_object_mut().expect("settings is a JSON object");
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("hooks is a JSON object");
    for ev in EventKind::ALL {
        let group = json!({
            "hooks": [ { "type": "command", "command": vigil_command(bin, ev) } ]
        });
        hooks
            .entry(ev.as_str())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("event hooks is a JSON array")
            .push(group);
    }
}

/// The settings vigil wants: current, with its own entries stripped and re-added
/// at `bin`. Equal to the input when the install is already consistent.
fn desired_settings(current: &Value, bin: &Path) -> Value {
    let mut desired = current.clone();
    strip_vigil(&mut desired);
    insert_vigil(&mut desired, bin);
    desired
}

// --- filesystem side effects --------------------------------------------------

fn read_settings() -> Result<Value, Error> {
    let path = config::settings_path();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(json!({})),
        Err(err) => return Err(err.into()),
    };
    if text.trim().is_empty() {
        return Ok(json!({}));
    }

    let value: Value = serde_json::from_str(&text)?;
    // Reject shapes the surgical edits assume against, rather than clobbering an
    // unexpected file.
    if !value.is_object() {
        return Err(io::Error::other("settings.json is not a JSON object").into());
    }
    if let Some(hooks) = value.get("hooks") {
        if !hooks.is_object() {
            return Err(io::Error::other("settings.json \"hooks\" is not an object").into());
        }
        for ev in EventKind::ALL {
            if hooks.get(ev.as_str()).is_some_and(|v| !v.is_array()) {
                return Err(io::Error::other(format!(
                    "settings.json hooks.{} is not an array",
                    ev.as_str()
                ))
                .into());
            }
        }
    }
    Ok(value)
}

/// Write settings atomically (temp file then rename), backing up the prior file
/// and pruning old backups to the newest two.
fn write_settings_backed_up(desired: &Value) -> Result<(), Error> {
    let path = config::settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let backup = path.with_file_name(format!("settings.json.bak-{}", event::now_secs()));
        fs::copy(&path, &backup)?;
        if let Some(parent) = path.parent() {
            prune_backups(parent);
        }
    }

    let mut text = serde_json::to_string_pretty(desired)?;
    text.push('\n');
    let tmp = path.with_extension("json.vigil-tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Keep only the two most recent `settings.json.bak-*` files.
fn prune_backups(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("settings.json.bak-"))
        })
        .collect();
    // The epoch-second suffix is fixed width for the next ~250 years, so a
    // lexical sort is chronological.
    backups.sort();
    const KEEP: usize = 2;
    if backups.len() > KEEP {
        for old in &backups[..backups.len() - KEEP] {
            let _ = fs::remove_file(old);
        }
    }
}

fn copy_self(dest: &Path) -> Result<(), Error> {
    let src = env::current_exe()?;
    // Running the installed binary already: nothing to copy.
    if canon_eq(&src, dest) {
        return Ok(());
    }
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    // Copy to a temp file beside the destination, then rename over it. rename
    // repoints the directory entry to a new inode instead of rewriting the bytes
    // of the inode a running daemon holds. An in-place overwrite invalidates the
    // Mach-O signature and macOS SIGKILLs the next exec of the path. The temp
    // shares the destination directory so the rename stays on one filesystem.
    let tmp = dest.with_file_name(format!("{}.vigil-tmp", config::BIN_NAME));
    fs::copy(&src, &tmp)?;
    let mut perms = fs::metadata(&tmp)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tmp, perms)?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

fn fix_symlink(bin: &Path) -> Result<(), Error> {
    let link = config::symlink_path();
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::remove_file(&link)?;
        }
        Ok(_) => {
            eprintln!(
                "vigil: {} exists and is not a symlink; leaving it in place",
                link.display()
            );
            return Ok(());
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    std::os::unix::fs::symlink(bin, &link)?;
    Ok(())
}

/// Remove the `bin/` and install-root directories once emptied. `remove_dir`
/// only succeeds on an empty directory, so a non-empty root is left untouched.
fn cleanup_install_dirs(bin: &Path) {
    if let Some(bindir) = bin.parent() {
        let _ = fs::remove_dir(bindir);
        if let Some(root) = bindir.parent() {
            let _ = fs::remove_dir(root);
        }
    }
}

fn binary_present(bin: &Path) -> bool {
    fs::metadata(bin).map(|m| m.is_file()).unwrap_or(false)
}

fn symlink_ok(bin: &Path) -> bool {
    match fs::read_link(config::symlink_path()) {
        Ok(target) => target == *bin || canon_eq(&config::symlink_path(), bin),
        Err(_) => false,
    }
}

fn canon_eq(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

fn daemon_running() -> bool {
    Command::new("pgrep")
        .args(["-f", "vigil daemon"])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false)
}

/// Stand the daemon down and wait for its clean exit. The `.disabled` sentinel
/// wakes the daemon reactively through its runtime-dir watch, and it exits
/// killing its caffeinate, where a signal would orphan the caffeinate until its
/// `-t` cap fired (ADR-0014). A daemon too wedged to see the flag within three
/// poll intervals falls back to pkill.
fn stop_daemon() {
    let _ = fs::create_dir_all(config::vigil_dir());
    let _ = fs::write(config::disable_flag_path(), b"");
    for _ in 0..30 {
        if lock_is_free() {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
    let _ = Command::new("pkill").args(["-f", "vigil daemon"]).status();
}

/// Whether the daemon's single-instance flock is unheld. The daemon holds it
/// for its lifetime, so an acquirable lock proves the daemon is gone, without
/// the false positives of a pgrep pattern match.
fn lock_is_free() -> bool {
    match fs::File::create(config::lock_path()) {
        Ok(file) => file.try_lock().is_ok(),
        // No lock file to open means no daemon holding it.
        Err(_) => true,
    }
}

// --- interactive I/O ----------------------------------------------------------

fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
}

fn print_install_plan(settings: &Value, plan: &Plan) {
    let header = if plan.stale {
        "vigil is installed at a different path. Move plan:"
    } else if hook_bins(settings).is_empty() {
        "vigil is not installed. Install plan:"
    } else if !plan.write_settings && !plan.fix_symlink {
        // Hooks and symlink are fine; only the binary is being rewritten (--force).
        "vigil is installed. Overwriting the binary (--force):"
    } else {
        "vigil is partially installed. Repair plan:"
    };
    println!("{header}\n");

    if plan.mode.is_managed() {
        let pm = if plan.mode == InstallMode::Cargo {
            "cargo"
        } else {
            "homebrew"
        };
        println!(
            "  binary         {}-managed at {}",
            pm,
            plan.desired_bin.display()
        );
    } else {
        if plan.copy_binary {
            match env::current_exe() {
                Ok(src) => println!("  copy binary    {}", src.display()),
                Err(_) => println!("  copy binary    <this executable>"),
            }
            println!("              -> {}", plan.desired_bin.display());
        } else {
            println!("  binary         present at {}", plan.desired_bin.display());
        }

        if plan.fix_symlink {
            println!(
                "  symlink        {} -> {}",
                config::symlink_path().display(),
                plan.desired_bin.display()
            );
        } else {
            println!("  symlink        ok");
        }
    }

    if plan.write_settings {
        let verb = if plan.stale {
            "repoint hooks "
        } else {
            "wire hooks    "
        };
        println!("  {verb} {}", config::settings_path().display());
        let names: Vec<&str> = EventKind::ALL.iter().map(|ev| ev.as_str()).collect();
        println!("                 {}", names.join(", "));
        println!(
            "              -> {} record <Event>",
            plan.desired_bin.display()
        );
        if config::settings_path().exists() {
            println!("  backup         settings.json.bak-<ts>  (all other hooks preserved)");
        }
    } else {
        println!(
            "  hooks          all {} already wired",
            EventKind::ALL.len()
        );
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin() -> PathBuf {
        PathBuf::from("/opt/vigil/bin/vigil")
    }

    #[test]
    fn managed_modes_leave_the_binary() {
        assert!(InstallMode::Cargo.is_managed());
        assert!(InstallMode::Homebrew.is_managed());
        assert!(!InstallMode::OwnCopy.is_managed());
    }

    #[test]
    fn dir_forces_own_copy_at_that_path() {
        let (target, mode) = resolve_target(Some(Path::new("/opt/vigil")), &json!({}));
        assert_eq!(target, PathBuf::from("/opt/vigil/bin/vigil"));
        assert_eq!(mode, InstallMode::OwnCopy);
    }

    #[test]
    fn managed_plan_skips_copy_and_symlink() {
        // A cargo install with hooks already wired at the cargo path is a noop:
        // no binary copy, no symlink, only settings if they differ.
        let cargo_bin = config::cargo_bin_path();
        let mut settings = json!({});
        insert_vigil(&mut settings, &cargo_bin);
        let plan = build_plan(&settings, &cargo_bin, InstallMode::Cargo, true);
        assert!(
            !plan.copy_binary,
            "managed install must not copy the binary"
        );
        assert!(
            !plan.fix_symlink,
            "managed install must not manage a symlink"
        );
        assert!(plan.is_noop(), "already-wired cargo install is a noop");
    }

    #[test]
    fn command_matching() {
        assert!(command_is_vigil("/opt/vigil/bin/vigil record PreToolUse"));
        assert!(command_is_vigil("vigil record Stop"));
        assert!(!command_is_vigil("/usr/bin/other record PreToolUse"));
        assert!(!command_is_vigil("/opt/vigil/bin/vigil daemon"));
        assert!(!command_is_vigil("echo vigil record"));
    }

    #[test]
    fn insert_then_strip_round_trips_external_hooks() {
        let mut settings = json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [ { "type": "command", "command": "/other/log.sh" } ] }
                ]
            },
            "model": "sonnet"
        });
        let original = settings.clone();

        insert_vigil(&mut settings, &bin());
        // External hook and unrelated key preserved.
        assert_eq!(settings["model"], json!("sonnet"));
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], json!("/other/log.sh"));
        // All events wired at the target path.
        assert_eq!(hook_bins(&settings).len(), EventKind::ALL.len());
        assert_eq!(
            consistent_hook_bin(&settings).as_deref(),
            Some(bin().as_path())
        );

        strip_vigil(&mut settings);
        assert_eq!(settings, original);
    }

    #[test]
    fn desired_is_noop_when_already_consistent() {
        let mut settings = json!({});
        insert_vigil(&mut settings, &bin());
        // A second pass yields the same document, so build_plan sees no write.
        assert_eq!(desired_settings(&settings, &bin()), settings);
    }

    #[test]
    fn strip_removes_only_vigil_and_prunes_empty() {
        let mut settings = json!({
            "hooks": {
                "Stop": [ { "hooks": [ { "type": "command", "command": "/x/vigil record Stop" } ] } ],
                "PreToolUse": [
                    { "hooks": [
                        { "type": "command", "command": "/x/vigil record PreToolUse" },
                        { "type": "command", "command": "/keep.sh" }
                    ] }
                ]
            }
        });
        strip_vigil(&mut settings);

        // Stop had only vigil, so the whole event array is pruned.
        assert!(settings["hooks"].get("Stop").is_none());
        // PreToolUse keeps its external hook.
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"], json!("/keep.sh"));
    }

    #[test]
    fn stale_path_is_detected() {
        let mut settings = json!({});
        insert_vigil(&mut settings, Path::new("/old/vigil"));
        assert_eq!(
            consistent_hook_bin(&settings).as_deref(),
            Some(Path::new("/old/vigil"))
        );
        // A different desired path forces a rewrite.
        assert_ne!(desired_settings(&settings, &bin()), settings);
    }
}
