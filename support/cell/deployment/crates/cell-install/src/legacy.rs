//! Predecessor release metadata shared by product-owned readers.

use crate::artifact::{current_uid, inventory, valid_release_id};
use crate::transaction::{PublicEntry, ReleaseInfo};
use crate::{Error, FileEntry, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub struct LegacySpec {
    pub format: &'static str,
    pub manifest: &'static str,
    pub metadata: &'static [&'static str],
    pub proofs: &'static [LegacyProof],
    pub providers: &'static [LegacyProvider],
    pub hash_path_lines: bool,
}

pub struct LegacyProof {
    pub key: &'static str,
    pub paths: &'static [&'static str],
}

pub struct LegacyProvider {
    pub key: &'static str,
    pub provider: &'static str,
    pub path: &'static str,
    pub version_key: &'static str,
}

fn retain_alias_metadata(
    files: &mut BTreeMap<String, FileEntry>,
    values: &BTreeMap<String, String>,
    spec: &LegacySpec,
) {
    for proof in spec.proofs {
        if let Some(value) = values.get(proof.key) {
            for path in proof.paths {
                if let Some(file) = files.get_mut(*path) {
                    file.sha256.clone_from(value);
                }
            }
        }
    }
}

/// Decode a regular predecessor manifest without inferring any missing fields.
///
/// # Errors
/// Rejects malformed JSON/text, duplicate text keys, and nonscalar JSON fields.
pub fn manifest(path: &Path) -> Result<BTreeMap<String, String>> {
    let bytes = fs::read(path)?;
    if bytes.len() > 1024 * 1024 {
        return Err(Error::new("legacy manifest exceeds one MiB"));
    }
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        let value: Value = serde_json::from_slice(&bytes)?;
        value
            .as_object()
            .ok_or_else(|| Error::new("invalid legacy manifest"))?
            .iter()
            .map(|(key, value)| {
                let text = value
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| value.as_u64().map(|v| v.to_string()))
                    .or_else(|| value.as_bool().map(|v| v.to_string()))
                    .ok_or_else(|| Error::new("invalid legacy manifest value"))?;
                Ok((key.clone(), text))
            })
            .collect()
    } else {
        let text =
            String::from_utf8(bytes).map_err(|_| Error::new("legacy manifest is not UTF-8"))?;
        let mut result = BTreeMap::new();
        for line in text.lines() {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| Error::new("invalid legacy manifest line"))?;
            if key.is_empty() || result.insert(key.to_owned(), value.to_owned()).is_some() {
                return Err(Error::new("duplicate legacy manifest key"));
            }
        }
        Ok(result)
    }
}

/// Read predecessor release metadata and file paths for installation setup.
///
/// # Errors
/// Returns an error for inaccessible files or malformed metadata.
pub fn read(root: &Path, spec: &LegacySpec, public: Vec<PublicEntry>) -> Result<ReleaseInfo> {
    let values = manifest(&root.join(spec.manifest))?;
    let (mut files, _) = inventory(root)?;
    retain_alias_metadata(&mut files, &values, spec);
    let mut versions = BTreeMap::new();
    for provider in spec.providers {
        let version = if provider.version_key.is_empty() {
            let data: Value =
                serde_json::from_slice(&fs::read(root.join(provider.path).join("provider.json"))?)?;
            data.pointer("/provider/release")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        } else {
            values
                .get(provider.version_key)
                .cloned()
                .unwrap_or_default()
        };
        versions.insert(provider.provider.to_owned(), version);
    }
    Ok(ReleaseInfo {
        release_id: root
            .file_name()
            .and_then(|p| p.to_str())
            .ok_or_else(|| Error::new("invalid release path"))?
            .to_owned(),
        format: values.get("format").cloned().unwrap_or_default(),
        versions,
        files,
        public,
    })
}

/// Check the predecessor's file inventory, modes and recorded identity.
///
/// # Errors
/// Refuses unknown keys/files, symbolic/hard-linked artifacts,
/// unsafe owner/modes, and provider/version or release-identity disagreement.
#[allow(clippy::too_many_lines)] // Keep the exact predecessor proof in one auditable sequence.
pub fn verify(root: &Path, spec: &LegacySpec, public: Vec<PublicEntry>) -> Result<ReleaseInfo> {
    let values = manifest(&root.join(spec.manifest))?;
    let expected_keys: BTreeSet<_> = ["format", "release_id"]
        .into_iter()
        .chain(spec.metadata.iter().copied())
        .chain(spec.proofs.iter().map(|p| p.key))
        .chain(spec.providers.iter().map(|p| p.key))
        .collect();
    if values.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_keys
        || values.get("format").map(String::as_str) != Some(spec.format)
    {
        return Err(Error::new("unsupported legacy manifest fields or format"));
    }
    let (mut files, directories) = inventory(root)?;
    retain_alias_metadata(&mut files, &values, spec);
    let uid = current_uid()?;
    for path in std::iter::once(root.to_path_buf())
        .chain(directories.iter().map(|p| root.join(p)))
        .chain(files.keys().map(|p| root.join(p)))
    {
        let info = fs::symlink_metadata(path)?;
        if info.uid() != uid || info.mode() & 0o7022 != 0 {
            return Err(Error::new("unsafe legacy release ownership or modes"));
        }
    }
    let mut expected_files = BTreeSet::from([spec.manifest.to_owned()]);
    for proof in spec.proofs {
        if proof.paths.is_empty() {
            return Err(Error::new("invalid legacy proof"));
        }
        for path in proof.paths {
            if !files.contains_key(*path) {
                return Err(Error::new("legacy artifact missing from inventory"));
            }
            expected_files.insert((*path).to_owned());
        }
    }
    let mut versions = BTreeMap::new();
    for provider in spec.providers {
        let prefix = format!("{}/", provider.path);
        for path in files.keys().filter(|path| path.starts_with(&prefix)) {
            expected_files.insert(path.clone());
        }
        let data: Value =
            serde_json::from_slice(&fs::read(root.join(provider.path).join("provider.json"))?)?;
        let version = if provider.version_key.is_empty() {
            data.pointer("/provider/release").and_then(Value::as_str)
        } else {
            values.get(provider.version_key).map(String::as_str)
        }
        .ok_or_else(|| Error::new("legacy provider version missing"))?;
        if data.pointer("/provider/id").and_then(Value::as_str) != Some(provider.provider)
            || data.pointer("/provider/release").and_then(Value::as_str) != Some(version)
        {
            return Err(Error::new("legacy provider identity or version differs"));
        }
        versions.insert(provider.provider.to_owned(), version.to_owned());
    }
    let release_id = values
        .get("release_id")
        .filter(|id| valid_release_id(id))
        .ok_or_else(|| Error::new("invalid legacy release identity"))?
        .clone();
    if root.file_name().and_then(|p| p.to_str()) != Some(release_id.as_str())
        || files.keys().cloned().collect::<BTreeSet<_>>() != expected_files
    {
        return Err(Error::new(
            "legacy release identity or exact inventory differs",
        ));
    }
    let mut expected_directories = BTreeSet::new();
    for file in &expected_files {
        let mut parent = Path::new(file).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            expected_directories.insert(path.to_string_lossy().into_owned());
            parent = path.parent();
        }
    }
    if directories != expected_directories {
        return Err(Error::new("legacy release has unmanifested directories"));
    }
    Ok(ReleaseInfo {
        release_id,
        format: spec.format.to_owned(),
        versions,
        files,
        public,
    })
}
