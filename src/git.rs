//! Committed-source selection and temporary private worktrees.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::model::CommitId;
use crate::paths::Paths;
use crate::process::{self, CommandSpec, ProcessResult};

pub(crate) fn run(
    paths: &Paths,
    repo: &Path,
    args: &[String],
    stdin: Option<String>,
    extra: BTreeMap<String, String>,
) -> Result<ProcessResult> {
    let repo = repo
        .canonicalize()
        .context("Git repository directory is unavailable")?;
    let mut arguments = vec![
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "commit.gpgSign=false".into(),
        "-C".into(),
        repo.display().to_string(),
    ];
    arguments.extend_from_slice(args);
    let mut environment = BTreeMap::new();
    environment.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    environment.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    environment.extend(extra);
    // Ambient repository selectors must not override the configured project.
    // Workflow commands still inherit their ordinary environment.
    let removed = std::env::vars_os()
        .filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_"))
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    process::run_without(
        paths,
        &CommandSpec {
            program: PathBuf::from("/usr/bin/git"),
            args: arguments,
            cwd: repo.clone(),
            env: environment,
            timeout_seconds: Some(120),
            stdin,
            context: None,
        },
        &removed,
    )
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|value| (*value).to_owned()).collect()
}

pub(crate) fn value(paths: &Paths, repo: &Path, args: &[&str]) -> Result<String> {
    let result = run(paths, repo, &strings(args), None, BTreeMap::new())?;
    ensure!(result.success(), "Git failed: {}", result.stderr.trim());
    Ok(result.stdout.trim().to_owned())
}

pub(crate) fn commit(paths: &Paths, repo: &Path, revision: &str) -> Result<CommitId> {
    let result = value(
        paths,
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
    )?;
    ensure!(
        (result.len() == 40 || result.len() == 64)
            && result
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "Git returned an invalid commit identity"
    );
    CommitId::new(result)
}

pub(crate) fn common(paths: &Paths, repo: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(value(
        paths,
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)
    .canonicalize()?)
}

pub(crate) fn repository(paths: &Paths, repo: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(value(paths, repo, &["rev-parse", "--show-toplevel"])?).canonicalize()?)
}

fn local_branch(paths: &Paths, repo: &Path, branch: &str) -> Result<String> {
    ensure!(
        value(paths, repo, &["check-ref-format", "--branch", branch])? == branch,
        "expected a literal local branch name: {branch}"
    );
    Ok(format!("refs/heads/{branch}"))
}

pub(crate) fn branch_tip(paths: &Paths, repo: &Path, branch: &str) -> Result<CommitId> {
    let reference = local_branch(paths, repo, branch)?;
    commit(paths, repo, &reference).with_context(|| format!("cannot resolve local branch {branch}"))
}

fn worktrees(paths: &Paths, repo: &Path) -> Result<String> {
    value(paths, repo, &["worktree", "list", "--porcelain", "-z"])
}

fn branch_checkouts(inventory: &str, reference: &str) -> Vec<PathBuf> {
    inventory
        .split("\0\0")
        .filter_map(|entry| {
            let mut fields = entry.split('\0');
            let path = fields.next()?.strip_prefix("worktree ")?;
            fields
                .any(|field| field.strip_prefix("branch ") == Some(reference))
                .then(|| PathBuf::from(path))
        })
        .collect()
}

fn checkout_git_dir(paths: &Paths, checkout: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(value(
        paths,
        checkout,
        &["rev-parse", "--absolute-git-dir"],
    )?))
}

fn checkout_idle(paths: &Paths, checkout: &Path) -> Result<()> {
    let git_dir = checkout_git_dir(paths, checkout)?;
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_START",
    ] {
        ensure!(
            !git_dir.join(marker).try_exists()?,
            "Git operation in progress in {}",
            checkout.display()
        );
    }
    Ok(())
}

