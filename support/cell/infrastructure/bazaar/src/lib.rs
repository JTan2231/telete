//! Versioned opaque strings, with an in-process `SQLite` API and a local CLI.
//!
//! [`api::Reader`] opens existing state read-only. [`api::Writer`] initializes
//! state and appends versions. [`prompts`] selects and renders prompt components;
//! callers own their meaning and execution policy.

pub mod api;
pub mod installation;
mod prompt_import;
pub mod prompts;

use std::path::{Path, PathBuf};

/// The default database beneath the selected user's home.
#[must_use]
pub fn database_path(home: &Path) -> PathBuf {
    home.join(".local/share/bazaar/bazaar.sqlite3")
}
