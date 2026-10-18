//! Owned, current-user program installation. Runtime maintenance and database
//! recovery remain the caller's responsibility.

pub mod adapter;
mod artifact;
pub mod command;
mod installation;
pub mod legacy;
pub mod migration;
pub mod simple;
pub mod transaction;

pub use transaction::*;

pub use artifact::{
    FileEntry, Manifest, ReleaseInput, file_digest, provider_inventory, read_release,
    valid_release_id,
};
#[allow(deprecated)] // Retained callers can read and recover the former layout.
pub use installation::{inspect, install, recover_installation, restore};

use serde::{Deserialize, Serialize};
use std::fmt;

pub const FORMAT: &str = "cell-install-simple-v2";

/// The first command supplies the version of this installation's provider.
#[derive(Clone, Copy, Debug)]
pub struct InstallSpec {
    pub product: &'static str,
    pub application: &'static str,
    pub commands: &'static [&'static str],
    pub provider: &'static str,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Installation {
    pub current: String,
    pub release_id: String,
    pub version: String,
    pub format: String,
}

// Installation concurrency concerns the selected release, not its recorded metadata.
impl PartialEq for Installation {
    fn eq(&self, other: &Self) -> bool {
        self.current == other.current
    }
}

impl Eq for Installation {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Unchanged,
    Restored,
    Detached,
    Uncertain,
}

#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub disposition: Disposition,
}

impl Error {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            disposition: Disposition::Unchanged,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{} ({:?})", self.message, self.disposition)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(format!(
            "installation filesystem operation failed: {}",
            error.kind()
        ))
    }
}

impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::new("invalid installation JSON")
    }
}

pub type Result<T> = std::result::Result<T, Error>;