fn checkout_clean(paths: &Paths, checkout: &Path, include_ignored: bool) -> Result<()> {
    let mut args = vec![
        "status",
        "--porcelain=v1",
        "--untracked-files=normal",
        "--ignore-submodules=none",
    ];
    if include_ignored {
        args.push("--ignored=matching");
    }
    ensure!(
        value(paths, checkout, &args)?.is_empty(),
        "worktree has local files or changes: {}",
        checkout.display()
    );
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
    let reference = local_branch(paths, repo, branch)?;
    let observed = commit(paths, repo, &reference)?;
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
    let inventory = worktrees(paths, repo)?;
    match branch_checkouts(&inventory, &reference).as_slice() {
        [] => {
            // A rebase or bisect temporarily detaches HEAD while retaining
            // ownership of the branch it will restore.
            ensure_detached_branch_idle(paths, &inventory, &reference, branch)?;
            value(
                paths,
                repo,
                &[
                    "update-ref",
                    "--no-deref",
                    "-m",
                    "Telete: integrate prepared release",
                    &reference,
                    &candidate.0,
                    &expected.0,
                ],
            )?;
        }
        [checkout] => {
            ensure!(
                common(paths, checkout)? == common(paths, repo)?,
                "destination checkout belongs to another repository"
            );
            checkout_idle(paths, checkout)?;
            checkout_clean(paths, checkout, false)?;
            ensure!(
                value(paths, checkout, &["symbolic-ref", "HEAD"])? == reference
                    && commit(paths, checkout, "HEAD")? == *expected,
                "destination checkout changed before integration"
            );
            value(
                paths,
                checkout,
                &[
                    "merge",
                    "--ff-only",
                    "--no-edit",
                    "--no-autostash",
                    "--no-overwrite-ignore",
                    &candidate.0,
                ],
            )?;
        }
        _ => bail!("local branch {branch} is checked out more than once"),
    }
    ensure!(
        is_ancestor(paths, repo, candidate, &commit(paths, repo, &reference)?)?,
        "local branch {branch} does not contain integrated candidate {candidate}"
    );
    Ok(())
}

