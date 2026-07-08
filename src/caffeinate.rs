//! The one `caffeinate` child owned by the daemon.

use std::process::{Child, Command};

use crate::config;
use crate::error::Error;

/// Owns at most one `caffeinate -di -t SAFETY_SECS` process. `-d` prevents
/// display sleep, `-i` prevents system idle sleep, `-t` is the self-expiry
/// backstop if the daemon dies without cleanup.
#[derive(Default)]
pub struct Caffeinate {
    child: Option<Child>,
}

impl Caffeinate {
    /// Start a caffeinate if none is live. Respawns when the previous child has
    /// exited, which happens if its `-t` safety cap fired during a hold longer
    /// than SAFETY_SECS.
    pub fn ensure_running(&mut self) -> Result<(), Error> {
        let live = match self.child.as_mut() {
            Some(child) => child.try_wait()?.is_none(),
            None => false,
        };
        if !live {
            self.child = Some(spawn()?);
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn() -> Result<Child, Error> {
    let child = Command::new("caffeinate")
        .arg("-di")
        .arg("-t")
        .arg(config::SAFETY_SECS.to_string())
        .spawn()?;
    Ok(child)
}
