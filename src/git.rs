//! Committed-source selection and temporary private worktrees.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::model::CommitId;
use crate::paths::Paths;

#[allow(unsafe_code)]
pub(crate) fn configure() -> Result<()> {
    for level in [
        git2::ConfigLevel::System,
        git2::ConfigLevel::Global,
        git2::ConfigLevel::XDG,
        git2::ConfigLevel::ProgramData,
    ] {
        // SAFETY: The CLI calls this once at startup before any repository
        // access or background operations in this process.
        unsafe { git2::opts::set_search_path(level, "")? };
    }
    Ok(())
}

fn open(path: &Path) -> Result<git2::Repository> {
    let path = path
        .canonicalize()
        .with_context(|| format!("repository directory is unavailable: {}", path.display()))?;
    ensure!(
        path.is_dir(),
        "repository path is not a directory: {}",
        path.display()
    );
    git2::Repository::open_ext(&path, git2::RepositoryOpenFlags::empty(), [""; 0])
        .with_context(|| format!("open Git repository at {}", path.display()))
}

fn oid(commit: &CommitId) -> Result<git2::Oid> {
    let format = if commit.0.len() == 64 {
        git2::ObjectFormat::Sha256
    } else {
        git2::ObjectFormat::Sha1
    };
    Ok(git2::Oid::from_str_ext(&commit.0, format)?)
}

fn commit_id(id: git2::Oid) -> Result<CommitId> {
    CommitId::new(id.to_string())
}

pub(crate) fn commit(_paths: &Paths, repo: &Path, revision: &str) -> Result<CommitId> {
    let repository = open(repo)?;
    let commit = repository
        .revparse_single(revision)
        .and_then(|object| object.peel_to_commit())
        .with_context(|| format!("resolve {revision} to a commit in {}", repo.display()))?;
    commit_id(commit.id())
}

pub(crate) fn common(_paths: &Paths, repo: &Path) -> Result<PathBuf> {
    Ok(open(repo)?.commondir().canonicalize()?)
}

pub(crate) fn repository(_paths: &Paths, repo: &Path) -> Result<PathBuf> {
    Ok(open(repo)?
        .workdir()
        .context("configured repository has no working tree")?
        .canonicalize()?)
}

fn local_branch(branch: &str) -> Result<String> {
    ensure!(
        git2::Branch::name_is_valid(branch)?,
        "expected a literal local branch name: {branch}"
    );
    Ok(format!("refs/heads/{branch}"))
}

pub(crate) fn branch_tip(_paths: &Paths, repo: &Path, branch: &str) -> Result<CommitId> {
    let reference = local_branch(branch)?;
    let repository = open(repo)?;
    let commit = repository
        .find_reference(&reference)
        .and_then(|reference| reference.peel_to_commit())
        .with_context(|| format!("cannot resolve local branch {branch}"))?;
    commit_id(commit.id())
}

fn linked_worktrees(repo: &git2::Repository) -> Result<Vec<(PathBuf, git2::Worktree)>> {
    let worktrees = repo.worktrees()?;
    worktrees
        .iter()
        .map(|name| {
            let name = name?.context("worktree registration has no name")?;
            let git_dir = repo.commondir().join("worktrees").join(name);
            ensure!(
                !fs::symlink_metadata(&git_dir)?.is_symlink(),
                "symbolic worktree registration"
            );
            Ok((git_dir, repo.find_worktree(name)?))
        })
        .collect()
}

fn branch_checkouts(repo: &git2::Repository, reference: &str) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut attached = Vec::new();
    if let Some(path) = repo.workdir()
        && repo.find_reference("HEAD")?.symbolic_target()? == Some(reference)
    {
        attached.push((path.to_path_buf(), repo.path().to_path_buf()));
    }
    for (git_dir, checkout) in linked_worktrees(repo)? {
        // A missing checkout still reserves its branch through its registration.
        let head = fs::read_to_string(git_dir.join("HEAD"))?;
        if head.trim_end().strip_prefix("ref: ") == Some(reference) {
            attached.push((checkout.path().to_path_buf(), git_dir));
        }
    }
    Ok(attached)
}

fn checkout_idle(repo: &git2::Repository) -> Result<()> {
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_START",
        "index.lock",
    ] {
        ensure!(
            !repo.path().join(marker).try_exists()?,
            "Git operation in progress in {}",
            repo.path().display()
        );
    }
    Ok(())
}

fn checkout_clean(repo: &git2::Repository, include_ignored: bool) -> Result<()> {
    let workdir = repo
        .workdir()
        .context("Git worktree has no working directory")?;
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .include_ignored(include_ignored);
    ensure!(
        repo.statuses(Some(&mut options))?.is_empty(),
        "worktree has local files or changes: {}",
        workdir.display()
    );
    for submodule in repo.submodules()? {
        let name = submodule.name().context("submodule name must be UTF-8")?;
        let status = repo.submodule_status(name, git2::SubmoduleIgnore::None)?;
        ensure!(
            !status.intersects(
                git2::SubmoduleStatus::WD_MODIFIED
                    | git2::SubmoduleStatus::WD_INDEX_MODIFIED
                    | git2::SubmoduleStatus::WD_WD_MODIFIED
                    | git2::SubmoduleStatus::WD_UNTRACKED
            ),
            "worktree submodule has local files or changes: {}",
            workdir.join(submodule.path()).display()
        );
    }
    Ok(())
}

