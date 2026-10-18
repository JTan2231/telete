//! Explicit file installation transactions. Product lifecycle remains in the caller.

use crate::artifact::{
    copy_file, current_uid, inventory, inventory_matches, valid_release_id, verify_tree_ownership,
};
use crate::{Disposition, Error, FileEntry, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

pub const TRANSACTION_FORMAT: &str = "cell-install-v4";

/// Recognize current and retained shared transaction release formats.
#[must_use]
pub fn is_transaction_format(value: &str) -> bool {
    matches!(
        value,
        TRANSACTION_FORMAT | "cell-install-v3" | "cell-install-v2"
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockKind {
    Shlock,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockSpec {
    pub path: PathBuf,
    pub kind: LockKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicKind {
    Symlink,
    Copy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicEntry {
    /// Path relative to the operator home.
    pub path: PathBuf,
    /// File or provider directory relative to the immutable release.
    pub artifact: String,
    pub kind: PublicKind,
    pub mode: u32,
}

#[derive(Clone, Debug)]
pub struct InstallLayout {
    pub product: String,
    pub application: String,
    pub product_lock: LockSpec,
    pub catalog_lock: Option<LockSpec>,
    pub public: Vec<PublicEntry>,
}

#[derive(Clone, Debug)]
pub struct SourceFile {
    pub source: PathBuf,
    pub mode: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSpec {
    pub path: String,
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct ReleasePlan {
    pub files: BTreeMap<String, SourceFile>,
    pub versions: BTreeMap<String, String>,
    pub providers: BTreeMap<String, ProviderSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseInfo {
    pub release_id: String,
    pub format: String,
    pub versions: BTreeMap<String, String>,
    pub files: BTreeMap<String, FileEntry>,
    pub public: Vec<PublicEntry>,
}

#[derive(Clone, Debug)]
pub struct PreparedRelease {
    pub root: PathBuf,
    pub info: ReleaseInfo,
}

pub type LegacyVerifier<'a> = dyn Fn(&Path) -> Result<ReleaseInfo> + 'a;

pub type LegacyReader<'a> = dyn Fn(&Path) -> Result<ReleaseInfo> + 'a;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum CapturedEntry {
    Absent,
    Link(PathBuf),
    Copy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSnapshot {
    pub current: Option<ReleaseInfo>,
    pub previous: Option<ReleaseInfo>,
    entries: BTreeMap<PathBuf, CapturedEntry>,
}

fn selection(release: Option<&ReleaseInfo>) -> Option<&str> {
    release.map(|release| release.release_id.as_str())
}

// Concurrency guards compare selection and public paths, not recorded artifact metadata.
impl PartialEq for InstallSnapshot {
    fn eq(&self, other: &Self) -> bool {
        selection(self.current.as_ref()) == selection(other.current.as_ref())
            && selection(self.previous.as_ref()) == selection(other.previous.as_ref())
            && matches_view(&self.entries, &other.entries)
    }
}

impl Eq for InstallSnapshot {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectionReceipt {
    pub before: InstallSnapshot,
    pub after: InstallSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    product: String,
    versions: BTreeMap<String, String>,
    providers: BTreeMap<String, ProviderSpec>,
    files: BTreeMap<String, FileEntry>,
    public: Vec<PublicEntry>,
    release_id: String,
}

impl Manifest {
    fn info(&self) -> ReleaseInfo {
        ReleaseInfo {
            release_id: self.release_id.clone(),
            format: self.format.clone(),
            versions: self.versions.clone(),
            files: self.files.clone(),
            public: self.public.clone(),
        }
    }
}

fn relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || path.to_str().is_none_or(|p| p.contains(['\n', '\r', '\\']))
    {
        return Err(Error::new(
            "installation path must be a literal relative path",
        ));
    }
    Ok(())
}

fn install_root(layout: &InstallLayout, home: &Path) -> PathBuf {
    home.join("Library/Application Support")
        .join(&layout.application)
        .join("install")
}

/// Fixed physical payload location. Release selectors only identify retained archives.
#[must_use]
pub fn runtime_root(layout: &InstallLayout, home: &Path) -> PathBuf {
    install_root(layout, home).join("runtime")
}

fn runtime_entries(layout: &InstallLayout, release: &ReleaseInfo) -> Vec<PublicEntry> {
    let root = PathBuf::from("Library/Application Support")
        .join(&layout.application)
        .join("install/runtime");
    release
        .files
        .iter()
        .map(|(artifact, file)| PublicEntry {
            path: root.join(artifact),
            artifact: artifact.clone(),
            kind: PublicKind::Copy,
            mode: file.mode,
        })
        .collect()
}

fn owned_entries(layout: &InstallLayout, release: &ReleaseInfo) -> Vec<PublicEntry> {
    release
        .public
        .iter()
        .cloned()
        .chain(runtime_entries(layout, release))
        .collect()
}

fn validate_layout(layout: &InstallLayout, home: &Path) -> Result<u32> {
    if !crate::artifact::valid_name(&layout.product)
        || layout.application.is_empty()
        || layout.application.contains(['/', '\n', '\r'])
        || matches!(layout.application.as_str(), "." | "..")
        || !home.is_absolute()
        || fs::canonicalize(home)? != home
    {
        return Err(Error::new("invalid installation layout or operator home"));
    }
    let uid = current_uid()?;
    safe_directory(home, uid)?;
    relative(&layout.product_lock.path)?;
    if let Some(lock) = &layout.catalog_lock {
        relative(&lock.path)?;
    }
    validate_public(&layout.public)?;
    Ok(uid)
}

fn validate_public(entries: &[PublicEntry]) -> Result<()> {
    let mut paths = BTreeSet::new();
    for entry in entries {
        relative(&entry.path)?;
        relative(Path::new(&entry.artifact))?;
        if !paths.insert(&entry.path) || entry.mode & !0o777 != 0 || entry.mode & 0o022 != 0 {
            return Err(Error::new("invalid or duplicate public installation entry"));
        }
    }
    Ok(())
}

fn safe_directory(path: &Path, uid: u32) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(Error::new("unsafe installation directory"));
    }
    Ok(())
}

fn ensure_path(home: &Path, relative_path: &Path, uid: u32, create: bool) -> Result<()> {
    relative(relative_path)?;
    let mut path = home.to_owned();
    for component in relative_path.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(_) => safe_directory(&path, uid)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                match fs::create_dir(&path) {
                    Ok(()) => fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
                safe_directory(&path, uid)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

enum HeldLock {
    Pid {
        _guard: crate::installation::Lock,
    },
    Directory {
        path: PathBuf,
        device: u64,
        inode: u64,
        owner: Vec<u8>,
    },
}

impl HeldLock {
    fn acquire(home: &Path, spec: &LockSpec, uid: u32) -> Result<Self> {
        relative(&spec.path)?;
        if let Some(parent) = spec.path.parent() {
            ensure_path(home, parent, uid, true)?;
        }
        let path = home.join(&spec.path);
        match spec.kind {
            LockKind::Shlock => Ok(Self::Pid {
                _guard: crate::installation::Lock::acquire(path, uid)?,
            }),
            LockKind::Directory => {
                if let Err(error) = fs::create_dir(&path) {
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                    reclaim_directory_lock(&path, uid)?;
                    fs::create_dir(&path)?;
                }
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                let owner = format!("cell-install-lock-v1\n{}\n", std::process::id()).into_bytes();
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path.join(LOCK_OWNER))?;
                file.write_all(&owner)?;
                file.sync_all()?;
                let metadata = fs::symlink_metadata(&path)?;
                Ok(Self::Directory {
                    path,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    owner,
                })
            }
        }
    }
}

const LOCK_OWNER: &str = ".cell-install-owner";

fn directory_lock_owner(path: &Path, uid: u32) -> Result<(fs::Metadata, Vec<u8>)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(Error::new("unsafe product directory lock retained"));
    }
    let children = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    if children.len() != 1 || children[0].file_name() != LOCK_OWNER {
        return Err(Error::new(
            "unrecognized product directory lock requires explicit recovery",
        ));
    }
    let marker = path.join(LOCK_OWNER);
    let info = fs::symlink_metadata(&marker)?;
    if !info.is_file()
        || info.uid() != uid
        || info.mode() & 0o777 != 0o600
        || info.nlink() != 1
        || info.len() > 64
    {
        return Err(Error::new("unsafe product directory lock owner retained"));
    }
    Ok((metadata, fs::read(marker)?))
}

fn reclaim_directory_lock(path: &Path, uid: u32) -> Result<()> {
    let (metadata, owner) = directory_lock_owner(path, uid)?;
    let text =
        std::str::from_utf8(&owner).map_err(|_| Error::new("unknown lock owner retained"))?;
    let pid: u32 = text
        .strip_prefix("cell-install-lock-v1\n")
        .and_then(|value| value.strip_suffix('\n'))
        .and_then(|value| value.parse().ok())
        .filter(|pid| *pid > 0 && i32::try_from(*pid).is_ok())
        .ok_or_else(|| Error::new("unknown lock owner retained"))?;
    let status = std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .env("LC_ALL", "C")
        .output()?;
    if status.status.success()
        || !String::from_utf8_lossy(&status.stderr).contains("No such process")
    {
        return Err(Error::new("product directory lock is held"));
    }
    let (again, bytes) = directory_lock_owner(path, uid)?;
    if again.dev() != metadata.dev() || again.ino() != metadata.ino() || bytes != owner {
        return Err(Error::new("product directory lock changed during recovery"));
    }
    fs::remove_file(path.join(LOCK_OWNER))?;
    fs::remove_dir(path)?;
    Ok(())
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        if let Self::Directory {
            path,
            device,
            inode,
            owner,
        } = self
            && let Ok(uid) = current_uid()
            && let Ok((metadata, bytes)) = directory_lock_owner(path, uid)
            && metadata.dev() == *device
            && metadata.ino() == *inode
            && bytes == *owner
        {
            let _ = fs::remove_file(path.join(LOCK_OWNER));
            let _ = fs::remove_dir(path);
        }
    }
}

fn read_manifest(root: &Path) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    if !is_transaction_format(&manifest.format) {
        return Err(Error::new("unsupported installation format"));
    }
    validate_public(&manifest.public)?;
    Ok(manifest)
}

/// Read a release's recorded installation layout without checking its artifacts.
///
/// # Errors
/// Returns an error for unsupported metadata or invalid installation paths.
pub fn read_release_at(
    layout: &InstallLayout,
    root: &Path,
    legacy: &LegacyReader<'_>,
) -> Result<ReleaseInfo> {
    if !root.is_absolute() || fs::canonicalize(root)? != root {
        return Err(Error::new("release path must be canonical"));
    }
    let result = if root.join("manifest.json").try_exists()? {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
        if value
            .get("format")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_transaction_format)
        {
            read_manifest(root)?.info()
        } else {
            legacy(root)?
        }
    } else {
        legacy(root)?
    };
    validate_public(&result.public)?;
    validate_public(&owned_entries(layout, &result))?;
    Ok(result)
}

/// Stage explicit payloads under the product lock.
/// # Errors
/// Rejects invalid paths or modes, inaccessible inputs, or lock failures.
pub fn prepare_release(
    layout: &InstallLayout,
    home: &Path,
    plan: &ReleasePlan,
) -> Result<PreparedRelease> {
    let uid = validate_layout(layout, home)?;
    let root = install_root(layout, home);
    ensure_path(
        home,
        &root
            .strip_prefix(home)
            .map_err(|_| Error::new("invalid install root"))?
            .join("releases"),
        uid,
        true,
    )?;
    let stage = tempfile::Builder::new()
        .prefix(&format!(
            ".cell-install-stage-{}-{}-",
            layout.product,
            std::process::id()
        ))
        .tempdir_in(root.join("releases"))?;
    for (path, file) in &plan.files {
        relative(Path::new(path))?;
        if path == "manifest.json"
            || file.mode & !0o777 != 0
            || file.mode & 0o022 != 0
            || !file.source.is_absolute()
        {
            return Err(Error::new("invalid release input path or mode"));
        }
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(stage.path().join(parent))?;
        }
        copy_file(&file.source, &stage.path().join(path), file.mode)?;
    }
    let (files, dirs) = inventory(stage.path())?;
    for dir in dirs {
        fs::set_permissions(stage.path().join(dir), fs::Permissions::from_mode(0o755))?;
    }
    let manifest = Manifest {
        format: TRANSACTION_FORMAT.to_owned(),
        product: layout.product.clone(),
        versions: plan.versions.clone(),
        providers: plan.providers.clone(),
        files,
        public: layout.public.clone(),
        release_id: uuid::Uuid::now_v7().to_string(),
    };
    validate_public(&owned_entries(layout, &manifest.info()))?;
    fs::write(
        stage.path().join("manifest.json"),
        serde_json::to_vec(&manifest)?,
    )?;
    fs::set_permissions(
        stage.path().join("manifest.json"),
        fs::Permissions::from_mode(0o444),
    )?;
    let _lock = HeldLock::acquire(home, &layout.product_lock, uid)?;
    crate::installation::clean_scratch_at(
        &layout.product,
        uid,
        &[root.join("releases")].into_iter().collect(),
        None,
    )?;
    let destination = root.join("releases").join(&manifest.release_id);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            safe_directory(&destination, uid)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(stage.path(), &destination)?;
        }
        Err(e) => return Err(e.into()),
    }

    Ok(PreparedRelease {
        root: destination,
        info: manifest.info(),
    })
}

fn capture(path: &Path) -> Result<CapturedEntry> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.uid() != current_uid()? => {
            Err(Error::new("foreign public installation entry"))
        }
        Ok(meta) if meta.file_type().is_symlink() => Ok(CapturedEntry::Link(fs::read_link(path)?)),
        Ok(meta) if meta.is_file() && meta.mode() & 0o022 == 0 => Ok(CapturedEntry::Copy),
        Ok(_) => Err(Error::new("public path has an unsupported type")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CapturedEntry::Absent),
        Err(e) => Err(e.into()),
    }
}

