use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::{Value, json};
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

fn configured_command(directory: &Path, config: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_telete"));
    command
        .current_dir(directory)
        .args(["--json", "--config"])
        .arg(config)
        .args(args);
    command
}

fn checked_json(command: Command) -> TestResult<Value> {
    Ok(serde_json::from_str(&checked(command)?)?)
}

fn write_worktree_config(worktree: &Path) -> TestResult<PathBuf> {
    let config = worktree.join("telete.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "repo": ".", "state": ".telete", "check": {"program": "/usr/bin/true"}
        }))?,
    )?;
    Ok(config)
}

fn commit_source(worktree: &Path, source: &str) -> TestResult<String> {
    fs::write(worktree.join("source.txt"), source)?;
    checked(git(worktree, &["add", "source.txt"]))?;
    checked(git(worktree, &["commit", "--quiet", "-m", source]))?;
    Ok(checked(git(worktree, &["rev-parse", "HEAD"]))?
        .trim()
        .to_owned())
}

fn finish_jobs(directory: &Path, config: &Path, ids: &[String]) -> TestResult {
    for _ in 0..48 {
        checked(configured_command(directory, config, &["worker", "--once"]))?;
        let status = checked_json(configured_command(directory, config, &["status"]))?;
        let succeeded = ids.iter().all(|id| {
            status["completions"].as_array().is_some_and(|jobs| {
                jobs.iter()
                    .any(|job| job["id"] == *id && job["phase"] == "succeeded")
            })
        });
        if succeeded {
            return Ok(());
        }
        if !status["failure"].is_null() {
            return Err(format!("workflow failed: {}", status["failure"]).into());
        }
    }
    Err("shared queue did not finish successfully".into())
}

struct RunningWorker(Child);