/// Advance a local branch without overwriting independent work.
pub(crate) fn publish_branch(
    paths: &Paths,
    repo: &Path,
    branch: &str,
    expected: &CommitId,
    candidate: &CommitId,
) -> Result<()> {
    let reference = local_branch(branch)?;
    let observed = branch_tip(paths, repo, branch)?;
    if is_ancestor(paths, repo, candidate, &observed)? {
        return Ok(());
    }
    ensure!(
        observed == *expected,
        "local branch {branch} advanced from {expected} to {observed}"
    );
    ensure!(
        is_ancestor(paths, repo, expected, candidate)?,
        "candidate {candidate} cannot fast-forward local branch {branch}"
    );
    let repository = open(repo)?;
    let main = open(repository.commondir())?;
    let mut transaction = main.transaction()?;
    transaction.lock_ref(&reference)?;
    let old = oid(expected)?;
    ensure!(
        main.find_reference(&reference)?.target() == Some(old),
        "local branch {branch} changed before integration"
    );
    let checkout = match branch_checkouts(&main, &reference)?.as_slice() {
        [] => {
            // A rebase or bisect temporarily detaches HEAD while retaining
            // ownership of the branch it will restore.
            ensure_detached_branch_idle(&main, &reference, branch)?;
            None
        }
        [(path, git_dir)] => {
            let checkout = open(path)?;
            ensure!(
                checkout.commondir() == main.commondir() && checkout.path() == git_dir,
                "destination checkout no longer matches its Git registration"
            );
            Some(checkout)
        }
        _ => bail!("local branch {branch} is checked out more than once"),
    };
    let mut checkout_head = checkout
        .as_ref()
        .map(git2::Repository::transaction)
        .transpose()?;
    let candidate_oid = oid(candidate)?;
    let signature = git2::Signature::now("Telete", "telete@localhost")?;
    transaction.set_target(
        &reference,
        candidate_oid,
        Some(&signature),
        "Telete: integrate prepared release",
    )?;
    if let (Some(checkout), Some(head_lock)) = (&checkout, &mut checkout_head) {
        head_lock.lock_ref("HEAD")?;
        checkout_idle(checkout)?;
        checkout_clean(checkout, false)?;
        ensure!(
            checkout.find_reference("HEAD")?.symbolic_target()? == Some(reference.as_str())
                && checkout.head()?.target() == Some(old),
            "destination checkout changed before integration"
        );
        // Publish the ref only after checkout writes complete. If interrupted
        // first, a changed checkout against the old tip fails the clean check.
        let mut options = git2::build::CheckoutBuilder::new();
        options.safe().overwrite_ignored(false);
        let candidate_tree = checkout.find_commit(candidate_oid)?.tree()?;
        checkout.checkout_tree(candidate_tree.as_object(), Some(&mut options))?;
    }
    transaction.commit()?;
    ensure!(
        is_ancestor(paths, repo, candidate, &branch_tip(paths, repo, branch)?)?,
        "local branch {branch} does not contain integrated candidate {candidate}"
    );
    Ok(())
}