fn selected(
    layout: &InstallLayout,
    home: &Path,
    name: &str,
    legacy: &LegacyReader<'_>,
) -> Result<Option<ReleaseInfo>> {
    match capture(&install_root(layout, home).join(name))? {
        CapturedEntry::Absent => Ok(None),
        CapturedEntry::Link(target) => {
            let text = target
                .to_str()
                .ok_or_else(|| Error::new("invalid selection"))?;
            let id = text
                .strip_prefix("releases/")
                .filter(|id| valid_release_id(id))
                .ok_or_else(|| Error::new("invalid selected release"))?;
            Ok(Some(read_release_at(
                layout,
                &install_root(layout, home).join("releases").join(id),
                legacy,
            )?))
        }
        CapturedEntry::Copy => Err(Error::new("release selector is not a symbolic link")),
    }
}

fn expected_entry(
    layout: &InstallLayout,
    home: &Path,
    entry: &PublicEntry,
    runtime: bool,
) -> CapturedEntry {
    match entry.kind {
        PublicKind::Symlink => CapturedEntry::Link(
            install_root(layout, home)
                .join(if runtime { "runtime" } else { "current" })
                .join(&entry.artifact),
        ),
        PublicKind::Copy => CapturedEntry::Copy,
    }
}

fn expected_view(
    layout: &InstallLayout,
    home: &Path,
    release: Option<&ReleaseInfo>,
    runtime: bool,
) -> BTreeMap<PathBuf, CapturedEntry> {
    let Some(release) = release else {
        return BTreeMap::new();
    };
    let entries = if runtime {
        owned_entries(layout, release)
    } else {
        release.public.clone()
    };
    entries
        .into_iter()
        .map(|entry| {
            let value = expected_entry(
                layout,
                home,
                &entry,
                runtime && release.files.contains_key(&entry.artifact),
            );
            (entry.path, value)
        })
        .collect()
}

