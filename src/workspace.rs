use crate::{host_setup, paths};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Workspace {
    schema_version: u32,
    volume: PathBuf,
    volume_uuid: String,
    directory: String,
}

fn config_path() -> Result<PathBuf> {
    Ok(host_setup::home()?.join("Library/Application Support/Cell/workspace.json"))
}

fn nonsymbolic_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
        "workspace path must be absolute and normalized"
    );
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "symbolic workspace path: {}",
                current.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn read_selection(path: &Path) -> Result<Option<Workspace>> {
    nonsymbolic_path(path)?;
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == host_setup::uid()
            && metadata.mode() & 0o777 == 0o600,
        "workspace configuration must be a user-owned mode 0600 regular file"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let value: Workspace = serde_json::from_slice(&bytes)?;
    ensure!(
        value.schema_version == 1 && value.directory == "cell" && !value.volume_uuid.is_empty(),
        "unsupported workspace selection"
    );
    nonsymbolic_path(&value.volume)?;
    Ok(Some(value))
}

fn observed_uuid(volume: &Path, info: &Value) -> Result<String> {
    let mount_point = volume.to_str().context("work volume path must be UTF-8")?;
    ensure!(
        info["MountPoint"].as_str() == Some(mount_point)
            && info["Internal"] == false
            && info["FilesystemType"] == "apfs"
            && info["WritableVolume"] == true
            && info["GlobalPermissionsEnabled"] == true,
        "work storage requires a mounted writable external APFS volume with ownership enabled"
    );
    let uuid = info["VolumeUUID"]
        .as_str()
        .filter(|uuid| !uuid.is_empty())
        .context("external work volume has no UUID")?;
    Ok(uuid.into())
}

fn probe(volume: &Path) -> Result<String> {
    nonsymbolic_path(volume)?;
    ensure!(volume.is_dir(), "external work volume is not mounted");
    let plist = Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist"])
        .arg(volume)
        .output()
        .context("external APFS workspace requires macOS diskutil")?;
    ensure!(
        plist.status.success(),
        "cannot inspect selected external volume"
    );
    let mut converter = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    converter
        .stdin
        .take()
        .context("plutil stdin missing")?
        .write_all(&plist.stdout)?;
    let json = converter.wait_with_output()?;
    ensure!(json.status.success(), "invalid volume observation");
    observed_uuid(volume, &serde_json::from_slice(&json.stdout)?)
}

fn selected_root(selection: &Workspace, observed_uuid: &str) -> Result<PathBuf> {
    ensure!(
        selection.volume_uuid == observed_uuid,
        "the mounted work volume has the wrong identity"
    );
    let base = selection.volume.join("cell");
    nonsymbolic_path(&base)?;
    let metadata = fs::metadata(&base)
        .context("external workspace is missing; restore it before continuing")?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == host_setup::uid()
            && metadata.mode() & 0o777 == 0o700
            && metadata.dev() == fs::metadata(&selection.volume)?.dev(),
        "external workspace must be a private user-owned directory on the selected volume"
    );
    Ok(base)
}

fn require_selection(path: &Path) -> Result<Workspace> {
    read_selection(path)?.context(
        "external work storage is not configured; run telete storage configure --volume PATH",
    )
}

pub(crate) fn root() -> Result<PathBuf> {
    let selection = require_selection(&config_path()?)?;
    selected_root(&selection, &probe(&selection.volume)?)
}

pub(crate) fn status() -> Result<Value> {
    let path = config_path()?;
    let selection = require_selection(&path)?;
    let root = selected_root(&selection, &probe(&selection.volume)?)?;
    Ok(json!({"configuration":path,"selection":selection,"root":root}))
}

