mod caffeinate;
mod cli;
mod config;
mod daemon;
mod error;
mod event;
mod proc;
mod watch;

use std::io::Read;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Commands};
use crate::error::Error;
use crate::event::EventKind;

fn main() -> ExitCode {
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
        Commands::Record { event } => {
            // Never block a turn: record failures go to stderr and exit 0.
            if let Err(err) = record(event) {
                eprintln!("vigil record: {err}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Commands::Daemon => daemon::run(),
        Commands::Status => {
            daemon::status()?;
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
