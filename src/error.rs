use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Process exit code. Usage errors (unknown event, bad arguments) are
    /// handled by clap and exit 2; these are runtime failures on the daemon and
    /// status paths. `record` swallows its own errors and always exits 0.
    pub fn exit_code(&self) -> u8 {
        1
    }
}