impl Drop for RunningWorker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_worker(worker: &mut RunningWorker, state: &Path) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = worker.0.try_wait()? {
            return Err(format!("foreground worker exited early: {status}").into());
        }
        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(state.join("worker.lock"))
        {
            Ok(lock) => match lock.try_lock_exclusive() {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(()) => FileExt::unlock(&lock)?,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("foreground worker did not acquire its ownership lock".into())
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

#[test]
fn worktree_uses_configured_head_and_resolves_destination_from_invocation() -> TestResult {
    let project = Project::new()?;
    let invocation = project.root.join("invocation");
    let foreign_commit = repository(&invocation, "ambient repository")?;
    assert_ne!(foreign_commit, project.commit);
    fs::write(project.repo.join("source.txt"), "uncommitted caller change")?;
    let destination = invocation.join("feature-a");
    // Exercise flags after the subcommand, as in the documented invocation.
    let mut command = Command::new(env!("CARGO_BIN_EXE_telete"));
    command
        .current_dir(&invocation)
        .args(["worktree", "feature-a", "--json", "--config"])
        .arg(project.root.join("telete.json"));
    let result = checked_json(command)?;
    assert_eq!(result["path"], json!(destination));
    assert_eq!(result["branch"], "feature-a");
    assert_eq!(result["commit"], project.commit);
    assert_eq!(result["state"], json!(project.state));
    assert_eq!(
        checked(git(&destination, &["rev-parse", "HEAD"]))?.trim(),
        project.commit
    );
    assert_eq!(
        checked(git(&destination, &["branch", "--show-current"]))?.trim(),
        "feature-a"
    );
    assert_eq!(
        fs::read_to_string(destination.join("source.txt"))?,
        "project source"
    );
    assert_eq!(
        fs::read_to_string(project.repo.join("source.txt"))?,
        "uncommitted caller change"
    );
    assert_eq!(
        checked(git(&invocation, &["rev-parse", "HEAD"]))?.trim(),
        foreign_commit
    );

    let occupied = project.root.join("occupied");
    fs::create_dir(&occupied)?;
    fs::write(occupied.join("keep"), "user file")?;
    let output = bounded_output(spawn(project.command(&["worktree", "occupied"]))?)?;
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(occupied.join("keep"))?, "user file");
    checked(git(&project.repo, &["branch", "existing-branch"]))?;
    let output = bounded_output(spawn(project.command(&["worktree", "existing-branch"]))?)?;
    assert!(!output.status.success());
    assert!(!project.root.join("existing-branch").exists());
    assert_eq!(
        checked(git(&project.repo, &["rev-parse", "existing-branch"]))?.trim(),
        project.commit
    );
    Ok(())
}

#[test]
fn worktrees_share_order_controls_and_frozen_workflows() -> TestResult {
    let project = Project::new()?;
    let audit = project.root.join("audit");
    let mut configs = Vec::new();
    let mut submissions = Vec::new();
    let mut expected_lines = Vec::new();
    let mut contexts = Vec::new();
    for name in ["feature-a", "feature-b", "feature-c"] {
        checked(project.command(&["worktree", name]))?;
        let worktree = project.root.join(name);
        let commit = commit_source(&worktree, name)?;
        let config = write_worktree_config(&worktree)?;
        let mut workflow = json!({
            "repo": ".", "state": ".telete", "parameters": {"feature": name}
        });
        for stage in ["check", "prepare", "deploy"] {
            let output = project.root.join(format!("{name}-{stage}-context.json"));
            workflow[stage] = json!({
                "program": "/bin/sh",
                "args": ["-c", "set -eu; printf '%s:%s:%s\\n' \"$1\" \"$FROZEN_STAGE\" \"$(cat source.txt)\" >> \"$AUDIT\"; cp \"$TELETE_CONTEXT\" \"$CONTEXT_OUTPUT\"", "telete-test", name],
                "env": {"FROZEN_STAGE": stage, "AUDIT": audit, "CONTEXT_OUTPUT": output},
                "timeout_seconds": 10
            });
            expected_lines.push(format!("{name}:{stage}:{name}"));
            contexts.push((output, name, stage, commit.clone()));
        }
        fs::write(&config, serde_json::to_vec(&workflow)?)?;
        let submitted = checked_json(configured_command(&worktree, &config, &["submit"]))?;
        assert_eq!(submitted["commit"], commit);
        assert_eq!(submitted["state_root"], json!(project.state));
        submissions.push(submitted);
        // Neither later configuration nor checkout edits may replace an accepted job.
        fs::write(worktree.join("source.txt"), "dirty after submission")?;
        fs::write(
            &config,
            serde_json::to_vec(&json!({
                "repo": ".", "state": ".telete", "check": {"program": "/usr/bin/false"},
                "parameters": {"feature": "changed"}
            }))?,
        )?;
        configs.push((worktree, config));
    }
    let ids = submissions
        .iter()
        .map(|value| {
            value["id"]
                .as_str()
                .map(str::to_owned)
                .ok_or("missing job ID")
        })
        .collect::<Result<Vec<_>, _>>()?;
    for ((worktree, config), expected) in configs.iter().zip([1, 2, 3]) {
        let status = checked_json(configured_command(worktree, config, &["status"]))?;
        assert_eq!(status["state_root"], json!(project.state));
        assert_eq!(status["queue"].as_array().ok_or("missing queue")?.len(), 3);
        assert_eq!(submissions[expected - 1]["sequence"], expected);
        for (queued, id) in status["queue"]
            .as_array()
            .ok_or("missing queue")?
            .iter()
            .zip(&ids)
        {
            assert_eq!(queued["id"], *id);
        }
        assert!(!worktree.join(".telete").exists());
    }

    let member = |index: usize, args: &[&str]| {
        configured_command(&configs[index].0, &configs[index].1, args)
    };
    checked(member(0, &["pause"]))?;
    let paused = checked_json(member(1, &["status"]))?;
    assert_eq!(paused["paused"], true);
    checked(member(2, &["worker", "--once"]))?;
    assert!(!audit.exists());
    checked(member(1, &["maintenance", "hold", "--owner", "test"]))?;
    checked(member(2, &["resume"]))?;
    let held = checked_json(project.command(&["maintenance", "status"]))?;
    assert_eq!(held["held"], true);
    assert_eq!(held["owners"], json!(["test"]));
    checked(project.command(&["worker", "--once"]))?;
    assert!(!audit.exists());
    checked(project.command(&["maintenance", "release", "--owner", "test"]))?;
    finish_jobs(&configs[2].0, &configs[2].1, &ids)?;
    assert_eq!(
        fs::read_to_string(audit)?.lines().collect::<Vec<_>>(),
        expected_lines
    );
    for (file, name, stage, commit) in contexts {
        let context: Value = serde_json::from_slice(&fs::read(file)?)?;
        assert_eq!(context["parameters"], json!({"feature": name}));
        assert_eq!(context["stage"], stage);
        assert_eq!(context["source"]["commit"], commit);
    }
    for (worktree, _) in configs {
        assert!(!worktree.join(".telete").exists());
    }
    Ok(())
}

#[test]
fn nested_and_manual_worktrees_share_accepted_source_after_submitter_removal() -> TestResult {
    let project = Project::new()?;
    checked(project.command(&["worktree", "child"]))?;
    let child = project.root.join("child");
    let child_config = write_worktree_config(&child)?;
    let selected = commit_source(&child, "child committed source")?;
    let nested = checked_json(configured_command(
        &child,
        &child_config,
        &["worktree", "../nested"],
    ))?;
    assert_eq!(nested["commit"], selected);
    assert_eq!(nested["state"], json!(project.state));
    let nested_path = project.root.join("nested");
    let nested_config = write_worktree_config(&nested_path)?;
    let manual = project.root.join("manual");
    let mut add_manual = git(&project.repo, &["worktree", "add", "-b", "manual"]);
    add_manual.arg(&manual).arg(&project.commit);
    checked(add_manual)?;
    let manual_config = write_worktree_config(&manual)?;
    let original = checked_json(configured_command(
        &child,
        &project.root.join("telete.json"),
        &["submit"],
    ))?;
    assert_eq!(original["commit"], project.commit);
    let original_id = original["id"].as_str().ok_or("missing job ID")?.to_owned();
    let submitted = checked_json(configured_command(&child, &child_config, &["submit"]))?;
    let id = submitted["id"].as_str().ok_or("missing job ID")?.to_owned();
    assert_eq!(submitted["commit"], selected);
    let mut remove = git(&project.repo, &["worktree", "remove", "--force"]);
    remove.arg(&child);
    checked(remove)?;
    checked(git(&project.repo, &["branch", "-D", "child"]))?;
    assert!(!child.exists());
    for (directory, config) in [(&manual, &manual_config), (&nested_path, &nested_config)] {
        let status = checked_json(configured_command(directory, config, &["status", &id]))?;
        assert_eq!(status["state_root"], json!(project.state));
        assert_eq!(status["commit"], selected);
        assert!(!directory.join(".telete").exists());
    }
    finish_jobs(&manual, &manual_config, &[original_id, id])?;
    assert_eq!(
        checked(git(&nested_path, &["rev-parse", "HEAD"]))?.trim(),
        selected
    );
    Ok(())
}

#[test]
fn sharing_rejects_a_discovered_independent_populated_runtime() -> TestResult {
    let project = Project::new()?;
    let manual = project.root.join("independent");
    let mut add_manual = git(&project.repo, &["worktree", "add", "-b", "independent"]);
    add_manual.arg(&manual).arg(&project.commit);
    checked(add_manual)?;
    let config = write_worktree_config(&manual)?;
    let independent = checked_json(configured_command(&manual, &config, &["submit"]))?;
    let independent_id = independent["id"].as_str().ok_or("missing job ID")?;
    assert!(manual.join(".telete").exists());
    checked(project.command(&["worktree", "bound-child"]))?;
    let output = bounded_output(spawn(configured_command(&manual, &config, &["submit"]))?)?;
    assert!(
        !output.status.success(),
        "populated independent queue was silently abandoned"
    );
    assert!(manual.join(".telete/jobs").join(independent_id).exists());
    let shared = checked_json(project.command(&["status"]))?;
    assert!(
        shared["queue"]
            .as_array()
            .ok_or("missing queue")?
            .is_empty()
    );
    Ok(())
}

#[test]
fn shared_binding_failures_never_fall_back_to_a_local_runtime() -> TestResult {
    let project = Project::new()?;
    checked(project.command(&["worktree", "child"]))?;
    let child = project.root.join("child");
    let config = write_worktree_config(&child)?;
    let binding = project.repo.join(".git/telete/queue.json");
    let saved_binding = fs::read(&binding)?;
    let saved_runtime = project.root.join("saved-runtime");
    fs::rename(&project.state, &saved_runtime)?;
    for operation in ["status", "submit"] {
        let output = bounded_output(spawn(configured_command(&child, &config, &[operation]))?)?;
        assert!(
            !output.status.success(),
            "missing shared runtime was accepted"
        );
        assert!(!project.state.exists());
        assert!(!child.join(".telete").exists());
        assert_eq!(fs::read(&binding)?, saved_binding);
    }
    fs::rename(&saved_runtime, &project.state)?;

    let valid: Value = serde_json::from_slice(&saved_binding)?;
    let mut wrong_version = valid.clone();
    wrong_version["version"] = json!(99);
    let mut wrong_repository = valid.clone();
    wrong_repository["common_git_dir"] = json!(project.root.join("foreign.git"));
    let mut relative_state = valid;
    relative_state["state"] = json!("relative-state");
    for contents in [
        b"invalid JSON".to_vec(),
        serde_json::to_vec(&wrong_version)?,
        serde_json::to_vec(&wrong_repository)?,
        serde_json::to_vec(&relative_state)?,
    ] {
        fs::write(&binding, &contents)?;
        for operation in ["status", "submit"] {
            let output = bounded_output(spawn(configured_command(&child, &config, &[operation]))?)?;
            assert!(
                !output.status.success(),
                "invalid shared binding was accepted"
            );
            assert!(!child.join(".telete").exists());
            assert_eq!(fs::read(&binding)?, contents);
        }
        let output = bounded_output(spawn(project.command(&["worktree", "blocked-child"]))?)?;
        assert!(!output.status.success());
        assert!(!project.root.join("blocked-child").exists());
    }
    fs::write(&binding, saved_binding)?;
    let status = checked_json(configured_command(&child, &config, &["status"]))?;
    assert_eq!(status["state_root"], json!(project.state));
    assert!(
        status["queue"]
            .as_array()
            .ok_or("missing queue")?
            .is_empty()
    );
    Ok(())
}

#[test]
fn first_binding_requires_unbound_worker_to_stop_but_bound_worker_allows_worktrees() -> TestResult {
    let project = Project::new()?;
    let mut unbound = RunningWorker(spawn(project.command(&["worker"]))?);
    wait_for_worker(&mut unbound, &project.state)?;
    let output = bounded_output(spawn(project.command(&["worktree", "blocked"]))?)?;
    assert!(!output.status.success());
    assert!(unbound.0.try_wait()?.is_none());
    assert!(!project.repo.join(".git/telete/queue.json").exists());
    assert!(!project.root.join("blocked").exists());
    assert!(
        checked(git(
            &project.repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads/blocked"]
        ))?
        .trim()
        .is_empty()
    );
    drop(unbound);

    checked(project.command(&["worktree", "child"]))?;
    let child = project.root.join("child");
    let config = write_worktree_config(&child)?;
    let mut bound = RunningWorker(spawn(project.command(&["worker"]))?);
    wait_for_worker(&mut bound, &project.state)?;
    let result = checked_json(configured_command(
        &child,
        &config,
        &["worktree", "../sibling"],
    ))?;
    assert_eq!(result["state"], json!(project.state));
    assert!(project.root.join("sibling").is_dir());
    assert!(bound.0.try_wait()?.is_none());
    let output = bounded_output(spawn(configured_command(
        &child,
        &config,
        &["worker", "--once"],
    ))?)?;
    assert!(
        !output.status.success(),
        "second worktree acquired worker ownership"
    );
    assert!(!child.join(".telete").exists());
    Ok(())
}

#[test]
fn git_creation_failure_preserves_binding_and_existing_git_lock() -> TestResult {
    let project = Project::new()?;
    let git_lock = project.repo.join(".git/refs/heads/blocked.lock");
    fs::write(&git_lock, "another Git operation owns this lock")?;
    let output = bounded_output(spawn(project.command(&["worktree", "blocked"]))?)?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("shared queue remains"));
    assert_eq!(
        fs::read_to_string(&git_lock)?,
        "another Git operation owns this lock"
    );
    assert!(!project.root.join("blocked").exists());
    let binding = project.repo.join(".git/telete/queue.json");
    let saved_binding = fs::read(&binding)?;
    let status = checked_json(project.command(&["status"]))?;
    assert_eq!(status["state_root"], json!(project.state));
    assert!(
        status["queue"]
            .as_array()
            .ok_or("missing queue")?
            .is_empty()
    );
    fs::remove_file(git_lock)?;
    let result = checked_json(project.command(&["worktree", "blocked"]))?;
    assert_eq!(result["state"], json!(project.state));
    assert_eq!(fs::read(binding)?, saved_binding);
    Ok(())
}
