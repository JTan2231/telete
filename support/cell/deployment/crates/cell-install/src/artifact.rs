use crate::{Error, FORMAT, InstallSpec, Installation, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub(crate) fn current_uid() -> Result<u32> {
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()?;
    let uid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| Error::new("cannot establish operator identity"))?;
    if !output.status.success() || uid == 0 {
        return Err(Error::new("installation requires a non-root current user"));
    }
    Ok(uid)
}

#[derive(Clone, Debug)]
pub struct ReleaseInput {
    pub binaries: BTreeMap<String, PathBuf>,
    pub provider_dir: PathBuf,
    pub installer: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Retained predecessor metadata. New releases do not calculate this field.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    pub mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_identifier: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub product: String,
    pub provider: String,
    pub versions: BTreeMap<String, String>,
    pub files: BTreeMap<String, FileEntry>,
    pub release_id: String,
}

pub(crate) fn regular(path: &Path) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(Error::new("artifact is not a regular single-link file"));
    }
    Ok(meta)
}

pub(crate) fn digest(path: &Path) -> Result<String> {
    regular(path)?;
    let mut input = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 65536];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Hash a regular single-link artifact without accepting a symbolic link.
///
/// # Errors
/// Returns an error for inaccessible, symbolic, hard-linked, or special files.
pub fn file_digest(path: &Path) -> Result<String> {
    digest(path)
}

pub(crate) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Check the literal selector format for current and retained releases.
#[must_use]
pub fn valid_release_id(value: &str) -> bool {
    valid_hash(value) || uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}

pub(crate) fn inventory_matches(
    actual: &BTreeMap<String, FileEntry>,
    recorded: &BTreeMap<String, FileEntry>,
) -> bool {
    actual.len() == recorded.len()
        && actual.iter().all(|(path, file)| {
            recorded
                .get(path)
                .is_some_and(|entry| entry.mode == file.mode)
        })
}

pub(crate) fn simple_format(value: &str) -> bool {
    matches!(value, FORMAT | "cell-install-v1")
}

pub(crate) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

pub(crate) fn validate_spec(spec: &InstallSpec) -> Result<()> {
    if !valid_name(spec.product)
        || !valid_name(spec.provider)
        || spec.commands.is_empty()
        || spec.commands.iter().any(|name| !valid_name(name))
        || spec.commands.iter().collect::<BTreeSet<_>>().len() != spec.commands.len()
        || spec.application.is_empty()
        || spec.application.contains(['/', '\n', '\r'])
        || matches!(spec.application, "." | "..")
    {
        return Err(Error::new("invalid installation specification"));
    }
    Ok(())
}

pub(crate) fn directory(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(Error::new("artifact directory is symbolic or invalid"));
    }
    Ok(())
}

pub(crate) fn inventory(root: &Path) -> Result<(BTreeMap<String, FileEntry>, BTreeSet<String>)> {
    directory(root)?;
    let mut files = BTreeMap::new();
    let mut dirs = BTreeSet::new();
    visit(root, root, &mut files, &mut dirs)?;
    Ok((files, dirs))
}

fn visit(
    root: &Path,
    at: &Path,
    files: &mut BTreeMap<String, FileEntry>,
    dirs: &mut BTreeSet<String>,
) -> Result<()> {
    for entry in fs::read_dir(at)? {
        let path = entry?.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| Error::new("invalid artifact path"))?
            .to_str()
            .ok_or_else(|| Error::new("artifact path is not UTF-8"))?
            .to_owned();
        if relative.contains(['\n', '\r', '\\'])
            || path.components().any(|c| matches!(c, Component::ParentDir))
        {
            return Err(Error::new("invalid artifact path"));
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            dirs.insert(relative);
            visit(root, &path, files, dirs)?;
        } else {
            regular(&path)?;
            files.insert(
                relative,
                FileEntry {
                    sha256: String::new(),
                    mode: meta.mode() & 0o7777,
                    code_identifier: None,
                },
            );
        }
    }
    Ok(())
}

pub(crate) fn provider_files(
    root: &Path,
    _spec: &InstallSpec,
) -> Result<BTreeMap<String, FileEntry>> {
    Ok(inventory(root)?.0)
}

/// List provider file paths and modes for copying.
///
/// # Errors
/// Returns an error when an input cannot be read.
pub fn provider_inventory(root: &Path, spec: &InstallSpec) -> Result<BTreeMap<String, FileEntry>> {
    provider_files(root, spec)
}

pub(crate) fn copy_file(source: &Path, destination: &Path, mode: u32) -> Result<()> {
    fs::copy(source, destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub(crate) fn write_manifest(
    root: &Path,
    spec: &InstallSpec,
    versions: BTreeMap<String, String>,
) -> Result<Manifest> {
    let (files, _) = inventory(root)?;
    let manifest = Manifest {
        format: FORMAT.to_owned(),
        product: spec.product.to_owned(),
        provider: spec.provider.to_owned(),
        versions,
        files,
        release_id: uuid::Uuid::now_v7().to_string(),
    };
    fs::write(root.join("manifest.json"), serde_json::to_vec(&manifest)?)?;
    fs::set_permissions(
        root.join("manifest.json"),
        fs::Permissions::from_mode(0o444),
    )?;
    Ok(manifest)
}

/// Read retained release metadata without checking artifact integrity.
///
/// # Errors
/// Returns an error for inaccessible metadata, unsupported formats, or paths.
pub fn read_release(spec: &InstallSpec, release: &Path) -> Result<Installation> {
    validate_spec(spec)?;
    if !release.is_absolute() || fs::canonicalize(release)? != release {
        return Err(Error::new("release path must be absolute and non-symbolic"));
    }
    let id = release
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::new("invalid release path"))?;
    let (version, format) = if release.join("manifest.json").try_exists()? {
        let path = release.join("manifest.json");
        let manifest: Manifest = serde_json::from_slice(&fs::read(&path)?)?;
        if !simple_format(&manifest.format)
            || manifest.product != spec.product
            || manifest.provider != spec.provider
            || manifest.release_id != id
            || !valid_release_id(id)
        {
            return Err(Error::new("unsupported release format"));
        }
        let version = manifest
            .versions
            .get(spec.commands[0])
            .cloned()
            .unwrap_or_default();
        (version, manifest.format)
    } else {
        let path = release.join("manifest.txt");
        let manifest = crate::legacy::manifest(&path)?;
        (
            manifest.get("version").cloned().unwrap_or_default(),
            "legacy-usher-v1".to_owned(),
        )
    };
    Ok(Installation {
        current: format!("releases/{id}"),
        release_id: id.to_owned(),
        version,
        format,
    })
}

pub(crate) fn verify_tree_ownership(path: &Path, uid: u32) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.uid() != uid || meta.mode() & 0o022 != 0 || meta.file_type().is_symlink() {
        return Err(Error::new(
            "release artifact ownership or permissions are unsafe",
        ));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            verify_tree_ownership(&entry?.path(), uid)?;
        }
    } else {
        regular(path)?;
    }
    Ok(())
}