fn matches_view(
    actual: &BTreeMap<PathBuf, CapturedEntry>,
    expected: &BTreeMap<PathBuf, CapturedEntry>,
) -> bool {
    actual
        .iter()
        .filter(|(_, value)| **value != CapturedEntry::Absent)
        .eq(expected
            .iter()
            .filter(|(_, value)| **value != CapturedEntry::Absent))
}

fn publication_view(
    layout: &InstallLayout,
    home: &Path,
    expected: &InstallSnapshot,
    info: &ReleaseInfo,
) -> InstallSnapshot {
    let mut after = expected.clone();
    if selection(after.current.as_ref()) != Some(info.release_id.as_str()) {
        after.previous.clone_from(&after.current);
    }
    after.current = Some(info.clone());
    for value in after.entries.values_mut() {
        *value = CapturedEntry::Absent;
    }
    for entry in owned_entries(layout, info) {
        after.entries.insert(
            entry.path.clone(),
            expected_entry(
                layout,
                home,
                &entry,
                info.files.contains_key(&entry.artifact),
            ),
        );
    }
    after
}

fn restoration_view(
    layout: &InstallLayout,
    home: &Path,
    prior: &InstallSnapshot,
    changed: &InstallSnapshot,
) -> InstallSnapshot {
    let mut restored = prior.clone();
    for path in changed.entries.keys() {
        restored
            .entries
            .entry(path.clone())
            .or_insert(CapturedEntry::Absent);
    }
    for value in restored.entries.values_mut() {
        *value = CapturedEntry::Absent;
    }
    restored
        .entries
        .extend(expected_view(layout, home, prior.current.as_ref(), true));
    restored
}

