//! Committed-source selection and temporary private worktrees.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

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

pub(crate) fn private_ref(job: &str) -> Result<String> {
    crate::model::JobId::new(job)?;
    Ok(format!("refs/telete/jobs/{job}"))
}

pub(crate) fn ref_value(paths: &Paths, repo: &Path, reference: &str) -> Result<Option<CommitId>> {
    ensure!(
        reference.starts_with("refs/telete/jobs/"),
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
        reference.starts_with("refs/telete/jobs/"),
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
