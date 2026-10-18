//! Durable completion receipts for product-owned deployment migrations.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

#[derive(Deserialize, Serialize)]
struct Receipt {
    schema: u32,
}

/// Record migration completion so a resumed deployment does not repeat it.
/// The product's migration must itself recover an interrupted call when no
/// receipt exists. Later configuration and admitted work may change live state.
///
/// # Errors
/// Refuses invalid receipt paths or migration failure.
pub fn install_once(receipt: &Path, migrate: impl FnOnce() -> Result<()>) -> Result<()> {
    if !receipt.is_absolute() {
        return Err(Error::new("migration receipt path must be absolute"));
    }
    let parent = receipt
        .parent()
        .ok_or_else(|| Error::new("migration receipt parent missing"))?;
    let owner = std::fs::symlink_metadata(parent)?;
    if !owner.is_dir() || owner.file_type().is_symlink() || owner.mode() & 0o077 != 0 {
        return Err(Error::new("migration receipt directory must be private"));
    }
    match std::fs::symlink_metadata(receipt) {
        Ok(metadata) => {
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.nlink() != 1
                || metadata.uid() != owner.uid()
                || metadata.mode() & 0o077 != 0
            {
                return Err(Error::new("migration receipt must be a private owned file"));
            }
            let saved: Receipt = serde_json::from_slice(&std::fs::read(receipt)?)?;
            if !matches!(saved.schema, 1 | 2) {
                return Err(Error::new("unsupported migration receipt schema"));
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    migrate()?;
    let saved = Receipt { schema: 2 };
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    pending.write_all(&serde_json::to_vec(&saved)?)?;
    pending.as_file().sync_all()?;
    pending
        .persist_noclobber(receipt)
        .map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