fn prove_recovery_view(
    actual: &InstallSnapshot,
    prior: &InstallSnapshot,
    target: &InstallSnapshot,
) -> Result<()> {
    if actual.current.is_some()
        && selection(actual.current.as_ref()) != selection(prior.current.as_ref())
        && selection(actual.current.as_ref()) != selection(target.current.as_ref())
        || actual.previous.is_some()
            && selection(actual.previous.as_ref()) != selection(prior.previous.as_ref())
            && selection(actual.previous.as_ref()) != selection(target.previous.as_ref())
    {
        return Err(Error::new("unattributable recovery release selection"));
    }
    for (path, value) in &actual.entries {
        if *value != CapturedEntry::Absent
            && Some(value) != prior.entries.get(path)
            && Some(value) != target.entries.get(path)
        {
            return Err(Error::new("unattributable recovery public path"));
        }
    }
    Ok(())
}

fn snapshot(
    layout: &InstallLayout,
    home: &Path,
    legacy: &LegacyReader<'_>,
    strict: bool,
) -> Result<InstallSnapshot> {
    let uid = validate_layout(layout, home)?;
    let root = install_root(layout, home);
    ensure_path(
        home,
        root.strip_prefix(home)
            .map_err(|_| Error::new("invalid installation path"))?,
        uid,
        false,
    )?;
    let current = selected(layout, home, "current", legacy)?;
    let previous = selected(layout, home, "previous", legacy)?;
    let mut public = BTreeSet::new();
    for entry in layout
        .public
        .iter()
        .chain(current.iter().flat_map(|r| &r.public))
        .chain(previous.iter().flat_map(|r| &r.public))
    {
        public.insert(entry.path.clone());
    }
    for release in current.iter().chain(previous.iter()) {
        for entry in runtime_entries(layout, release) {
            public.insert(entry.path);
        }
    }
    let mut entries = BTreeMap::new();
    for path in &public {
        if let Some(parent) = path.parent() {
            ensure_path(home, parent, uid, false)?;
        }
        let actual = capture(&home.join(path))?;
        entries.insert(path.clone(), actual);
    }
    let stable = expected_view(layout, home, current.as_ref(), true);
    let retained = expected_view(layout, home, current.as_ref(), false);
    let legacy_view = current
        .as_ref()
        .is_some_and(|r| r.format != TRANSACTION_FORMAT)
        && matches_view(&entries, &retained);
    if strict && !matches_view(&entries, &stable) && !legacy_view {
        return Err(Error::new(
            "foreign or incoherent public installation entry",
        ));
    }
    if strict && current.is_none() && previous.is_some() {
        return Err(Error::new("previous selector exists without current"));
    }
    Ok(InstallSnapshot {
        current,
        previous,
        entries,
    })
}

/// Inspect complete owned installation state without changing or executing it.
/// # Errors
/// Rejects foreign/incoherent public paths, unsafe directories, or invalid releases.
pub fn inspect_installation(
    layout: &InstallLayout,
    home: &Path,
    legacy: &LegacyReader<'_>,
) -> Result<InstallSnapshot> {
    snapshot(layout, home, legacy, true)
}

/// Inspect selected releases while allowing their owned public entries to be absent.
/// The product decides whether its runtime state permits repair or reinstallation.
/// # Errors
/// Rejects unsafe releases, foreign public paths, or entries other than absent/exact selected bytes.
pub fn inspect_detached_installation(
    layout: &InstallLayout,
    home: &Path,
    legacy: &LegacyReader<'_>,
) -> Result<InstallSnapshot> {
    let state = snapshot(layout, home, legacy, false)?;
    for (path, value) in &state.entries {
        if *value == CapturedEntry::Absent {
            continue;
        }
        let current = state
            .current
            .as_ref()
            .ok_or_else(|| Error::new("public entry has no selected release"))?;
        let owned = owned_entries(layout, current);
        let entry = owned
            .iter()
            .find(|entry| &entry.path == path)
            .ok_or_else(|| Error::new("public entry is not owned by selected release"))?;
        if *value
            != expected_entry(
                layout,
                home,
                entry,
                current.files.contains_key(&entry.artifact),
            )
            && (current.format == TRANSACTION_FORMAT
                || *value != expected_entry(layout, home, entry, false))
        {
            return Err(Error::new("foreign public installation entry"));
        }
    }
    Ok(state)
}

