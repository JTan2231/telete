use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::Serialize;

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub(crate) root: PathBuf,
}

impl Paths {
    pub(crate) fn discovery(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn selected(root: PathBuf, create: bool) -> Result<Self> {
        if let Some(config) = crate::store::probe_config(&root)? {
            ensure!(config.state_root == root, "queue state directory changed");
        }
        let paths = Self::discovery(root);
        if create {
            paths.create_directories()?;
        }
        Ok(paths)
    }

    pub(crate) fn verify_config(&self, config: &crate::store::Config) -> Result<()> {
        ensure!(
            config.state_root == self.root,
            "queue state directory changed"
        );
        Ok(())
    }

    fn create_directories(&self) -> Result<()> {
        ensure_private(&self.root)?;
        for name in ["jobs", "tmp", "broker", "deployments"] {
            ensure_private(&self.root.join(name))?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: PathBuf) -> Result<Self> {
        Self::selected(root, true)
    }

    pub(crate) fn job(&self, id: &str) -> PathBuf {
        self.root.join("jobs").join(id)
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
            && metadata.uid() == rustix::process::getuid().as_raw()
            && metadata.mode() & 0o777 == 0o700,
        "private directory must be user-owned mode 0700: {}",
        path.display()
    );
    Ok(true)
}

pub(crate) fn ensure_private(path: &Path) -> Result<()> {
    if !private_directory_exists(path)? {
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(path)?;
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

#[derive(Debug)]
pub(crate) struct FileLock {
    file: File,
}

impl FileLock {
    /// Lifecycle owners must not leak into workflow commands.
    pub(crate) fn close_on_exec(&self) -> Result<()> {
        rustix::io::fcntl_setfd(&self.file, rustix::io::FdFlags::CLOEXEC)?;
        Ok(())
    }
}

pub(crate) fn try_lock(path: &Path) -> Result<Option<FileLock>> {
    match lock(path, false) {
        Ok(owner) => Ok(Some(owner)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
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
            && file.metadata()?.uid() == rustix::process::getuid().as_raw()
            && file.metadata()?.mode() & 0o777 == 0o600,
        "lock must be a private user-owned regular file"
    );
    if blocking {
        file.lock_exclusive()?;
    } else {
        file.try_lock_exclusive()
            .with_context(|| format!("operation already owned: {}", path.display()))?;
    }
    // Retain process ownership through child execution. Closing the parent's
    // descriptor does not release an inherited flock while a child holds it.
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())?;
    Ok(FileLock { file })
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