fn ensure_detached_branch_idle(
    repo: &git2::Repository,
    reference: &str,
    branch: &str,
) -> Result<()> {
    let mut checkouts: Vec<_> = linked_worktrees(repo)?
        .into_iter()
        .map(|(git_dir, tree)| (git_dir, tree.path().to_path_buf()))
        .collect();
    if let Some(workdir) = repo.workdir() {
        checkouts.push((repo.path().to_path_buf(), workdir.to_path_buf()));
    }
    for (git_dir, checkout) in checkouts {
        if !checkout.try_exists()? || fs::read_to_string(git_dir.join("HEAD"))?.starts_with("ref: ")
        {
            continue;
        }
        for (marker, branch_name) in [
            ("rebase-merge/head-name", reference),
            ("rebase-apply/head-name", reference),
            ("BISECT_START", branch),
        ] {
            match fs::read_to_string(git_dir.join(marker)) {
                Ok(owner) => ensure!(
                    owner.trim() != branch_name,
                    "Git operation in progress on local branch {branch} in {}",
                    checkout.display()
                ),
                Err(error) if error.kind() == ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

pub(crate) fn merge_worktree_source(
    paths: &Paths,
    repo: &Path,
    checkout: &Path,
    source: &CommitId,
) -> Result<()> {
    let repository = open_private_worktree(paths, checkout)?;
    ensure!(
        common(paths, checkout)? == common(paths, repo)?,
        "candidate belongs to another repository"
    );
    ensure!(
        repository.head_detached()?,
        "candidate checkout must have a detached HEAD"
    );
    ensure!(
        repository.state() == git2::RepositoryState::Clean,
        "Git operation in progress in {}",
        checkout.display()
    );
    let head = repository.head()?.peel_to_commit()?;
    let incoming = repository.find_commit(oid(source)?)?;
    let base = repository
        .merge_base(head.id(), incoming.id())
        .with_context(|| format!("could not find a merge base for checked source {source}"))?;
    let mut options = git2::build::CheckoutBuilder::new();
    options.safe().overwrite_ignored(false);

    if base == incoming.id() || base == head.id() {
        repository.reference("ORIG_HEAD", head.id(), true, "Telete: merge checked source")?;
        if base != incoming.id() {
            repository.checkout_tree(incoming.as_object(), Some(&mut options))?;
            repository.reference_matching(
                "HEAD",
                incoming.id(),
                true,
                head.id(),
                "Telete: fast-forward to checked source",
            )?;
        }
        return Ok(());
    }

    let annotated = repository.find_annotated_commit(incoming.id())?;
    repository.merge(&[&annotated], None, Some(&mut options))?;
    let mut index = repository.index()?;
    ensure!(
        !index.has_conflicts(),
        "could not merge checked source {source}: merge conflicts"
    );
    let tree = repository.find_tree(index.write_tree()?)?;
    let identity = git2::Signature::now("Telete", "telete@localhost")?;
    let message = format!("Merge commit '{source}' into HEAD\n");
    repository.commit(
        Some("HEAD"),
        &identity,
        &identity,
        &message,
        &tree,
        &[&head, &incoming],
    )?;
    repository.cleanup_state()?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WorktreeOwner {
    id: String,
    branch: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DevelopmentWorktree {
    path: PathBuf,
    owner: WorktreeOwner,
    submitted: CommitId,
}

impl DevelopmentWorktree {
    pub(crate) fn branch(&self) -> &str {
        &self.owner.branch
    }
}

fn worktree_owner_path(checkout: &git2::Repository) -> Option<PathBuf> {
    let registration = checkout.path();
    let linked = checkout.commondir().join("worktrees");
    (registration.parent() == Some(linked.as_path()))
        .then(|| registration.join("telete").join("worktree.json"))
}

pub(crate) fn add_development_worktree(
    paths: &Paths,
    repo: &Path,
    target: &Path,
    branch: &str,
    commit: &CommitId,
) -> Result<()> {
    let reference = local_branch(branch)?;
    let repo = open(repo)?;
    let registrations = repo.commondir().join("worktrees");
    let mut name = branch.to_owned();
    for suffix in 1.. {
        match fs::symlink_metadata(registrations.join(&name)) {
            Ok(_) => name = format!("{branch}{suffix}"),
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    let parent = target
        .parent()
        .context("worktree destination has no parent")?;
    let destination = if parent.try_exists()? {
        parent.canonicalize()?.join(branch)
    } else {
        target.to_path_buf()
    };
    ensure!(
        !linked_worktrees(&repo)?
            .iter()
            .any(|(_, worktree)| worktree.path() == destination),
        "worktree destination is already registered: {}",
        target.display()
    );
    let source = repo.find_commit(oid(commit)?)?;
    let branch_ref = repo
        .branch(branch, &source, false)
        .with_context(|| format!("cannot create local branch {branch}"))?;
    (|| -> Result<()> {
        fs::create_dir_all(parent)?;
        let mut options = git2::WorktreeAddOptions::new();
        options.reference(Some(branch_ref.get()));
        repo.worktree(&name, target, Some(&options))?;
        Ok(())
    })().with_context(|| format!(
        "worktree creation failed; inspect branch {branch} and destination {} for partial Git creation before retrying",
        target.display()
    ))?;
    let checkout = open(target)?;
    ensure!(
        checkout.find_reference("HEAD")?.symbolic_target()? == Some(reference.as_str())
            && checkout.head()?.target() == Some(source.id()),
        "new development worktree does not match branch {branch} at {commit}"
    );
    record_development_worktree(paths, target, branch).with_context(|| {
        format!(
            "worktree was created at {} but could not be recorded for cleanup",
            target.display()
        )
    })
}

pub(crate) fn record_development_worktree(
    _paths: &Paths,
    checkout: &Path,
    branch: &str,
) -> Result<()> {
    let marker =
        worktree_owner_path(&open(checkout)?).context("worktree is not a linked checkout")?;
    crate::paths::atomic_json(
        &marker,
        &WorktreeOwner {
            id: uuid::Uuid::now_v7().to_string(),
            branch: branch.to_owned(),
        },
    )
}

fn worktree_owner(checkout: &git2::Repository) -> Result<Option<WorktreeOwner>> {
    let Some(marker) = worktree_owner_path(checkout) else {
        return Ok(None);
    };
    let parent = marker
        .parent()
        .context("worktree identity has no directory")?;
    if !crate::paths::private_directory_exists(parent)? {
        return Ok(None);
    }
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&marker)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read Telete worktree identity"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == rustix::process::getuid().as_raw()
            && metadata.mode() & 0o777 == 0o600,
        "Telete worktree identity must be a private user-owned regular file"
    );
    let owner = serde_json::from_reader(file).context("decode Telete worktree identity")?;
    Ok(Some(owner))
}

pub(crate) fn development_worktree(
    paths: &Paths,
    repo: &Path,
    commit: &CommitId,
) -> Result<Option<DevelopmentWorktree>> {
    let path = repository(paths, repo)?;
    let checkout = open(&path)?;
    let Some(owner) = worktree_owner(&checkout)? else {
        return Ok(None);
    };
    let reference = local_branch(&owner.branch)?;
    if checkout.find_reference("HEAD")?.symbolic_target()? != Some(reference.as_str())
        || checkout.head()?.target() != Some(oid(commit)?)
    {
        return Ok(None);
    }
    Ok(Some(DevelopmentWorktree {
        path,
        owner,
        submitted: commit.clone(),
    }))
}

pub(crate) fn cleanup_development_worktree(
    paths: &Paths,
    repo: &Path,
    worktree: &DevelopmentWorktree,
    checked: &CommitId,
    integrated: &CommitId,
) -> Result<()> {
    let reference = local_branch(&worktree.owner.branch)?;
    ensure!(
        !paths.root.starts_with(&worktree.path),
        "development worktree contains the active queue: {}",
        worktree.path.display()
    );
    let repository = open(repo)?;
    let main = open(repository.commondir())?;
    let metadata = match fs::symlink_metadata(&worktree.path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut transaction = main.transaction()?;
    transaction.lock_ref(&reference)?;
    let branch = match main.find_reference(&reference) {
        Ok(branch) => branch,
        Err(error) if error.code() == git2::ErrorCode::NotFound && metadata.is_none() => {
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("development branch disappeared: {reference}"));
        }
    };
    let observed = commit_id(branch.target().context("development branch is symbolic")?)?;
    ensure!(
        observed == worktree.submitted || observed == *checked,
        "development branch has advanced: {reference}"
    );
    ensure!(
        is_ancestor(paths, repo, checked, integrated)?
            && is_ancestor(paths, repo, &observed, integrated)?,
        "integrated candidate does not include development branch {reference}"
    );

    let checkouts = branch_checkouts(&main, &reference)?;
    if let Some(metadata) = metadata {
        let [(path, git_dir)] = checkouts.as_slice() else {
            bail!("development checkout changed: {}", worktree.path.display());
        };
        let checkout = open(&worktree.path)?;
        ensure!(
            metadata.is_dir()
                && *path == worktree.path
                && worktree.path.canonicalize()? == worktree.path
                && checkout.path() == git_dir
                && checkout.commondir() == main.commondir()
                && worktree_owner(&checkout)?.as_ref() == Some(&worktree.owner),
            "development checkout changed: {}",
            worktree.path.display()
        );
        checkout_idle(&checkout)?;
        checkout_clean(&checkout, true)?;
        ensure!(
            checkout.find_reference("HEAD")?.symbolic_target()? == Some(reference.as_str())
                && checkout.head()?.target() == branch.target(),
            "development checkout advanced before cleanup"
        );
        ensure!(
            !checkout.path().join("modules").try_exists()?
                && !checkout.submodules()?.iter().any(|module| worktree
                    .path
                    .join(module.path())
                    .join(".git")
                    .exists()),
            "cannot remove a worktree containing initialized submodules"
        );
        let registered = git2::Worktree::open_from_repository(&checkout)?;
        registered.validate()?;
        ensure!(
            registered.path() == worktree.path
                && registered.is_locked()? == git2::WorktreeLockStatus::Unlocked,
            "development checkout is locked or moved: {}",
            worktree.path.display()
        );
        let mut options = git2::WorktreePruneOptions::new();
        options.valid(true).working_tree(true);
        registered.prune(Some(&mut options))?;
    } else {
        ensure!(
            checkouts.is_empty(),
            "development branch is checked out at another path"
        );
    }
    ensure!(
        branch_checkouts(&main, &reference)?.is_empty(),
        "development branch is checked out"
    );
    ensure_detached_branch_idle(&main, &reference, &worktree.owner.branch)?;
    transaction.remove(&reference)?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn private_ref(job: &str) -> Result<String> {
    crate::model::JobId::new(job)?;
    Ok(format!("refs/telete/jobs/{job}"))
}

pub(crate) fn result_ref(job: &str) -> Result<String> {
    crate::model::JobId::new(job)?;
    Ok(format!("refs/telete/results/{job}"))
}

pub(crate) fn release_ref(job: &str) -> Result<String> {
    crate::model::JobId::new(job)?;
    Ok(format!("refs/telete/releases/{job}"))
}

pub(crate) fn release_target_ref(target: &str) -> Result<String> {
    crate::model::JobId::new(target)?;
    Ok(format!("refs/telete/releases/targets/{target}"))
}

fn managed_ref(reference: &str) -> bool {
    reference.starts_with("refs/telete/jobs/")
        || reference.starts_with("refs/telete/results/")
        || reference.starts_with("refs/telete/releases/")
}

pub(crate) fn ref_value(_paths: &Paths, repo: &Path, reference: &str) -> Result<Option<CommitId>> {
    ensure!(
        managed_ref(reference),
        "Telete cannot read another manager's refs"
    );
    managed_ref_target(&open(repo)?, reference)
}

fn managed_ref_target(repo: &git2::Repository, name: &str) -> Result<Option<CommitId>> {
    match repo.find_reference(name) {
        Ok(reference) => {
            let id = reference.target().context("Telete ref must be direct")?;
            Ok(Some(commit_id(id)?))
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read Telete ref {name}")),
    }
}

pub(crate) fn advance(
    _paths: &Paths,
    repo: &Path,
    reference: &str,
    old: Option<&CommitId>,
    new: &CommitId,
) -> Result<()> {
    ensure!(
        managed_ref(reference),
        "Telete cannot mutate another manager's refs"
    );
    let repo = open(repo)?;
    if managed_ref_target(&repo, reference)?.as_ref() == Some(new) {
        return Ok(());
    }
    let mut transaction = repo.transaction()?;
    transaction
        .lock_ref(reference)
        .with_context(|| format!("lock Telete ref {reference}"))?;
    let observed = managed_ref_target(&repo, reference)?;
    if observed.as_ref() == Some(new) {
        return Ok(());
    }
    ensure!(
        observed.as_ref() == old,
        "private Git ref changed before promotion: {reference}"
    );
    let target = repo.find_commit(oid(new)?)?;
    transaction.set_target(
        reference,
        target.id(),
        Some(&git2::Signature::now("Telete", "telete@localhost")?),
        "Telete: retain source",
    )?;
    transaction.commit().context("retain Telete ref")
}

pub(crate) fn forget(_paths: &Paths, repo: &Path, job: &str, commit: &CommitId) -> Result<()> {
    let reference = private_ref(job)?;
    let repo = open(repo)?;
    if managed_ref_target(&repo, &reference)?.is_none() {
        return Ok(());
    }
    let mut transaction = repo.transaction()?;
    transaction.lock_ref(&reference)?;
    if let Some(observed) = managed_ref_target(&repo, &reference)? {
        ensure!(
            observed == *commit,
            "private source ref changed before cleanup"
        );
        transaction.remove(&reference)?;
        transaction.commit()?;
    }
    Ok(())
}

fn private_worktree(paths: &Paths, path: &Path) -> Result<()> {
    let parent = path.parent().context("candidate has no job directory")?;
    ensure!(
        crate::paths::normalized_absolute(path)
            && parent.parent() == Some(paths.root.join("jobs").as_path())
            && path.file_name().is_some_and(|name| name == "worktree"),
        "worktree is outside Telete's jobs"
    );
    crate::paths::private_directory_exists(parent)?;
    without_symlink(path)?;
    Ok(())
}

fn without_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "symbolic Git path: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(crate) fn ensure_worktree(
    paths: &Paths,
    repo: &Path,
    path: &Path,
    revision: &CommitId,
) -> Result<()> {
    private_worktree(paths, path)?;
    crate::paths::ensure_private(path.parent().context("candidate has no job directory")?)?;
    let marker = path.join(".git");
    without_symlink(&marker)?;
    if !marker.try_exists()? {
        create_private_worktree(paths, repo, path, revision)?;
    }
    ensure!(
        common(paths, path)? == common(paths, repo)?,
        "candidate belongs to another repository"
    );
    reset_private(paths, path, revision)?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout
        .force()
        .remove_untracked(true)
        .overwrite_ignored(false);
    open(path)?.checkout_head(Some(&mut checkout))?;
    Ok(())
}

fn create_private_worktree(
    paths: &Paths,
    repo: &Path,
    path: &Path,
    revision: &CommitId,
) -> Result<()> {
    let common = common(paths, repo)?;
    open(repo)?.find_commit(oid(revision)?)?;
    fs::create_dir_all(path)?;
    ensure!(
        fs::read_dir(path)?.next().is_none(),
        "candidate directory is occupied: {}",
        path.display()
    );
    let registration = if let Some(retained) = registration(paths, repo, path)? {
        retained
    } else {
        // libgit2's linked-worktree constructor requires a local branch.
        // Register the detached job directly, then let libgit2 populate it.
        let parent = common.join("worktrees");
        without_symlink(&parent)?;
        fs::create_dir_all(&parent)?;
        let staging = tempfile::Builder::new()
            .prefix("telete-")
            .tempdir_in(&common)?;
        for (name, value) in [
            ("commondir", format!("{}\n", common.display())),
            ("gitdir", format!("{}\n", path.join(".git").display())),
            ("HEAD", format!("{revision}\n")),
        ] {
            let mut file = File::create(staging.path().join(name))?;
            file.write_all(value.as_bytes())?;
            file.sync_all()?;
        }
        File::open(staging.path())?.sync_all()?;
        let registration = parent.join(
            staging
                .path()
                .file_name()
                .context("missing worktree name")?,
        );
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            staging.path(),
            rustix::fs::CWD,
            &registration,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        File::open(&parent)?.sync_all()?;
        registration
    };
    let mut marker = tempfile::NamedTempFile::new_in(path)?;
    writeln!(marker, "gitdir: {}", registration.display())?;
    marker.as_file().sync_all()?;
    marker
        .persist_noclobber(path.join(".git"))
        .map_err(|error| error.error)?;
    File::open(path)?.sync_all()?;
    Ok(())
}

fn validate_registration(
    paths: &Paths,
    repo: &Path,
    worktree: &Path,
    registration: &Path,
) -> Result<()> {
    let parent = common(paths, repo)?.join("worktrees");
    ensure!(
        registration.is_absolute() && registration.parent() == Some(parent.as_path()),
        "candidate registration is outside linked-worktree metadata"
    );
    for path in [&parent, registration] {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "symbolic candidate registration"
            );
        }
    }
    let marker = registration.join("gitdir");
    if marker.exists() {
        ensure!(
            !fs::symlink_metadata(&marker)?.file_type().is_symlink(),
            "symbolic registration marker"
        );
        ensure!(
            Path::new(fs::read_to_string(marker)?.trim()) == worktree.join(".git"),
            "registration belongs to another worktree"
        );
    }
    Ok(())
}

pub(crate) fn registration(paths: &Paths, repo: &Path, worktree: &Path) -> Result<Option<PathBuf>> {
    private_worktree(paths, worktree)?;
    let marker = worktree.join(".git");
    if marker.exists() {
        ensure!(
            !fs::symlink_metadata(&marker)?.file_type().is_symlink(),
            "symbolic candidate Git marker"
        );
        let text = fs::read_to_string(marker)?;
        let registration = PathBuf::from(
            text.trim()
                .strip_prefix("gitdir: ")
                .context("invalid worktree Git marker")?,
        );
        validate_registration(paths, repo, worktree, &registration)?;
        return Ok(Some(registration));
    }
    let parent = common(paths, repo)?.join("worktrees");
    if parent.exists() {
        ensure!(
            !fs::symlink_metadata(&parent)?.file_type().is_symlink(),
            "symbolic linked-worktree directory"
        );
        for entry in fs::read_dir(parent)? {
            let path = entry?.path();
            let marker = path.join("gitdir");
            if marker.is_file()
                && !fs::symlink_metadata(&marker)?.file_type().is_symlink()
                && fs::read_to_string(&marker)?.trim()
                    == worktree.join(".git").display().to_string()
            {
                validate_registration(paths, repo, worktree, &path)?;
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

pub(crate) fn remove_worktree(
    paths: &Paths,
    repo: &Path,
    worktree: &Path,
    retained: Option<&Path>,
) -> Result<()> {
    private_worktree(paths, worktree)?;
    let common = common(paths, repo)?;
    ensure!(
        open(&common)?
            .workdir()
            .is_none_or(|primary| primary != worktree),
        "cannot remove primary worktree"
    );
    let current = registration(paths, repo, worktree)?;
    let registration = if let Some(path) = retained {
        validate_registration(paths, repo, worktree, path)?;
        ensure!(
            current.as_deref().is_none_or(|current| current == path),
            "candidate registration changed before cleanup"
        );
        Some(path.to_path_buf())
    } else {
        current
    };
    if worktree.try_exists()? {
        fs::remove_dir_all(worktree)?;
    }
    if let Some(path) = registration
        && path.try_exists()?
    {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

/// Retain automatic edits independently of the execution checkout. A separate
/// index excludes untracked generated files and ignores a command's staging.
pub(crate) fn retain_fixes(
    paths: &Paths,
    repo: &Path,
    checkout: &Path,
    job: &crate::store::Job,
) -> Result<CommitId> {
    let repository = open_private_worktree(paths, checkout)?;
    ensure!(
        common(paths, checkout)? == common(paths, repo)?,
        "fix checkout belongs to another repository"
    );
    ensure!(
        repository.head_detached()?,
        "fix checkout must keep a detached HEAD"
    );
    let reference = result_ref(&job.id.0)?;
    let head = commit_id(repository.head()?.peel_to_commit()?.id())?;
    if let Some(retained) = ref_value(paths, repo, &reference)? {
        ensure!(
            head == job.commit || head == retained,
            "fix command changed its checkout HEAD"
        );
        if retained != job.commit {
            ensure!(
                parents(checkout, &retained)? == [job.commit.clone()],
                "retained fix commit is not a child of submitted source"
            );
        }
        reset_private(paths, checkout, &retained)?;
        return Ok(retained);
    }
    ensure!(head == job.commit, "fix command changed its checkout HEAD");
    let submitted = repository.find_commit(oid(&job.commit)?)?;
    let original_tree = submitted.tree()?;
    let mut index = repository.index()?;
    // Work from a fresh snapshot of the submitted tree. This index is never
    // written to disk, and the later hard reset opens the real index afresh.
    index.clear()?;
    index.read_tree(&original_tree)?;
    index.update_all(std::iter::empty::<&str>(), None)?;
    let tree = index.write_tree()?;
    let result = if tree == original_tree.id() {
        job.commit.clone()
    } else {
        let time = git2::Time::new(job.created.try_into()?, 0);
        let identity = git2::Signature::new("Telete", "telete@localhost", &time)?;
        let message = format!("Apply automatic source updates\n\nTelete-Job: {}\n", job.id);
        commit_id(repository.commit(
            None,
            &identity,
            &identity,
            &message,
            &repository.find_tree(tree)?,
            &[&submitted],
        )?)?
    };
    advance(paths, repo, &reference, None, &result)?;
    reset_private(paths, checkout, &result)?;
    Ok(result)
}

fn open_private_worktree(paths: &Paths, checkout: &Path) -> Result<git2::Repository> {
    private_worktree(paths, checkout)?;
    let repository = open(checkout)?;
    let root = repository.workdir().map(Path::canonicalize).transpose()?;
    ensure!(
        repository.is_worktree() && root.as_deref() == Some(checkout.canonicalize()?.as_path()),
        "repository worktree does not match private checkout"
    );
    Ok(repository)
}

pub(crate) fn reset_private(paths: &Paths, checkout: &Path, commit: &CommitId) -> Result<()> {
    let repository = open_private_worktree(paths, checkout)?;
    ensure!(
        repository.head_detached()?,
        "workflow checkout must keep a detached HEAD"
    );
    let previous = repository.head()?.peel_to_commit()?.id();
    let target = repository.find_commit(oid(commit)?)?;
    repository.reset(target.as_object(), git2::ResetType::Hard, None)?;
    repository.reference(
        "ORIG_HEAD",
        previous,
        true,
        "Telete: reset private checkout",
    )?;
    Ok(())
}

pub(crate) fn ensure_detached(paths: &Paths, checkout: &Path) -> Result<()> {
    ensure!(
        open_private_worktree(paths, checkout)?.head_detached()?,
        "workflow checkout must keep a detached HEAD"
    );
    Ok(())
}

pub(crate) fn unchanged(paths: &Paths, checkout: &Path, revision: &CommitId) -> Result<()> {
    let repository = open_private_worktree(paths, checkout)?;
    ensure!(
        repository.head()?.peel_to_commit()?.id() == oid(revision)?,
        "workflow changed its checked commit"
    );
    let mut options = git2::StatusOptions::new();
    options.include_untracked(false).include_ignored(false);
    for entry in repository.statuses(Some(&mut options))?.iter() {
        // libgit2 can report untracked submodule files as a working-tree change
        // even when untracked paths were excluded from the parent status.
        let modified_submodule = entry.status() == git2::Status::WT_MODIFIED
            && entry.index_to_workdir().is_some_and(|delta| {
                delta.old_file().mode() == git2::FileMode::Commit
                    && delta.new_file().mode() == git2::FileMode::Commit
            });
        ensure!(
            modified_submodule
                && !repository
                    .submodule_status(entry.path()?, git2::SubmoduleIgnore::Untracked)?
                    .intersects(
                        git2::SubmoduleStatus::WD_MODIFIED
                            | git2::SubmoduleStatus::WD_INDEX_MODIFIED
                            | git2::SubmoduleStatus::WD_WD_MODIFIED
                    ),
            "workflow changed tracked source after automatic edits"
        );
    }
    Ok(())
}

pub(crate) fn merge_head(paths: &Paths, checkout: &Path) -> Result<Option<CommitId>> {
    let path = open_private_worktree(paths, checkout)?
        .path()
        .join("MERGE_HEAD");
    match fs::read_to_string(path) {
        Ok(head) => Ok(Some(CommitId::new(head.trim())?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn has_conflicts(checkout: &Path) -> Result<bool> {
    Ok(open(checkout)?.index()?.has_conflicts())
}

pub(crate) fn parents(checkout: &Path, commit: &CommitId) -> Result<Vec<CommitId>> {
    let repository = open(checkout)?;
    repository
        .find_commit(oid(commit)?)?
        .parent_ids()
        .map(commit_id)
        .collect()
}

pub(crate) fn is_ancestor(
    _paths: &Paths,
    repo: &Path,
    ancestor: &CommitId,
    revision: &CommitId,
) -> Result<bool> {
    let repo = open(repo)?;
    let ancestor = oid(ancestor)?;
    let revision = oid(revision)?;
    if ancestor == revision {
        repo.find_commit(ancestor)?;
        Ok(true)
    } else {
        repo.graph_descendant_of(revision, ancestor)
            .context("Git ancestry check failed")
    }
}

/// Retain a project's composed release without changing any project branch.
/// Every member and the preceding release must be present in its ancestry.
pub(crate) fn retain_release<'a>(
    paths: &Paths,
    repo: &Path,
    checkout: &Path,
    job: &str,
    ancestors: impl IntoIterator<Item = &'a CommitId>,
    previous: Option<&CommitId>,
    revision: &CommitId,
) -> Result<()> {
    ensure!(
        common(paths, checkout)? == common(paths, repo)?,
        "release checkout belongs to another repository"
    );
    ensure_detached(paths, checkout)?;
    unchanged(paths, checkout, revision)?;
    for ancestor in ancestors {
        ensure!(
            is_ancestor(paths, repo, ancestor, revision)?,
            "release commit {revision} must include source {ancestor}"
        );
    }
    advance(paths, repo, &release_ref(job)?, previous, revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_source_survives_in_a_detached_job_worktree() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let paths = Paths::discovery(root.join("queue"));
        let path = root.join("repo");
        let repo = git2::Repository::init_opts(
            &path,
            git2::RepositoryInitOptions::new()
                .initial_head("main")
                .object_format(git2::ObjectFormat::Sha256),
        )?;
        fs::write(path.join("source"), "committed content")?;
        crate::test_support::record(&repo, &["source"], "source")?;
        let source = commit(&paths, &path, "HEAD")?;
        assert_eq!(source.0.len(), 64);
        let reference = private_ref("sha256")?;
        let checkout = paths.job("sha256").join("worktree");
        advance(&paths, &path, &reference, None, &source)?;
        ensure_worktree(&paths, &path, &checkout, &source)?;
        ensure_detached(&paths, &checkout)?;
        assert_eq!(commit(&paths, &checkout, "HEAD")?, source);
        assert_eq!(
            fs::read_to_string(checkout.join("source"))?,
            "committed content"
        );
        assert_eq!(ref_value(&paths, &path, &reference)?, Some(source));
        Ok(())
    }

    #[test]
    fn checked_out_publication_keeps_the_old_tip_when_the_index_is_locked() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let paths = Paths::discovery(root.join("queue"));
        let path = root.join("repo");
        let repo = crate::test_support::init(&path)?;
        fs::write(path.join("source"), "before")?;
        let base = crate::test_support::record(&repo, &["source"], "base")?;
        crate::test_support::branch(&path, "change", "HEAD")?;
        crate::test_support::switch(&path, "change")?;
        fs::write(path.join("source"), "after")?;
        let candidate = crate::test_support::record(&repo, &["source"], "candidate")?;
        crate::test_support::switch(&path, "main")?;
        let lock = repo.path().join("index.lock");
        fs::write(&lock, "held by another operation")?;
        let base = CommitId::new(base.to_string())?;
        let candidate = CommitId::new(candidate.to_string())?;

        assert!(publish_branch(&paths, &path, "main", &base, &candidate).is_err());
        assert_eq!(branch_tip(&paths, &path, "main")?, base);
        assert_eq!(fs::read_to_string(path.join("source"))?, "before");
        assert_eq!(fs::read_to_string(lock)?, "held by another operation");
        Ok(())
    }

    #[test]
    fn cleanup_preserves_ignored_worktree_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let paths = Paths::discovery(root.join("queue"));
        let repo = root.join("repo");
        let checkout = root.join("topic");
        let fixture = crate::test_support::init(&repo)?;
        fs::write(repo.join(".gitignore"), "local-data\n")?;
        let head = CommitId::new(
            crate::test_support::record(&fixture, &[".gitignore"], "Initial commit")?.to_string(),
        )?;
        crate::test_support::worktree(&repo, "topic", &checkout, &head.0)?;
        assert!(development_worktree(&paths, &checkout, &head)?.is_none());
        record_development_worktree(&paths, &checkout, "topic")?;
        let worktree = development_worktree(&paths, &checkout, &head)?
            .context("Telete-owned worktree was not recorded")?;

        let ignored = checkout.join("local-data");
        fs::write(&ignored, "keep")?;
        assert!(cleanup_development_worktree(&paths, &repo, &worktree, &head, &head).is_err());
        assert_eq!(fs::read_to_string(&ignored)?, "keep");
        assert_eq!(branch_tip(&paths, &repo, "topic")?, head);

        fs::remove_file(ignored)?;
        cleanup_development_worktree(&paths, &repo, &worktree, &head, &head)?;
        cleanup_development_worktree(&paths, &repo, &worktree, &head, &head)?;
        assert!(!checkout.exists());
        assert!(branch_tip(&paths, &repo, "topic").is_err());
        Ok(())
    }
}
