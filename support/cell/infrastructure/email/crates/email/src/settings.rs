//! Email-owned local setup and receiving-domain discovery. No mail is read.
use crate::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivingSettings {
    pub domains: Vec<String>,
}

#[derive(Default, Deserialize, clap::Parser)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    #[arg(long)]
    pub receiving_domain: Option<String>,
    #[arg(long)]
    pub credential_file: Option<PathBuf>,
}

fn failure() -> AppError {
    AppError::new("Email setup is unavailable or invalid")
}

fn root() -> AppResult<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(failure)?;
    Ok(std::fs::canonicalize(home)
        .map_err(|_| failure())?
        .join("Library/Application Support/Email/settings"))
}

pub(crate) fn validate_domain(domain: &str) -> AppResult<()> {
    if domain.len() > 253
        || !domain.contains('.')
        || !domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(AppError::new("receiving domain must be a DNS name"));
    }
    Ok(())
}

fn read_private(path: &Path, limit: u64) -> AppResult<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| failure())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(AppError::new(
            "Email settings and supplied credential files must be private regular files",
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| failure())?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure())?;
    if bytes.len() as u64 > limit {
        return Err(failure());
    }
    Ok(bytes)
}

fn credential(bytes: Vec<u8>) -> AppResult<String> {
    let value = String::from_utf8(bytes).map_err(|_| failure())?;
    let value = value.trim();
    if value.is_empty() || value.len() > 4096 || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(failure());
    }
    Ok(value.to_owned())
}

/// Validate setup inputs without changing settings or reading account mail.
/// # Errors
/// Rejects invalid domains and unsafe or missing supplied credential files.
pub fn validate(setup: &Setup) -> AppResult<()> {
    if let Some(domain) = &setup.receiving_domain {
        validate_domain(domain)?;
    }
    if let Some(path) = &setup.credential_file {
        if !path.is_absolute() {
            return Err(failure());
        }
        credential(read_private(path, 4096)?)?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let parent = path.parent().ok_or_else(failure)?;
    for ancestor in parent.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err(failure());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(failure()),
        }
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|_| failure())?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|_| failure())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.mode() & 0o077 != 0 {
        return Err(failure());
    }
    if path.exists() {
        read_private(path, 65536)?;
    }
    let pending = parent.join(format!(".setup-{}", uuid::Uuid::now_v7()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)
        .map_err(|_| failure())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| failure())?;
    std::fs::rename(&pending, path).map_err(|_| failure())?;
    std::fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|_| failure())
}

/// Install supplied local settings. Credentials never enter receipts or definitions.
/// # Errors
/// Returns invalid inputs or unavailable private storage.
pub fn configure(setup: &Setup) -> AppResult<()> {
    validate(setup)?;
    let root = root()?;
    if let Some(path) = &setup.credential_file {
        let key = credential(read_private(path, 4096)?)?;
        write_private(&root.join("resend-api-key"), key.as_bytes())?;
    }
    if let Some(domain) = &setup.receiving_domain {
        write_private(
            &root.join("receiving.json"),
            &serde_json::to_vec(&ReceivingSettings {
                domains: vec![domain.to_ascii_lowercase()],
            })
            .map_err(|_| failure())?,
        )?;
    }
    Ok(())
}

pub(crate) fn configured_credential() -> AppResult<Option<String>> {
    let path = root()?.join("resend-api-key");
    match std::fs::symlink_metadata(&path) {
        Ok(_) => credential(read_private(&path, 4096)?).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(failure()),
    }
}

#[derive(Deserialize)]
struct DomainPage {
    data: Vec<Domain>,
    has_more: bool,
}
#[derive(Deserialize)]
struct Domain {
    id: String,
    name: String,
    capabilities: Capabilities,
}
#[derive(Deserialize)]
struct Capabilities {
    receiving: String,
}
#[derive(Deserialize)]
struct DomainDetail {
    records: Vec<Record>,
}
#[derive(Deserialize)]
struct Record {
    record: String,
    status: String,
}

pub(crate) async fn receiving_settings() -> AppResult<ReceivingSettings> {
    let path = root()?.join("receiving.json");
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let settings: ReceivingSettings =
                serde_json::from_slice(&read_private(&path, 65536)?).map_err(|_| failure())?;
            for domain in &settings.domains {
                validate_domain(domain)?;
            }
            return Ok(settings);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(failure()),
    }
    let mut domains = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut query = vec![("limit", "100".to_owned())];
    for _ in 0..10 {
        let page: DomainPage =
            crate::receiving::get_json("https://api.resend.com/domains", &query).await?;
        if page.data.len() > 100 || page.has_more && page.data.is_empty() {
            return Err(failure());
        }
        let last = page.data.last().map(|d| d.id.clone());
        for domain in page.data {
            crate::receiving::validate_id(&domain.id)?;
            if !seen.insert(domain.id.clone()) {
                return Err(failure());
            }
            if domain.capabilities.receiving == "enabled" {
                validate_domain(&domain.name)?;
                let detail: DomainDetail = crate::receiving::get_json(
                    &format!("https://api.resend.com/domains/{}", domain.id),
                    &[],
                )
                .await?;
                if detail
                    .records
                    .iter()
                    .any(|record| record.record == "Receiving MX" && record.status == "verified")
                {
                    domains.insert(domain.name.to_ascii_lowercase());
                }
            }
        }
        if !page.has_more {
            return Ok(ReceivingSettings {
                domains: domains.into_iter().collect(),
            });
        }
        query = vec![
            ("limit", "100".into()),
            ("after", last.ok_or_else(failure)?),
        ];
    }
    Err(AppError::new(
        "receiving domain discovery exceeds 1000 domains; supply receiving_domain explicitly",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_refuse_addresses_urls_and_empty_labels() {
        for value in [
            "a@domain.example",
            "https://domain.example",
            "a..example",
            "-a.example",
            "a.example\n",
        ] {
            assert!(validate_domain(value).is_err());
        }
        assert!(validate_domain("account.resend.app").is_ok());
    }
}