pub struct InstallTransaction<'a> {
    layout: InstallLayout,
    home: PathBuf,
    legacy: &'a LegacyReader<'a>,
    uid: u32,
    _product_lock: HeldLock,
}

/// Hold the product's exact existing installation lock across its lifecycle.
/// # Errors
/// Rejects unsafe paths, invalid layout, or an unavailable product lock.
pub fn lock_installation<'a>(
    layout: &InstallLayout,
    home: &Path,
    legacy: &'a LegacyReader<'a>,
) -> Result<InstallTransaction<'a>> {
    let uid = validate_layout(layout, home)?;
    let lock = HeldLock::acquire(home, &layout.product_lock, uid)?;
    Ok(InstallTransaction {
        layout: layout.clone(),
        home: home.to_owned(),
        legacy,
        uid,
        _product_lock: lock,
    })
}

impl InstallTransaction<'_> {
    /// Check captured state again while holding the product lock.
    /// # Errors
    /// Returns an error for changed selectors or captured public paths.
    pub fn recheck(&self, expected: &InstallSnapshot) -> Result<()> {
        if snapshot(&self.layout, &self.home, self.legacy, false)? != *expected {
            return Err(Error::new("stale installation snapshot"));
        }
        Ok(())
    }

    fn catalog(&self) -> Result<Option<HeldLock>> {
        self.layout
            .catalog_lock
            .as_ref()
            .map(|spec| HeldLock::acquire(&self.home, spec, self.uid))
            .transpose()
    }

    /// Remove owned public views before product-owned migration; keep release selectors.
    /// # Errors
    /// Refuses stale snapshots or failed detachment; partial changes remain visible.
    pub fn suspend(
        &mut self,
        expected: &InstallSnapshot,
        paths: &[PathBuf],
    ) -> Result<SelectionReceipt> {
        let _catalog = self.catalog()?;
        self.recheck(expected)?;
        let mut after = expected.clone();
        for path in paths {
            *after
                .entries
                .get_mut(path)
                .ok_or_else(|| Error::new("suspension path is not a captured public entry"))? =
                CapturedEntry::Absent;
        }
        self.change(expected, &after, true, || Ok(()))
    }

    /// Detach all owned public and release selectors; retain releases and runtime state.
    /// # Errors
    /// Refuses stale snapshots or failed detachment; partial changes remain visible.
    pub fn detach(&mut self, expected: &InstallSnapshot) -> Result<SelectionReceipt> {
        let _catalog = self.catalog()?;
        self.recheck(expected)?;
        let mut after = expected.clone();
        after.current = None;
        after.previous = None;
        for value in after.entries.values_mut() {
            *value = CapturedEntry::Absent;
        }
        self.change(expected, &after, true, || Ok(()))
    }

    /// Publish while holding the catalog lock, then run the caller lifecycle step.
    /// # Errors
    /// Failed publication or caller work retains completed selector changes.
    pub fn publish(
        &mut self,
        prepared: &PreparedRelease,
        expected: &InstallSnapshot,
        verify: impl FnOnce(&ReleaseInfo) -> Result<()>,
    ) -> Result<SelectionReceipt> {
        if prepared.root
            != install_root(&self.layout, &self.home)
                .join("releases")
                .join(&prepared.info.release_id)
        {
            return Err(Error::new(
                "prepared release belongs to another installation",
            ));
        }
        let _catalog = self.catalog()?;
        self.recheck(expected)?;
        let after = publication_view(&self.layout, &self.home, expected, &prepared.info);
        self.change(expected, &after, false, || verify(&prepared.info))
    }

    /// Repair an interrupted publication using captured prior and exact candidate evidence.
    /// The product explicitly chooses the compatible program selection after its state recovery.
    /// # Errors
    /// Rejects unproved releases or paths outside the prior/candidate/absent views.
    pub fn recover(
        &mut self,
        prior: &InstallSnapshot,
        candidate: &PreparedRelease,
        select_candidate: bool,
        verify: impl FnOnce(&ReleaseInfo) -> Result<()>,
    ) -> Result<SelectionReceipt> {
        if candidate.root
            != install_root(&self.layout, &self.home)
                .join("releases")
                .join(&candidate.info.release_id)
        {
            return Err(Error::new(
                "recovery candidate belongs to another installation",
            ));
        }
        let target = publication_view(&self.layout, &self.home, prior, &candidate.info);
        let _catalog = self.catalog()?;
        let mut actual = snapshot(&self.layout, &self.home, self.legacy, false)?;
        for path in prior.entries.keys().chain(target.entries.keys()) {
            if !actual.entries.contains_key(path) {
                if let Some(parent) = path.parent() {
                    ensure_path(&self.home, parent, self.uid, false)?;
                }
                actual
                    .entries
                    .insert(path.clone(), capture(&self.home.join(path))?);
            }
        }
        prove_recovery_view(&actual, prior, &target)?;
        let restored = restoration_view(&self.layout, &self.home, prior, &target);
        let selected = if select_candidate { &target } else { &restored };
        self.change(&actual, selected, !select_candidate, || {
            if let Some(info) = &selected.current {
                verify(info)?;
            }
            Ok(())
        })
    }

    /// Restore the captured prior file selection when explicitly requested.
    /// # Errors
    /// Refuses changed selections and reports restoration uncertainty.
    pub fn restore(
        &mut self,
        receipt: &SelectionReceipt,
        verify: impl FnOnce(&ReleaseInfo) -> Result<()>,
    ) -> Result<()> {
        let _catalog = self.catalog()?;
        self.recheck(&receipt.after)?;
        let restored = restoration_view(&self.layout, &self.home, &receipt.before, &receipt.after);
        self.change(&receipt.after, &restored, true, || {
            if let Some(prior) = &receipt.before.current {
                verify(prior)?;
            }
            Ok(())
        })?;
        Ok(())
    }

    fn change(
        &self,
        before: &InstallSnapshot,
        after: &InstallSnapshot,
        recorded_recovery: bool,
        verify: impl FnOnce() -> Result<()>,
    ) -> Result<SelectionReceipt> {
        for path in after
            .entries
            .keys()
            .filter(|path| !before.entries.contains_key(*path))
        {
            if let Some(parent) = path.parent() {
                ensure_path(&self.home, parent, self.uid, false)?;
            }
            if capture(&self.home.join(path))? != CapturedEntry::Absent {
                return Err(Error::new("new installation path is already occupied"));
            }
        }
        self.clean_scratch(before, after)?;
        let result = self
            .write_view(after, recorded_recovery)
            .and_then(|()| self.recheck(after))
            .and_then(|()| verify());
        if let Err(mut error) = result {
            error.disposition = Disposition::Uncertain;
            return Err(error);
        }
        Ok(SelectionReceipt {
            before: before.clone(),
            after: after.clone(),
        })
    }

    fn clean_scratch(&self, before: &InstallSnapshot, after: &InstallSnapshot) -> Result<()> {
        let root = install_root(&self.layout, &self.home);
        let mut parents = BTreeSet::from([root.clone()]);
        let mut targets = BTreeSet::new();
        for snapshot in [before, after] {
            for (path, value) in &snapshot.entries {
                if let Some(parent) = path.parent() {
                    parents.insert(self.home.join(parent));
                }
                if let CapturedEntry::Link(target) = value {
                    targets.insert(target.clone());
                }
            }
        }
        crate::installation::clean_scratch_at(
            &self.layout.product,
            self.uid,
            &parents,
            Some(&targets),
        )?;
        crate::installation::clean_scratch_at(
            &self.layout.product,
            self.uid,
            &BTreeSet::from([root.join("releases")]),
            None,
        )
    }

    fn write_view(&self, target: &InstallSnapshot, _recorded_recovery: bool) -> Result<()> {
        let root = install_root(&self.layout, &self.home);
        ensure_path(
            &self.home,
            root.strip_prefix(&self.home)
                .map_err(|_| Error::new("invalid installation root"))?,
            self.uid,
            true,
        )?;
        for (path, value) in &target.entries {
            if let Some(parent) = path.parent() {
                ensure_path(&self.home, parent, self.uid, true)?;
            }
            let destination = self.home.join(path);
            match value {
                CapturedEntry::Absent => remove_owned(&destination)?,
                CapturedEntry::Link(link) => atomic_link(link, &destination, &self.layout.product)?,
                CapturedEntry::Copy => {
                    let selected = target
                        .current
                        .as_ref()
                        .ok_or_else(|| Error::new("copied public path has no selected release"))?;
                    let owned = owned_entries(&self.layout, selected);
                    let entry = owned
                        .iter()
                        .find(|e| &e.path == path && e.kind == PublicKind::Copy)
                        .ok_or_else(|| Error::new("missing copied path declaration"))?;
                    let source = root
                        .join("releases")
                        .join(&selected.release_id)
                        .join(&entry.artifact);
                    let temporary = tempfile::Builder::new()
                        .prefix(&format!(
                            ".cell-install-selector-{}-{}-",
                            self.layout.product,
                            std::process::id()
                        ))
                        .tempdir_in(
                            destination
                                .parent()
                                .ok_or_else(|| Error::new("missing public parent"))?,
                        )?;
                    let file = temporary.path().join("copy");
                    copy_file(&source, &file, entry.mode)?;
                    fs::rename(&file, &destination)?;
                }
            }
        }
        for (name, value) in [("previous", &target.previous), ("current", &target.current)] {
            if let Some(info) = value {
                atomic_link(
                    &PathBuf::from(format!("releases/{}", info.release_id)),
                    &root.join(name),
                    &self.layout.product,
                )?;
            } else {
                remove_owned(&root.join(name))?;
            }
        }
        Ok(())
    }
}

