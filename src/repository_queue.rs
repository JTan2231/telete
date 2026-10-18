//! A Git repository's shared queue binding and development worktree creation.

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::git;
use crate::paths::{self, Paths};
use crate::store::{self, Config, Store};
use crate::workflow::{self, ProjectConfig};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u32,
    common_git_dir: PathBuf,
    state: PathBuf,
}

fn metadata_directory(common: &Path) -> PathBuf {
    common.join("telete")
}

fn binding(common: &Path) -> Result<Option<Binding>> {
    let directory = metadata_directory(common);
    if !paths::private_directory_exists(&directory)? {
        return Ok(None);
    }
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("queue.json"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read shared queue binding"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == rustix::process::getuid().as_raw()
            && metadata.mode() & 0o777 == 0o600,
        "shared queue binding must be a private user-owned regular file"
    );
    let binding: Binding = serde_json::from_reader(file).context("decode shared queue binding")?;
    ensure!(
        binding.version == 1 && binding.common_git_dir == common,
        "unsupported or changed shared repository binding"
    );
    ensure!(
        paths::normalized_absolute(&binding.state),
        "shared queue state path must be absolute and normalized"
    );
    Ok(Some(binding))
}

fn reject_other_runtime(project: &ProjectConfig, state: &Path) -> Result<()> {
    if project.state != state {
        ensure!(
            store::probe_config(&project.state)?.is_none(),
            "configured state {} contains a different queue; shared queue is {}; drain and retire the other runtime explicitly",
            project.state.display(),
            state.display()
        );
    }
    Ok(())
}

fn runtime(
    project: &ProjectConfig,
    state: &Path,
    repository: &Path,
    common: &Path,
    shared: bool,
    create: bool,
) -> Result<Paths> {
    let mut selected = Paths::selected(state.to_path_buf(), false)?;
    if let Some(previous) = store::probe_config(state)? {
        ensure!(
            previous.state_root == state
                && previous.common_git_dir == common
                && (shared || previous.repository == repository),
            "configured repository or state identity changed"
        );
        if create {
            selected = Paths::selected(state.to_path_buf(), true)?;
        }
    } else if create {
        selected = Paths::selected(state.to_path_buf(), true)?;
        let _admission = store::lock(&state.join("admission.lock"), true)?;
        Store::create(
            state,
            &Config {
                state_root: state.to_path_buf(),
                repository: repository.to_path_buf(),
                common_git_dir: common.to_path_buf(),
            },
        )?;
    }
    // The selected workflow remains local; only operational state is inherited.
    project.workflow.validate()?;
    Ok(selected)
}

fn shared_runtime(
    project: &ProjectConfig,
    repository: &Path,
    common: &Path,
    binding: &Binding,
    create: bool,
) -> Result<Paths> {
    reject_other_runtime(project, &binding.state)?;
    ensure!(
        store::probe_config(&binding.state)?.is_some(),
        "shared queue runtime is missing at {}; restore its operational state before continuing",
        binding.state.display()
    );
    runtime(project, &binding.state, repository, common, true, create)
}

pub(crate) fn select(project: &ProjectConfig, create: bool) -> Result<Paths> {
    project.workflow.validate()?;
    // Git discovery does not write process records or initialize this path.
    let discovery = Paths::discovery(project.state.clone());
    let repository = git::repository(&discovery, &project.repo)?;
    let common = git::common(&discovery, &repository)?;
    if !create {
        return if let Some(binding) = binding(&common)? {
            shared_runtime(project, &repository, &common, &binding, false)
        } else {
            runtime(project, &project.state, &repository, &common, false, false)
        };
    }

    // Serialize first admission across all linked worktrees and configs. The
    // complete runtime is published before its immutable shared binding.
    let directory = metadata_directory(&common);
    let _selection = paths::lock(&directory.join("queue.lock"), true)?;
    if let Some(binding) = binding(&common)? {
        return shared_runtime(project, &repository, &common, &binding, true);
    }
    let selected = runtime(project, &project.state, &repository, &common, true, true)?;
    store::atomic_json(
        &directory.join("queue.json"),
        &Binding {
            version: 1,
            common_git_dir: common,
            state: selected.root.clone(),
        },
    )?;
    Ok(selected)
}

pub(crate) fn create_worktree(project: &ProjectConfig, target: &Path) -> Result<Value> {
    let discovery = Paths::discovery(project.state.clone());
    let repository = git::repository(&discovery, &project.repo)?;
    let commit = git::commit(&discovery, &repository, "HEAD")?;
    ensure!(
        !target.as_os_str().is_empty(),
        "worktree destination is empty"
    );
    let target = workflow::normalized_absolute(target, &std::env::current_dir()?);
    let destination = target
        .to_str()
        .context("worktree destination must be UTF-8")?;
    match fs::symlink_metadata(&target) {
        Ok(_) => anyhow::bail!("worktree destination already exists: {}", target.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let branch = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("worktree destination has no branch name")?;
    ensure!(
        git::value(
            &discovery,
            &repository,
            &["check-ref-format", "--branch", branch],
        )? == branch,
        "worktree branch name must be literal: {branch}"
    );
    let branch_exists = git::run(
        &discovery,
        &repository,
        &[
            "show-ref".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/heads/{branch}"),
        ],
        None,
        std::collections::BTreeMap::new(),
    )?;
    ensure!(
        branch_exists.exited_with(&[1]),
        "worktree branch already exists or could not be checked: {branch}"
    );

    git::value(
        &discovery,
        &repository,
        &["worktree", "add", "-b", branch, destination, &commit.0],
    ).with_context(|| format!(
        "worktree creation failed; inspect branch {branch} and destination {} for partial Git creation before retrying",
        target.display()
    ))?;
    Ok(json!({"path":target,"branch":branch,"commit":commit}))
}
