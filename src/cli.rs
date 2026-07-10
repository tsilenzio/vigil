use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::event::EventKind;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// With no subcommand, install if needed or report install state
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Record a session lifecycle event from a Claude Code hook (reads hook JSON
    /// on stdin), then ensure the daemon is running
    Record {
        /// The hook event name
        event: EventKind,
    },

    /// Run the supervisor loop (normally spawned detached by `record`)
    Daemon,

    /// Print active sessions, assertion state, and power state
    Status,

    /// Install the binary and wire the Claude Code hooks. Idempotent: a noop
    /// when already installed and consistent
    Install {
        /// Install root (default `${XDG_DATA_HOME}/vigil` or `$VIGIL_INSTALL_DIR`)
        #[arg(long)]
        dir: Option<PathBuf>,

        /// Overwrite the installed binary even if one is already present
        #[arg(long)]
        force: bool,

        /// Skip the confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Remove vigil's hooks, binary, and runtime state, leaving other hooks intact
    Uninstall {
        /// Skip the confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
}