fn remove_owned(path: &Path) -> Result<()> {
    if capture(path)? != CapturedEntry::Absent {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn atomic_link(target: &Path, destination: &Path, product: &str) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| Error::new("invalid selector parent"))?;
    let temporary = tempfile::Builder::new()
        .prefix(&format!(
            ".cell-install-selector-{product}-{}-",
            std::process::id()
        ))
        .tempdir_in(parent)?;
    let link = temporary.path().join("link");
    symlink(target, &link)?;
    fs::rename(link, destination)?;
    Ok(())
}

fn verify_manifest(root: &Path, product: Option<&str>) -> Result<Manifest> {
    if !root.is_absolute() || fs::canonicalize(root)? != root {
        return Err(Error::new("release path must be canonical"));
    }
    verify_tree_ownership(root, current_uid()?)?;
    let bytes = fs::read(root.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    if !is_transaction_format(&manifest.format)
        || product.is_some_and(|p| p != manifest.product)
        || !valid_release_id(&manifest.release_id)
        || root.file_name().and_then(|p| p.to_str()) != Some(manifest.release_id.as_str())
    {
        return Err(Error::new(
            "invalid release format, product, or recorded identity",
        ));
    }
    validate_public(&manifest.public)?;
    let (mut files, dirs) = inventory(root)?;
    files.remove("manifest.json");
    if !inventory_matches(&files, &manifest.files) {
        return Err(Error::new("release files differ from the sealed inventory"));
    }
    let mut expected_dirs = BTreeSet::new();
    for path in manifest.files.keys() {
        relative(Path::new(path))?;
        for parent in Path::new(path)
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            expected_dirs.insert(parent.to_string_lossy().into_owned());
        }
    }
    if dirs != expected_dirs {
        return Err(Error::new("release has unmanifested directories"));
    }
    for entry in &manifest.public {
        if !manifest.files.contains_key(&entry.artifact) && !dirs.contains(&entry.artifact) {
            return Err(Error::new("public entry names a missing artifact"));
        }
        if entry.kind == PublicKind::Copy
            && manifest
                .files
                .get(&entry.artifact)
                .is_none_or(|file| file.mode != entry.mode)
        {
            return Err(Error::new(
                "copied public entry has an invalid artifact or mode",
            ));
        }
    }
    for (id, provider) in &manifest.providers {
        relative(Path::new(&provider.path))?;
        let data: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(&provider.path).join("provider.json"))?)?;
        if data
            .pointer("/provider/id")
            .and_then(serde_json::Value::as_str)
            != Some(id.as_str())
            || data
                .pointer("/provider/release")
                .and_then(serde_json::Value::as_str)
                != Some(provider.version.as_str())
        {
            return Err(Error::new(
                "provider identity or independent version mismatch",
            ));
        }
    }
    Ok(manifest)
}

