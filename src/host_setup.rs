//! Shared current-user host setup authority.
use crate::paths::{self, FileLock};
use anyhow::{Context, Result, ensure};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

pub(crate) fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn account_home(output: &str, expected_uid: u32) -> Result<PathBuf> {
    let mut directories = Vec::new();
    for record in output.split("\n\n") {
        let fields: Vec<_> = record
            .lines()
            .filter_map(|line| line.split_once(": "))
            .collect();
        if fields
            .iter()
            .any(|(key, value)| *key == "uid" && value.parse::<u32>().ok() == Some(expected_uid))
        {
            directories.extend(
                fields
                    .iter()
                    .filter(|(key, _)| *key == "dir")
                    .map(|(_, value)| PathBuf::from(value)),
            );
        }
    }
    ensure!(
        directories.len() == 1 && directories[0].is_absolute(),
        "cannot establish the current user's account home"
    );
    Ok(directories.remove(0))
}

pub(crate) fn home() -> Result<PathBuf> {
    static HOME_PATH: OnceLock<PathBuf> = OnceLock::new();
    if let Some(home) = HOME_PATH.get() {
        return Ok(home.clone());
    }
    ensure!(cfg!(target_os = "macos"), "Cell host setup requires macOS");
    let result = Command::new("/usr/bin/dscacheutil")
        .args(["-q", "user", "-a", "uid", &uid().to_string()])
        .output()
        .context("cannot read the current user's account")?;
    ensure!(
        result.status.success(),
        "cannot read the current user's account"
    );
    let home = account_home(&String::from_utf8(result.stdout)?, uid())?;
    ensure!(
        fs::metadata(&home)?.uid() == uid(),
        "account home is not owned by the current user"
    );
    let _ = HOME_PATH.set(home.clone());
    Ok(home)
}

pub(crate) fn setup_lock() -> Result<FileLock> {
    paths::lock(
        &home()?.join("Library/Application Support/Cell/host-setup.lock"),
        false,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn home_uses_exact_account_identity() {
        assert_eq!(
            account_home("uid: 501\ndir: /Users/fixture\n", 501).unwrap(),
            PathBuf::from("/Users/fixture")
        );
        assert!(account_home("uid: 502\ndir: /Users/fixture\n", 501).is_err());
        assert!(account_home("uid: 501\ndir: relative\n", 501).is_err());
        assert!(account_home("uid: 501\ndir: /one\ndir: /two\n", 501).is_err());
    }
}
