use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::Serialize;

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub(crate) root: PathBuf,
    pub(crate) workspace: PathBuf,
    local: bool,
}

pub(crate) fn default_root() -> Result<PathBuf> {
    Ok(crate::workspace::root()?.join("telete"))
}

impl Paths {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let workspace = crate::workspace::root()?;
        ensure!(
            root.is_absolute() && root.starts_with(&workspace) && root != workspace,
            "Telete state must be inside the selected external workspace"
        );
        ensure!(normalized_absolute(&root), "state path is not normalized");
        let relative = root.strip_prefix(&workspace)?;
        let first = relative
            .components()
            .next()
            .context("empty state path")?
            .as_os_str();
        ensure!(
            first
                .to_str()
                .is_some_and(|v| v == "telete" || v.starts_with("telete-")),
            "state must use a separate telete directory"
        );
        let paths = Self {
            root,
            workspace,
            local: false,
        };
        paths.initialize()?;
        Ok(paths)
    }

    /// Select an explicit state path without using an incomplete journal to
    /// infer a new layout. Default and native bootstrap paths use `new` directly.
    pub(crate) fn selected(root: PathBuf, initialize_project: bool) -> Result<Self> {
        if Self::select_local(&root, initialize_project)? {
            Self::local(root)
        } else {
            Self::new(root)
        }
    }

    fn select_local(root: &Path, initialize_project: bool) -> Result<bool> {
        if let Some(config) = crate::store::probe_config(root)? {
            config.local_workspace(root)
        } else {
            ensure!(
                initialize_project,
                "Telete is not initialized; run telete init"
            );
            ensure!(
                !private_directory_exists(root)? || fs::read_dir(root)?.next().is_none(),
                "refusing to initialize nonempty state without a Telete journal"
            );
            Ok(true)
        }
    }

    fn local(root: PathBuf) -> Result<Self> {
        let paths = Self {
            workspace: root.clone(),
            root,
            local: true,
        };
        paths.initialize()?;
        // Generic queues do not share Cell's Cargo download directory.
        ensure_private(&paths.workspace.join("cargo"))?;
        ensure_private(&paths.root.join("cache"))?;
        Ok(paths)
    }

    pub(crate) fn local_workspace(&self) -> Option<PathBuf> {
        self.local.then(|| self.root.clone())
    }

    pub(crate) fn verify_config(&self, config: &crate::store::Config) -> Result<()> {
        ensure!(
            config.local_workspace(&self.root)? == self.local,
            "queue workspace selection changed"
        );
        Ok(())
    }

    fn initialize(&self) -> Result<()> {
        ensure_private(&self.root)?;
        for name in ["jobs", "tmp", "targets", "broker", "deployments", "logs"] {
            ensure_private(&self.root.join(name))?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: PathBuf) -> Result<Self> {
        let paths = Self {
            workspace: root.clone(),
            root,
            local: false,
        };
        paths.initialize()?;
        Ok(paths)
    }

    pub(crate) fn job(&self, id: &str) -> PathBuf {
        self.root.join("jobs").join(id)
    }
    pub(crate) fn targets(&self) -> PathBuf {
        self.root.join("targets")
    }
    pub(crate) fn require_capacity(&self) -> Result<()> {
        #[cfg(not(test))]
        if !self.local {
            ensure!(
                crate::workspace::root()? == self.workspace,
                "external workspace selection changed"
            );
        }
        ensure!(
            fs2::available_space(&self.workspace)? >= 2 * 1024 * 1024 * 1024,
            "selected workspace has less than 2 GiB free"
        );
        Ok(())
    }
    pub(crate) fn environment(&self) -> BTreeMap<String, String> {
        let temporary = self.root.join("tmp").display().to_string();
        let cache = self.root.join("cache").display().to_string();
        let home = std::env::var("HOME").unwrap_or_default();
        BTreeMap::from([
            (
                "PATH".into(),
                format!("{home}/.cargo/bin:{home}/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"),
            ),
            ("RUSTUP_HOME".into(), format!("{home}/.rustup")),
            ("TMPDIR".into(), temporary.clone()),
            ("TMP".into(), temporary.clone()),
            ("TEMP".into(), temporary),
            (
                "CARGO_HOME".into(),
                self.workspace.join("cargo").display().to_string(),
            ),
            (
                "CARGO_TARGET_DIR".into(),
                self.targets().display().to_string(),
            ),
            ("XDG_CACHE_HOME".into(), cache.clone()),
            ("CLANG_MODULE_CACHE_PATH".into(), format!("{cache}/clang")),
            ("SWIFT_MODULECACHE_PATH".into(), format!("{cache}/swift")),
            ("CHANCERY_USAGE_INTERNAL".into(), "1".into()),
        ])
    }
}

pub(crate) fn normalized_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .as_os_str()
            .as_encoded_bytes()
            .split(|byte| *byte == b'/')
            .any(|part| part == b"." || part == b"..")
}

/// Inspect a private directory without creating it or its ancestors.
pub(crate) fn private_directory_exists(path: &Path) -> Result<bool> {
    ensure!(
        normalized_absolute(path),
        "private directory must be absolute and normalized"
    );
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "symbolic private path: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir()
            && metadata.uid() == crate::host_setup::uid()
            && metadata.mode() & 0o777 == 0o700,
        "private directory must be user-owned mode 0700: {}",
        path.display()
    );
    Ok(true)
}

pub(crate) fn ensure_private(path: &Path) -> Result<()> {
    if !private_directory_exists(path)? {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        ensure!(
            private_directory_exists(path)?,
            "private directory is missing"
        );
    }
    Ok(())
}

pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("record has no parent")?;
    ensure_private(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "record is not a regular file"
        );
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) struct FileLock {
    _file: File,
}

pub(crate) fn lock(path: &Path, blocking: bool) -> Result<FileLock> {
    ensure_private(path.parent().context("lock has no parent")?)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file()
            && file.metadata()?.uid() == crate::host_setup::uid()
            && file.metadata()?.mode() & 0o777 == 0o600,
        "lock must be a private user-owned regular file"
    );
    if blocking {
        file.lock_exclusive()?;
    } else if let Err(error) = file.try_lock_exclusive() {
        bail!("operation already owned: {}: {error}", path.display());
    }
    // Retain process ownership through child execution. Closing the parent's
    // descriptor does not release an inherited flock while a child holds it.
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())?;
    Ok(FileLock { _file: file })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::process::Command;

    include!("local_path_tests.rs");

    #[test]
    fn child_retains_lock_after_parent_descriptor_closes() {
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("private");
        ensure_private(&private).unwrap();
        let path = private.join("owner.lock");
        let owner = lock(&path, false).unwrap();
        let mut child = Command::new("/bin/sleep").arg("1").spawn().unwrap();
        drop(owner);
        assert!(lock(&path, false).is_err());
        assert!(child.wait().unwrap().success());
        assert!(lock(&path, false).is_ok());
    }
}