/// Read and prove a retained release; legacy proof belongs to its product.
/// # Errors
/// Rejects unsafe paths, incompatible manifests, changed bytes, or rejected legacy proof.
pub fn verify_release_at(
    layout: &InstallLayout,
    root: &Path,
    legacy: &LegacyVerifier<'_>,
) -> Result<ReleaseInfo> {
    if !root.is_absolute() || fs::canonicalize(root)? != root {
        return Err(Error::new("release path must be canonical"));
    }
    verify_tree_ownership(root, current_uid()?)?;
    let result = if root.join("manifest.json").try_exists()? {
        let format: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
        if format
            .get("format")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_transaction_format)
        {
            let manifest = verify_manifest(root, Some(&layout.product))?;
            if manifest.public != layout.public {
                return Err(Error::new(
                    "release public declarations differ from product layout",
                ));
            }
            manifest.info()
        } else {
            legacy(root)?
        }
    } else {
        legacy(root)?
    };
    if root.file_name().and_then(|name| name.to_str()) != Some(result.release_id.as_str())
        || !valid_release_id(&result.release_id)
    {
        return Err(Error::new("legacy proof returned an invalid identity"));
    }
    validate_public(&result.public)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (InstallLayout, ReleaseInfo) {
        let layout = InstallLayout {
            product: "fixture".to_owned(),
            application: "Fixture".to_owned(),
            product_lock: LockSpec {
                path: PathBuf::from(".fixture/lock"),
                kind: LockKind::Directory,
            },
            catalog_lock: None,
            public: vec![
                PublicEntry {
                    path: PathBuf::from(".local/bin/fixture"),
                    artifact: "bin/fixture".to_owned(),
                    kind: PublicKind::Symlink,
                    mode: 0o555,
                },
                PublicEntry {
                    path: PathBuf::from("providers/fixture"),
                    artifact: "share/chancery".to_owned(),
                    kind: PublicKind::Symlink,
                    mode: 0o555,
                },
            ],
        };
        let release = ReleaseInfo {
            release_id: "first-archive".to_owned(),
            format: TRANSACTION_FORMAT.to_owned(),
            versions: BTreeMap::new(),
            public: layout.public.clone(),
            files: BTreeMap::from([
                (
                    "bin/fixture".to_owned(),
                    FileEntry {
                        sha256: String::new(),
                        mode: 0o555,
                        code_identifier: None,
                    },
                ),
                (
                    "share/chancery/provider.json".to_owned(),
                    FileEntry {
                        sha256: String::new(),
                        mode: 0o444,
                        code_identifier: None,
                    },
                ),
            ]),
        };
        (layout, release)
    }

    #[test]
    fn publication_keeps_actual_paths_when_archive_identity_changes() {
        let (layout, first) = fixture();
        let home = Path::new("/operator");
        let empty = InstallSnapshot {
            current: None,
            previous: None,
            entries: BTreeMap::new(),
        };
        let before = publication_view(&layout, home, &empty, &first);
        let mut second = first.clone();
        second.release_id = "second-archive".to_owned();
        let after = publication_view(&layout, home, &before, &second);
        assert_eq!(before.entries, after.entries);
        assert_ne!(before.current, after.current);
        assert_eq!(after.previous, Some(first));
        assert_eq!(
            after.entries[Path::new(".local/bin/fixture")],
            CapturedEntry::Link(runtime_root(&layout, home).join("bin/fixture"))
        );
        assert_eq!(
            after.entries
                [Path::new("Library/Application Support/Fixture/install/runtime/bin/fixture")],
            CapturedEntry::Copy
        );
        assert_eq!(
            after.entries[Path::new("providers/fixture")],
            CapturedEntry::Link(install_root(&layout, home).join("current/share/chancery"))
        );
    }

    #[test]
    fn restoring_a_legacy_archive_uses_runtime_and_cleans_new_files() {
        let (layout, mut first) = fixture();
        first.format = "cell-install-v3".to_owned();
        let home = Path::new("/operator");
        let prior = InstallSnapshot {
            current: Some(first.clone()),
            previous: None,
            entries: expected_view(&layout, home, Some(&first), false),
        };
        let mut second = first.clone();
        second.release_id = "second-archive".to_owned();
        second.format = TRANSACTION_FORMAT.to_owned();
        second.files.insert(
            "libexec/companion".to_owned(),
            FileEntry {
                sha256: String::new(),
                mode: 0o555,
                code_identifier: None,
            },
        );
        let changed = publication_view(&layout, home, &prior, &second);
        let restored = restoration_view(&layout, home, &prior, &changed);
        assert_eq!(restored.current, prior.current);
        assert_eq!(restored.previous, prior.previous);
        assert_eq!(
            restored.entries[Path::new(".local/bin/fixture")],
            CapturedEntry::Link(runtime_root(&layout, home).join("bin/fixture"))
        );
        assert_eq!(
            restored.entries[Path::new(
                "Library/Application Support/Fixture/install/runtime/libexec/companion"
            )],
            CapturedEntry::Absent
        );
        assert!(matches_view(
            &restored.entries,
            &expected_view(&layout, home, Some(&first), true)
        ));
    }

    #[test]
    fn snapshot_comparison_ignores_absent_cleanup_paths_but_checks_live_paths() {
        let (layout, release) = fixture();
        let empty = InstallSnapshot {
            current: None,
            previous: None,
            entries: BTreeMap::new(),
        };
        let mut snapshot = publication_view(&layout, Path::new("/operator"), &empty, &release);
        let expected = snapshot.clone();
        snapshot
            .entries
            .insert(PathBuf::from("retired/path"), CapturedEntry::Absent);
        assert_eq!(snapshot, expected);
        snapshot
            .entries
            .insert(PathBuf::from("retired/path"), CapturedEntry::Copy);
        assert_ne!(snapshot, expected);
        assert!(!matches_view(&BTreeMap::new(), &expected.entries));
    }

    #[test]
    fn partial_upgrade_attribution_includes_candidate_only_runtime_files() -> Result<()> {
        let (layout, mut first) = fixture();
        first.format = "cell-install-v3".to_owned();
        let home = Path::new("/operator");
        let prior = InstallSnapshot {
            current: Some(first.clone()),
            previous: None,
            entries: expected_view(&layout, home, Some(&first), false),
        };
        let mut candidate = first.clone();
        candidate.release_id = "second-archive".to_owned();
        candidate.files.insert(
            "libexec/companion".to_owned(),
            FileEntry {
                sha256: String::new(),
                mode: 0o555,
                code_identifier: None,
            },
        );
        let target = publication_view(&layout, home, &prior, &candidate);
        let companion =
            PathBuf::from("Library/Application Support/Fixture/install/runtime/libexec/companion");
        let mut actual = prior.clone();
        actual
            .entries
            .insert(companion.clone(), CapturedEntry::Copy);
        prove_recovery_view(&actual, &prior, &target)?;
        let restored = restoration_view(&layout, home, &prior, &target);
        assert_eq!(restored.entries[&companion], CapturedEntry::Absent);
        actual
            .entries
            .insert(companion, CapturedEntry::Link(PathBuf::from("/foreign")));
        assert!(prove_recovery_view(&actual, &prior, &target).is_err());
        Ok(())
    }

    #[test]
    fn retained_release_keeps_inventory_checks_without_byte_hashes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = fs::canonicalize(temporary.path())?.join("a".repeat(64));
        fs::create_dir(&root)?;
        fs::create_dir(root.join("bin"))?;
        fs::write(root.join("bin/fixture"), "retained content")?;
        fs::set_permissions(root.join("bin/fixture"), fs::Permissions::from_mode(0o555))?;
        let manifest = Manifest {
            format: "cell-install-v2".to_owned(),
            product: "fixture".to_owned(),
            versions: BTreeMap::new(),
            providers: BTreeMap::new(),
            files: BTreeMap::from([(
                "bin/fixture".to_owned(),
                FileEntry {
                    sha256: "b".repeat(64),
                    mode: 0o555,
                    code_identifier: None,
                },
            )]),
            public: Vec::new(),
            release_id: "a".repeat(64),
        };
        fs::write(root.join("manifest.json"), serde_json::to_vec(&manifest)?)?;
        fs::set_permissions(
            root.join("manifest.json"),
            fs::Permissions::from_mode(0o444),
        )?;
        verify_manifest(&root, Some("fixture"))?;
        fs::set_permissions(root.join("bin/fixture"), fs::Permissions::from_mode(0o755))?;
        fs::write(root.join("bin/fixture"), "different retained content")?;
        fs::set_permissions(root.join("bin/fixture"), fs::Permissions::from_mode(0o555))?;
        verify_manifest(&root, Some("fixture"))?;
        fs::write(root.join("extra"), "unmanifested")?;
        assert!(verify_manifest(&root, Some("fixture")).is_err());
        fs::remove_file(root.join("extra"))?;
        fs::set_permissions(root.join("bin/fixture"), fs::Permissions::from_mode(0o444))?;
        assert!(verify_manifest(&root, Some("fixture")).is_err());
        Ok(())
    }
}