fn ensure_detached_branch_idle(
    paths: &Paths,
    inventory: &str,
    reference: &str,
    branch: &str,
) -> Result<()> {
    for entry in inventory.split("\0\0") {
        let mut fields = entry.split('\0');
        let Some(path) = fields
            .next()
            .and_then(|field| field.strip_prefix("worktree "))
        else {
            continue;
        };
        let checkout = Path::new(path);
        if !fields.any(|field| field == "detached") || !checkout.exists() {
            continue;
        }
        let git_dir = checkout_git_dir(paths, checkout)?;
        for (marker, branch_name) in [
            ("rebase-merge/head-name", reference),
            ("rebase-apply/head-name", reference),
            ("BISECT_START", branch),
        ] {
            match fs::read_to_string(git_dir.join(marker)) {
                Ok(owner) => ensure!(
                    owner.trim() != branch_name,
                    "Git operation in progress on local branch {branch} in {path}"
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
    private_worktree(paths, checkout)?;
    ensure!(
        common(paths, checkout)? == common(paths, repo)?,
        "candidate belongs to another repository"
    );
    ensure_detached(paths, checkout)?;
    let result = run(
        paths,
        checkout,
        &strings(&[
            "merge",
            "--ff",
            "--no-edit",
            "--no-autostash",
            "--no-overwrite-ignore",
            &source.0,
        ]),
        None,
        telete_author(),
    )?;
    ensure!(
        result.success(),
        "could not merge checked source {source}: {} {}",
        result.stdout.trim(),
        result.stderr.trim()
    );
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

fn worktree_owner_path(paths: &Paths, checkout: &Path) -> Result<Option<PathBuf>> {
    let registration = checkout_git_dir(paths, checkout)?;
    let linked = common(paths, checkout)?.join("worktrees");
    Ok((registration.parent() == Some(linked.as_path()))
        .then(|| registration.join("telete").join("worktree.json")))
}

pub(crate) fn record_development_worktree(
    paths: &Paths,
    checkout: &Path,
    branch: &str,
) -> Result<()> {
    let marker =
        worktree_owner_path(paths, checkout)?.context("worktree is not a linked checkout")?;
    crate::paths::atomic_json(
        &marker,
        &WorktreeOwner {
            id: uuid::Uuid::now_v7().to_string(),
            branch: branch.to_owned(),
        },
    )
}

fn worktree_owner(paths: &Paths, checkout: &Path) -> Result<Option<WorktreeOwner>> {
    let Some(marker) = worktree_owner_path(paths, checkout)? else {
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
    let Some(owner) = worktree_owner(paths, &path)? else {
        return Ok(None);
    };
    let reference = format!("refs/heads/{}", owner.branch);
    if value(paths, &path, &["rev-parse", "--symbolic-full-name", "HEAD"])? != reference
        || self::commit(paths, &path, "HEAD")? != *commit
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
    let reference = local_branch(paths, repo, &worktree.owner.branch)?;
    ensure!(
        !paths.root.starts_with(&worktree.path),
        "development worktree contains the active queue: {}",
        worktree.path.display()
    );
    let branch_exists = run(
        paths,
        repo,
        &strings(&["show-ref", "--verify", "--quiet", &reference]),
        None,
        BTreeMap::new(),
    )?;
    if branch_exists.exited_with(&[1]) && !worktree.path.try_exists()? {
        return Ok(());
    }
    ensure!(
        branch_exists.success(),
        "development branch disappeared: {reference}"
    );
    let observed = commit(paths, repo, &reference)?;
    ensure!(
        observed == worktree.submitted || observed == *checked,
        "development branch has advanced: {reference}"
    );
    ensure!(
        is_ancestor(paths, repo, checked, integrated)?
            && is_ancestor(paths, repo, &observed, integrated)?,
        "integrated candidate does not include development branch {reference}"
    );

    let inventory = worktrees(paths, repo)?;
    let checkouts = branch_checkouts(&inventory, &reference);
    if worktree.path.try_exists()? {
        ensure!(
            checkouts.as_slice() == [worktree.path.as_path()]
                && common(paths, &worktree.path)? == common(paths, repo)?
                && worktree_owner(paths, &worktree.path)?.as_ref() == Some(&worktree.owner),
            "development checkout changed: {}",
            worktree.path.display()
        );
        checkout_idle(paths, &worktree.path)?;
        checkout_clean(paths, &worktree.path, true)?;
        ensure!(
            value(paths, &worktree.path, &["symbolic-ref", "HEAD"])? == reference
                && commit(paths, &worktree.path, "HEAD")? == observed,
            "development checkout advanced before cleanup"
        );
        value(
            paths,
            repo,
            &[
                "worktree",
                "remove",
                "--",
                &worktree.path.display().to_string(),
            ],
        )?;
    } else {
        ensure!(
            checkouts.is_empty(),
            "development branch is checked out at another path"
        );
    }
    ensure!(
        branch_checkouts(&worktrees(paths, repo)?, &reference).is_empty(),
        "development branch is checked out"
    );
    value(
        paths,
        repo,
        &["update-ref", "--no-deref", "-d", &reference, &observed.0],
    )?;
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

pub(crate) fn ref_value(paths: &Paths, repo: &Path, reference: &str) -> Result<Option<CommitId>> {
    ensure!(
        managed_ref(reference),
        "Telete cannot read another manager's refs"
    );
    let result = run(
        paths,
        repo,
        &strings(&["rev-parse", "--verify", reference]),
        None,
        BTreeMap::new(),
    )?;
    if result.success() {
        Ok(Some(CommitId::new(result.stdout.trim())?))
    } else {
        ensure!(result.exited_with(&[128, 1]), "Git ref observation failed");
        Ok(None)
    }
}

pub(crate) fn advance(
    paths: &Paths,
    repo: &Path,
    reference: &str,
    old: Option<&CommitId>,
    new: &CommitId,
) -> Result<()> {
    ensure!(
        managed_ref(reference),
        "Telete cannot mutate another manager's refs"
    );
    let observed = ref_value(paths, repo, reference)?;
    if observed.as_ref() == Some(new) {
        return Ok(());
    }
    ensure!(
        observed.as_ref() == old,
        "private Git ref changed before promotion: {reference}"
    );
    let missing = "0".repeat(new.0.len());
    value(
        paths,
        repo,
        &[
            "update-ref",
            reference,
            &new.0,
            old.map_or(missing.as_str(), |value| value.0.as_str()),
        ],
    )?;
    Ok(())
}

pub(crate) fn forget(paths: &Paths, repo: &Path, job: &str, commit: &CommitId) -> Result<()> {
    let reference = private_ref(job)?;
    if let Some(observed) = ref_value(paths, repo, &reference)? {
        ensure!(
            observed == *commit,
            "private source ref changed before cleanup"
        );
        value(paths, repo, &["update-ref", "-d", &reference, &commit.0])?;
    }
    Ok(())
}

fn private_worktree(paths: &Paths, path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.starts_with(paths.root.join("jobs"))
            && path.file_name().is_some_and(|name| name == "worktree"),
        "worktree is outside Telete's jobs"
    );
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "symbolic candidate worktree"
        );
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
    if !path.exists() {
        value(
            paths,
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                &path.display().to_string(),
                &revision.0,
            ],
        )?;
    }
    ensure!(
        common(paths, path)? == common(paths, repo)?,
        "candidate belongs to another repository"
    );
    value(paths, path, &["reset", "--hard", &revision.0])?;
    value(paths, path, &["clean", "-ffd"])?;
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
    let inventory = value(paths, repo, &["worktree", "list", "--porcelain", "-z"])?;
    let trees = inventory
        .split('\0')
        .filter_map(|record| record.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    ensure!(
        trees.first().is_none_or(|path| path != worktree),
        "cannot remove primary worktree"
    );
    let registration = if let Some(path) = retained {
        validate_registration(paths, repo, worktree, path)?;
        Some(path.to_path_buf())
    } else {
        registration(paths, repo, worktree)?
    };
    if worktree.exists() {
        fs::remove_dir_all(worktree)?;
    }
    if trees.iter().any(|path| path == worktree) {
        value(
            paths,
            repo,
            &[
                "worktree",
                "remove",
                "--force",
                "--force",
                &worktree.display().to_string(),
            ],
        )?;
    }
    if let Some(path) = registration
        && path.exists()
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
    ensure!(
        common(paths, checkout)? == common(paths, repo)?,
        "fix checkout belongs to another repository"
    );
    let symbolic = run(
        paths,
        checkout,
        &strings(&["symbolic-ref", "-q", "HEAD"]),
        None,
        BTreeMap::new(),
    )?;
    ensure!(
        symbolic.exited_with(&[1]),
        "fix checkout must keep a detached HEAD"
    );
    let reference = result_ref(&job.id.0)?;
    let head = commit(paths, checkout, "HEAD")?;
    if let Some(retained) = ref_value(paths, repo, &reference)? {
        ensure!(
            head == job.commit || head == retained,
            "fix command changed its checkout HEAD"
        );
        if retained != job.commit {
            let parent = value(
                paths,
                repo,
                &["rev-list", "--parents", "-n", "1", &retained.0],
            )?;
            ensure!(
                parent == format!("{} {}", retained, job.commit),
                "retained fix commit is not a child of submitted source"
            );
        }
        value(paths, checkout, &["reset", "--hard", &retained.0])?;
        return Ok(retained);
    }
    ensure!(head == job.commit, "fix command changed its checkout HEAD");
    let environment = BTreeMap::from([(
        "GIT_INDEX_FILE".into(),
        paths.job(&job.id.0).join("fix.index").display().to_string(),
    )]);
    for args in [
        vec!["read-tree", &job.commit.0],
        vec!["add", "-u", "--", "."],
    ] {
        let result = run(paths, checkout, &strings(&args), None, environment.clone())?;
        ensure!(
            result.success(),
            "could not stage automatic edits: {}",
            result.stderr.trim()
        );
    }
    let tree = run(
        paths,
        checkout,
        &strings(&["write-tree"]),
        None,
        environment.clone(),
    )?;
    ensure!(
        tree.success(),
        "could not record automatic source tree: {}",
        tree.stderr.trim()
    );
    let tree = tree.stdout.trim();
    let original_tree = value(
        paths,
        checkout,
        &["rev-parse", &format!("{}^{{tree}}", job.commit)],
    )?;
    let result = if tree == original_tree {
        job.commit.clone()
    } else {
        automatic_commit(paths, checkout, tree, job)?
    };
    advance(paths, repo, &reference, None, &result)?;
    value(paths, checkout, &["reset", "--hard", &result.0])?;
    Ok(result)
}

pub(crate) fn ensure_detached(paths: &Paths, checkout: &Path) -> Result<()> {
    let symbolic = run(
        paths,
        checkout,
        &strings(&["symbolic-ref", "-q", "HEAD"]),
        None,
        BTreeMap::new(),
    )?;
    ensure!(
        symbolic.exited_with(&[1]),
        "workflow checkout must keep a detached HEAD"
    );
    Ok(())
}

pub(crate) fn unchanged(paths: &Paths, checkout: &Path, revision: &CommitId) -> Result<()> {
    ensure!(
        commit(paths, checkout, "HEAD")? == *revision,
        "workflow changed its checked commit"
    );
    let diff = value(
        paths,
        checkout,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    ensure!(
        diff.is_empty(),
        "workflow changed tracked source after automatic edits"
    );
    Ok(())
}

pub(crate) fn merge_head(paths: &Paths, checkout: &Path) -> Result<Option<CommitId>> {
    let path = value(
        paths,
        checkout,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "MERGE_HEAD",
        ],
    )?;
    match fs::read_to_string(path) {
        Ok(head) => Ok(Some(CommitId::new(head.trim())?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn is_ancestor(
    paths: &Paths,
    repo: &Path,
    ancestor: &CommitId,
    revision: &CommitId,
) -> Result<bool> {
    let result = run(
        paths,
        repo,
        &strings(&["merge-base", "--is-ancestor", &ancestor.0, &revision.0]),
        None,
        BTreeMap::new(),
    )?;
    ensure!(result.exited_with(&[0, 1]), "Git ancestry check failed");
    Ok(result.success())
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
    let symbolic = run(
        paths,
        checkout,
        &strings(&["symbolic-ref", "-q", "HEAD"]),
        None,
        BTreeMap::new(),
    )?;
    ensure!(
        symbolic.exited_with(&[1]),
        "release checkout must keep a detached HEAD"
    );
    unchanged(paths, checkout, revision)?;
    for ancestor in ancestors {
        ensure!(
            is_ancestor(paths, repo, ancestor, revision)?,
            "release commit {revision} must include source {ancestor}"
        );
    }
    advance(paths, repo, &release_ref(job)?, previous, revision)
}

fn telete_author() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("GIT_AUTHOR_NAME".into(), "Telete".into()),
        ("GIT_COMMITTER_NAME".into(), "Telete".into()),
        ("GIT_AUTHOR_EMAIL".into(), "telete@localhost".into()),
        ("GIT_COMMITTER_EMAIL".into(), "telete@localhost".into()),
    ])
}

fn automatic_commit(
    paths: &Paths,
    checkout: &Path,
    tree: &str,
    job: &crate::store::Job,
) -> Result<CommitId> {
    let mut environment = telete_author();
    for name in ["GIT_AUTHOR_DATE", "GIT_COMMITTER_DATE"] {
        environment.insert(name.into(), format!("@{} +0000", job.created));
    }
    let generated = run(
        paths,
        checkout,
        &strings(&["commit-tree", tree, "-p", &job.commit.0]),
        Some(format!(
            "Apply automatic source updates\n\nTelete-Job: {}\n",
            job.id
        )),
        environment,
    )?;
    ensure!(
        generated.success(),
        "could not commit automatic edits: {}",
        generated.stderr.trim()
    );
    CommitId::new(generated.stdout.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_preserves_ignored_worktree_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let paths = Paths::discovery(root.join("queue"));
        let repo = root.join("repo");
        let checkout = root.join("topic");
        let repo_path = repo.to_str().context("invalid test repository path")?;
        let checkout_path = checkout.to_str().context("invalid test worktree path")?;
        value(&paths, &root, &["init", "-b", "main", repo_path])?;
        fs::write(repo.join(".gitignore"), "local-data\n")?;
        value(&paths, &repo, &["add", ".gitignore"])?;
        let result = run(
            &paths,
            &repo,
            &strings(&["commit", "-m", "Initial commit"]),
            None,
            telete_author(),
        )?;
        ensure!(result.success(), "initial commit failed: {}", result.stderr);
        let head = commit(&paths, &repo, "HEAD")?;
        value(
            &paths,
            &repo,
            &["worktree", "add", "-b", "topic", checkout_path],
        )?;
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
