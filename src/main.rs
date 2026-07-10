mod caffeinate;
mod cli;
mod config;
mod daemon;
mod error;
mod event;
mod install;
mod proc;
mod watch;

use std::io::Read;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Commands};
use crate::error::Error;
use crate::event::EventKind;

fn main() -> ExitCode {
    // Rust ignores SIGPIPE at startup, which turns a closed stdout (`vigil status
    // | head`) into an EPIPE panic. Restore the default so we exit quietly like
    // any other Unix tool. Safe: the daemon's stdio is /dev/null and child
    // processes carry their own.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(err.exit_code())
        }
    }
}

fn run() -> Result<ExitCode, Error> {
    let cli = Cli::parse();

    match cli.command {
        // Bare `vigil`: install if needed, else report install state.
        None => {
            install::bootstrap()?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Record { event }) => {
            // Never block a turn: record failures go to stderr and exit 0.
            if let Err(err) = record(event) {
                eprintln!("vigil record: {err}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Daemon) => daemon::run(),
        Some(Commands::Status) => {
            daemon::status()?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Install { dir, force, yes }) => {
            install::install(dir, force, yes)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Commands::Uninstall { yes }) => {
            install::uninstall(yes)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn record(event: EventKind) -> Result<(), Error> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    event::record(event, &input)?;
    daemon::ensure_running();
    Ok(())
}
