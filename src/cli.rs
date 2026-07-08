use clap::{Parser, Subcommand};

use crate::event::EventKind;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
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
}