fn publish(path: &Path, selection: &Workspace) -> Result<()> {
    let parent = path
        .parent()
        .context("workspace configuration has no parent")?;
    paths::ensure_private(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut file, selection)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn configure_at(
    path: &Path,
    volume: &Path,
    probe: impl Fn(&Path) -> Result<String>,
) -> Result<Value> {
    let volume = std::path::absolute(volume)?;
    nonsymbolic_path(&volume)?;
    let selection = Workspace {
        schema_version: 1,
        volume_uuid: probe(&volume)?,
        volume,
        directory: "cell".into(),
    };
    if let Some(existing) = read_selection(path)? {
        ensure!(
            existing == selection,
            "work storage is already configured; do not create a second queue scope"
        );
        selected_root(&existing, &selection.volume_uuid)?;
        return Ok(serde_json::to_value(existing)?);
    }

    paths::ensure_private(&selection.volume.join("cell"))?;
    selected_root(&selection, &probe(&selection.volume)?)?;
    publish(path, &selection)?;
    Ok(serde_json::to_value(selection)?)
}

pub(crate) fn configure(volume: &Path) -> Result<Value> {
    let _setup = host_setup::setup_lock()?;
    configure_at(&config_path()?, volume, probe)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, Barrier};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(directory.path()).unwrap();
        let volume = base.join("volume");
        fs::create_dir(&volume).unwrap();
        let path = base.join("host/workspace.json");
        (directory, path, volume)
    }

    #[allow(clippy::unnecessary_wraps)] // Match the fallible production probe callback.
    fn fake_probe(_: &Path) -> Result<String> {
        Ok("volume-uuid".into())
    }

    fn observation(volume: &Path) -> Value {
        json!({"MountPoint":volume,"Internal":false,"FilesystemType":"apfs",
            "WritableVolume":true,"GlobalPermissionsEnabled":true,"VolumeUUID":"volume-uuid"})
    }

    #[test]
    fn rejects_incomplete_or_incompatible_volume_observations() {
        let volume = Path::new("/Volumes/CellWork");
        let valid = observation(volume);
        assert_eq!(observed_uuid(volume, &valid).unwrap(), "volume-uuid");
        for (key, value) in [
            ("MountPoint", json!("/Volumes/Another")),
            ("Internal", json!(true)),
            ("FilesystemType", json!("hfs")),
            ("WritableVolume", json!(false)),
            ("GlobalPermissionsEnabled", json!(false)),
            ("VolumeUUID", json!("")),
            ("VolumeUUID", Value::Null),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(observed_uuid(volume, &invalid).is_err(), "{key}");
        }
        for key in valid.as_object().unwrap().keys() {
            let mut invalid = valid.clone();
            invalid.as_object_mut().unwrap().remove(key);
            assert!(observed_uuid(volume, &invalid).is_err(), "{key}");
        }
    }

    #[test]
    fn selects_once_and_preserves_the_existing_file_and_workspace() {
        let (_directory, path, volume) = fixture();
        let configured = configure_at(&path, &volume, fake_probe).unwrap();
        assert_eq!(configured["schema_version"], 1);
        assert_eq!(configured["directory"], "cell");
        assert_eq!(configured["volume_uuid"], "volume-uuid");
        let before = fs::metadata(&path).unwrap();
        assert_eq!(before.mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(volume.join("cell")).unwrap().mode() & 0o777,
            0o700
        );
        fs::write(volume.join("cell/retained"), "journal").unwrap();
        let repeated = configure_at(&path, &volume, fake_probe).unwrap();
        assert_eq!(configured, repeated);
        assert_eq!(before.ino(), fs::metadata(&path).unwrap().ino());
        assert_eq!(
            fs::read_to_string(volume.join("cell/retained")).unwrap(),
            "journal"
        );
        let original = fs::read(&path).unwrap();
        assert!(configure_at(&path, &volume, |_| Ok("replacement".into())).is_err());
        assert_eq!(original, fs::read(&path).unwrap());
    }

    #[test]
    fn rechecks_volume_identity_before_publication_and_allows_retry() {
        let (_directory, path, volume) = fixture();
        let probes = Cell::new(0);
        assert!(
            configure_at(&path, &volume, |_| {
                probes.set(probes.get() + 1);
                Ok(if probes.get() == 1 {
                    "original"
                } else {
                    "replaced"
                }
                .into())
            })
            .is_err()
        );
        assert!(!path.exists());
        assert!(volume.join("cell").is_dir());
        configure_at(&path, &volume, fake_probe).unwrap();
    }

    #[test]
    fn refuses_symlinks_invalid_configuration_and_nonprivate_paths() {
        let (_directory, path, volume) = fixture();
        paths::ensure_private(path.parent().unwrap()).unwrap();
        symlink("missing", &path).unwrap();
        assert!(read_selection(&path).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, "unfinished").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_selection(&path).is_err());
        fs::remove_file(&path).unwrap();
        configure_at(&path, &volume, fake_probe).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_selection(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let selection = require_selection(&path).unwrap();
        fs::set_permissions(volume.join("cell"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(selected_root(&selection, "volume-uuid").is_err());
        fs::remove_dir(volume.join("cell")).unwrap();
        symlink(path.parent().unwrap(), volume.join("cell")).unwrap();
        assert!(selected_root(&selection, "volume-uuid").is_err());
    }

    #[test]
    fn missing_selection_reads_do_not_create_state() {
        let (_directory, path, volume) = fixture();
        assert!(require_selection(&path).is_err());
        assert!(!path.parent().unwrap().exists());
        assert!(!volume.join("cell").exists());
    }

    #[test]
    fn no_clobber_publication_survives_competing_writers() {
        let (_directory, path, volume) = fixture();
        paths::ensure_private(path.parent().unwrap()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let writers = (0..2)
            .map(|number| {
                let path = path.clone();
                let volume = volume.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let selection = Workspace {
                        schema_version: 1,
                        volume,
                        volume_uuid: format!("uuid-{number}"),
                        directory: "cell".into(),
                    };
                    barrier.wait();
                    publish(&path, &selection).is_ok()
                })
            })
            .collect::<Vec<_>>();
        let succeeded = writers
            .into_iter()
            .map(|writer| usize::from(writer.join().unwrap()))
            .sum::<usize>();
        assert_eq!(succeeded, 1);
        assert!(require_selection(&path).is_ok());
    }
}
