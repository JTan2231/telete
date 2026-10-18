use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Project {
    _temporary: TempDir,
    root: PathBuf,
    repo: PathBuf,
    state: PathBuf,
    commit: String,
}

impl Project {
    fn new() -> TestResult<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let repo = root.join("repo");
        let commit = repository(&repo, "project source")?;
        let state = root.join("state");
        fs::write(
            root.join("telete.json"),
            r#"{"repo":"repo","state":"state","check":{"program":"/usr/bin/true"}}"#,
        )?;
        Ok(Self {
            _temporary: temporary,
            root,
            repo,
            state,
            commit,
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_telete"));
        command
            .current_dir(&self.root)
            .args(["--json", "--config", "telete.json"])
            .args(args);
        command
    }
}

fn git(repo: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("/usr/bin/git");
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "user.name=Telete CLI Test",
            "-c",
            "user.email=telete-test@example.invalid",
            "-C",
        ])
        .arg(repo)
        .args(args);
    command
}

fn repository(path: &Path, content: &str) -> TestResult<String> {
    fs::create_dir(path)?;
    checked(git(path, &["init", "--quiet"]))?;
    fs::write(path.join("source.txt"), content)?;
    checked(git(path, &["add", "source.txt"]))?;
    checked(git(path, &["commit", "--quiet", "-m", "source"]))?;
    Ok(checked(git(path, &["rev-parse", "HEAD"]))?
        .trim()
        .to_owned())
}

fn spawn(mut command: Command) -> TestResult<Child> {
    Ok(command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?)
}

fn bounded_output(mut child: Child) -> TestResult<Output> {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return Err(format!(
                "CLI child timed out: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn successful(output: Output) -> TestResult<String> {
    if !output.status.success() {
        return Err(format!(
            "command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn checked(command: Command) -> TestResult<String> {
    successful(bounded_output(spawn(command)?)?)
}

fn job_id(value: &str) -> TestResult<&str> {
    value
        .split_once("\"id\":\"")
        .and_then(|(_, remainder)| remainder.split_once('"'))
        .map(|(id, _)| id)
        .ok_or_else(|| format!("submission has no job ID: {value}").into())
}

#[test]
fn cold_project_runs_head_and_keeps_only_success_marker() -> TestResult {
    let project = Project::new()?;
    let status = checked(project.command(&["status"]))?;
    assert!(status.contains("\"queue\":[]"));
    assert!(!project.state.exists());

    let submitted = checked(project.command(&["submit"]))?;
    let id = job_id(&submitted)?;
    assert!(submitted.contains(&format!("\"commit\":\"{}\"", project.commit)));
    assert!(submitted.contains("\"phase\":\"queued\""));

    let mut succeeded = false;
    for _ in 0..8 {
        let result = checked(project.command(&["worker", "--once"]))?;
        if result.contains("\"phase\":\"succeeded\"") {
            succeeded = true;
            break;
        }
    }
    assert!(succeeded, "workflow did not finish successfully");

    let result = checked(project.command(&["wait", id, "--timeout", "1"]))?;
    assert!(result.contains("\"observation\":\"terminal\""));
    assert!(result.contains("\"phase\":\"succeeded\""));
    assert!(!result.contains("\"commit\""));
    assert!(!result.contains("\"command\""));
    assert!(!project.state.join("jobs").join(id).exists());
    assert_eq!(fs::read_dir(project.state.join("jobs"))?.count(), 0);
    assert!(
        checked(git(
            &project.repo,
            &["for-each-ref", "--format=%(refname)", "refs/telete/jobs/"],
        ))?
        .trim()
        .is_empty()
    );
    assert_eq!(
        checked(git(&project.repo, &["worktree", "list", "--porcelain"]))?
            .matches("worktree ")
            .count(),
        1
    );
    assert_eq!(
        checked(git(&project.repo, &["rev-parse", "HEAD"]))?.trim(),
        project.commit
    );
    Ok(())
}

#[test]
fn configured_repository_wins_over_ambient_git_selectors() -> TestResult {
    let project = Project::new()?;
    let foreign = project.root.join("foreign");
    let foreign_commit = repository(&foreign, "foreign source")?;
    assert_ne!(project.commit, foreign_commit);
    let mut command = project.command(&["submit"]);
    command
        .env("GIT_DIR", foreign.join(".git"))
        .env("GIT_WORK_TREE", &foreign)
        .env("GIT_INDEX_FILE", foreign.join(".git/index"));
    let submitted = checked(command)?;
    assert!(submitted.contains(&format!("\"commit\":\"{}\"", project.commit)));
    assert!(!submitted.contains(&foreign_commit));
    assert!(
        checked(git(
            &foreign,
            &["for-each-ref", "--format=%(refname)", "refs/telete/jobs/"],
        ))?
        .trim()
        .is_empty()
    );
    Ok(())
}

#[test]
fn concurrent_first_submissions_publish_one_complete_queue() -> TestResult {
    let project = Project::new()?;
    let mut children = Vec::new();
    for _ in 0..6 {
        match spawn(project.command(&["submit"])) {
            Ok(child) => children.push(child),
            Err(error) => {
                for child in children {
                    let _ = bounded_output(child);
                }
                return Err(error);
            }
        }
    }
    // Reap every child before reporting a failed submission or dropping its
    // temporary repository.
    let outputs = children.into_iter().map(bounded_output).collect::<Vec<_>>();
    let mut ids = BTreeSet::new();
    for output in outputs {
        let submitted = successful(output?)?;
        assert!(submitted.contains("\"phase\":\"queued\""));
        assert!(ids.insert(job_id(&submitted)?.to_owned()));
    }
    assert_eq!(ids.len(), 6);
    let status = checked(project.command(&["status"]))?;
    assert_eq!(status.matches("\"phase\":\"queued\"").count(), 6);
    for id in ids {
        assert!(status.contains(&format!("\"id\":\"{id}\"")));
    }
    assert_eq!(
        checked(git(
            &project.repo,
            &["for-each-ref", "--format=%(refname)", "refs/telete/jobs/"],
        ))?
        .lines()
        .count(),
        6
    );
    Ok(())
}
