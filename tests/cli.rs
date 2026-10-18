use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::os::unix::process::CommandExt;
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

impl Drop for Project {
    fn drop(&mut self) {
        // Assertions must not leave detached workers behind after their
        // temporary repository disappears.
        let Ok(status) = checked_json(self.command(&["status"])) else {
            return;
        };
        let mut ids = status["queue"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|job| job["id"].as_str())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if let Some(id) = status["active"]["id"].as_str() {
            ids.push(id.to_owned());
        }
        for id in ids {
            let _ = checked(self.command(&["cancel", &id]));
        }
        for owner in status["maintenance_owners"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let _ = checked(self.command(&["maintenance", "release", "--owner", owner]));
        }
        if status["paused"] == true {
            let _ = checked(self.command(&["resume"]));
        }
        if let Some(state) = status["state_root"].as_str() {
            let _ = wait_for_worker_ownership(Path::new(state), false);
        }
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
    for id in ids {
        let result = checked_json(configured_command(directory, config, &["wait", id]))?;
        assert_eq!(result["observation"], "terminal");
        assert_eq!(result["job"]["phase"], "succeeded");
    }
    Ok(())
}

fn wait_for_worker_ownership(state: &Path, expected: bool) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let owned = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(state.join("supervisor.lock"))
        {
            Ok(lock) => match lock.try_lock_exclusive() {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
                Err(error) => return Err(error.into()),
                Ok(()) => {
                    FileExt::unlock(&lock)?;
                    false
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if owned == expected {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(format!("worker ownership did not become {expected}").into())
}

fn wait_for_file(path: &Path) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(format!("file was not created: {}", path.display()).into())
}

fn wait_for_worker_pid(state: &Path, previous: Option<i32>) -> TestResult<i32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(bytes) = fs::read(state.join("worker.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(pid) = value["worker_pid"].as_i64()
        {
            let pid = i32::try_from(pid)?;
            if Some(pid) != previous {
                return Ok(pid);
            }
        }
        if Instant::now() >= deadline {
            return Err("supervisor did not identify a new running worker".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn gated_workflow(project: &Project) -> TestResult {
    fs::write(
        project.root.join("telete.json"),
        serde_json::to_vec(&json!({
            "repo": "repo", "state": "state", "check": {
                "program": "/bin/sh",
                "args": ["-c", "set -eu; printf 'started\\n' >> \"$STARTED\"; while [ ! -e \"$RELEASE\" ]; do sleep 0.05; done; printf finished > \"$FINISHED\""],
                "env": {
                    "STARTED": project.root.join("started"),
                    "RELEASE": project.root.join("release"),
                    "FINISHED": project.root.join("finished")
                },
                "timeout_seconds": 15
            }
        }))?,
    )?;
    Ok(())
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

    let result = checked(project.command(&["wait", id]))?;
    assert!(result.contains("\"observation\":\"terminal\""));
    assert!(result.contains("\"phase\":\"succeeded\""));
    wait_for_worker_ownership(&project.state, false)?;
    let result = checked(project.command(&["status", id]))?;
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
    checked(project.command(&["wait", job_id(&submitted)?]))?;
    wait_for_worker_ownership(&project.state, false)?;
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
    finish_jobs(
        &project.root,
        &project.root.join("telete.json"),
        &ids.into_iter().collect::<Vec<_>>(),
    )?;
    wait_for_worker_ownership(&project.state, false)?;
    assert!(
        checked(git(
            &project.repo,
            &["for-each-ref", "--format=%(refname)", "refs/telete/jobs/"],
        ))?
        .trim()
        .is_empty()
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
    assert!(result.get("state").is_none());
    assert!(result.get("state_root").is_none());
    assert!(!project.state.exists());
    assert!(!project.repo.join(".git/telete").exists());
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
    checked(project.command(&["pause"]))?;
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
    let paused = checked_json(member(1, &["status"]))?;
    assert_eq!(paused["paused"], true);
    wait_for_worker_ownership(&project.state, true)?;
    assert!(!audit.exists());
    checked(member(1, &["maintenance", "hold", "--owner", "test"]))?;
    checked(member(2, &["resume"]))?;
    let held = checked_json(project.command(&["maintenance", "status"]))?;
    assert_eq!(held["held"], true);
    assert_eq!(held["owners"], json!(["test"]));
    std::thread::sleep(Duration::from_millis(150));
    assert!(!audit.exists());
    checked(project.command(&["maintenance", "release", "--owner", "test"]))?;
    finish_jobs(&configs[2].0, &configs[2].1, &ids)?;
    wait_for_worker_ownership(&project.state, false)?;
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
    checked(project.command(&["pause"]))?;
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
    assert!(nested.get("state_root").is_none());
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
    checked(configured_command(&manual, &manual_config, &["resume"]))?;
    finish_jobs(&manual, &manual_config, &[original_id, id])?;
    wait_for_worker_ownership(&project.state, false)?;
    assert_eq!(
        checked(git(&nested_path, &["rev-parse", "HEAD"]))?.trim(),
        selected
    );
    Ok(())
}

#[test]
fn first_queue_mutation_binds_the_runtime_for_all_worktrees() -> TestResult {
    let project = Project::new()?;
    let manual = project.root.join("independent");
    let mut add_manual = git(&project.repo, &["worktree", "add", "-b", "independent"]);
    add_manual.arg(&manual).arg(&project.commit);
    checked(add_manual)?;
    let config = write_worktree_config(&manual)?;
    checked_json(configured_command(&manual, &config, &["status"]))?;
    assert!(!manual.join(".telete").exists());
    assert!(!project.repo.join(".git/telete").exists());
    checked(project.command(&["worktree", "bound-child"]))?;
    assert!(!project.state.exists());
    assert!(!project.repo.join(".git/telete").exists());
    checked(configured_command(&manual, &config, &["pause"]))?;
    let selected_state = manual.join(".telete");
    let binding: Value =
        serde_json::from_slice(&fs::read(project.repo.join(".git/telete/queue.json"))?)?;
    assert_eq!(binding["state"], json!(selected_state));
    let submitted = checked_json(project.command(&["submit"]))?;
    let id = submitted["id"].as_str().ok_or("missing job ID")?;
    assert_eq!(submitted["state_root"], json!(selected_state));
    let shared = checked_json(project.command(&["status"]))?;
    assert_eq!(shared["queue"][0]["id"], id);
    assert!(!project.state.exists());
    checked(project.command(&["resume"]))?;
    checked(configured_command(&manual, &config, &["wait", id]))?;
    wait_for_worker_ownership(&selected_state, false)?;
    Ok(())
}

#[test]
fn shared_binding_failures_never_fall_back_to_a_local_runtime() -> TestResult {
    let project = Project::new()?;
    checked(project.command(&["pause"]))?;
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
    for (index, contents) in [
        b"invalid JSON".to_vec(),
        serde_json::to_vec(&wrong_version)?,
        serde_json::to_vec(&wrong_repository)?,
        serde_json::to_vec(&relative_state)?,
    ]
    .into_iter()
    .enumerate()
    {
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
        let name = format!("unaffected-child-{index}");
        checked(project.command(&["worktree", &name]))?;
        assert!(project.root.join(name).is_dir());
        assert_eq!(fs::read(&binding)?, contents);
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
fn worktree_creation_ignores_running_workers_and_queue_locks() -> TestResult {
    let project = Project::new()?;
    checked(project.command(&["pause"]))?;
    let submitted = checked(project.command(&["submit"]))?;
    let id = job_id(&submitted)?;
    wait_for_worker_ownership(&project.state, true)?;
    let selection = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(project.repo.join(".git/telete/queue.lock"))?;
    selection.try_lock_exclusive()?;
    checked(project.command(&["worktree", "child"]))?;
    let child = project.root.join("child");
    let config = write_worktree_config(&child)?;
    let result = checked_json(configured_command(
        &child,
        &config,
        &["worktree", "../sibling"],
    ))?;
    assert!(result.get("state_root").is_none());
    assert!(project.root.join("sibling").is_dir());
    assert!(!child.join(".telete").exists());
    FileExt::unlock(&selection)?;
    checked(project.command(&["cancel", id]))?;
    wait_for_worker_ownership(&project.state, false)?;
    Ok(())
}

#[test]
fn git_creation_failure_preserves_existing_git_lock_without_queue_state() -> TestResult {
    let project = Project::new()?;
    let git_lock = project.repo.join(".git/refs/heads/blocked.lock");
    fs::write(&git_lock, "another Git operation owns this lock")?;
    let output = bounded_output(spawn(project.command(&["worktree", "blocked"]))?)?;
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(&git_lock)?,
        "another Git operation owns this lock"
    );
    assert!(!project.root.join("blocked").exists());
    let binding = project.repo.join(".git/telete/queue.json");
    assert!(!binding.exists());
    assert!(!project.state.exists());
    let status = checked_json(project.command(&["status"]))?;
    assert_eq!(status["state_root"], json!(project.state));
    assert!(
        status["queue"]
            .as_array()
            .ok_or("missing queue")?
            .is_empty()
    );
    fs::remove_file(git_lock)?;
    checked_json(project.command(&["worktree", "blocked"]))?;
    assert!(!binding.exists());
    assert!(!project.state.exists());
    Ok(())
}

#[test]
fn default_submission_returns_before_background_work_finishes() -> TestResult {
    let project = Project::new()?;
    gated_workflow(&project)?;
    let receipt = checked(project.command(&["submit"]))?;
    let id = job_id(&receipt)?;
    wait_for_file(&project.root.join("started"))?;
    assert!(!project.root.join("finished").exists());
    fs::write(project.root.join("release"), "continue")?;
    checked(project.command(&["wait", id]))?;
    assert!(project.root.join("finished").exists());
    wait_for_worker_ownership(&project.state, false)?;
    Ok(())
}

#[test]
fn follow_prints_the_submission_receipt_and_final_observation() -> TestResult {
    let project = Project::new()?;
    let output = checked(project.command(&["submit", "--follow"]))?;
    let values = output
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(values.len(), 2, "expected receipt and final observation");
    assert_eq!(values[0]["phase"], "queued");
    assert_eq!(values[0]["commit"], project.commit);
    assert_eq!(values[1]["observation"], "terminal");
    assert_eq!(values[1]["job"]["id"], values[0]["id"]);
    assert_eq!(values[1]["job"]["phase"], "succeeded");
    wait_for_worker_ownership(&project.state, false)?;
    Ok(())
}

#[test]
fn failed_and_cancelled_jobs_make_wait_unsuccessful() -> TestResult {
    let project = Project::new()?;
    fs::write(
        project.root.join("telete.json"),
        r#"{"repo":"repo","state":"state","check":{"program":"/usr/bin/false"}}"#,
    )?;
    let receipt = checked(project.command(&["submit"]))?;
    let failed = bounded_output(spawn(project.command(&["wait", job_id(&receipt)?]))?)?;
    assert!(!failed.status.success());
    let observation: Value = serde_json::from_slice(&failed.stdout)?;
    assert_eq!(observation["observation"], "terminal");
    assert_eq!(observation["job"]["phase"], "failed");
    wait_for_worker_ownership(&project.state, false)?;

    checked(project.command(&["pause"]))?;
    let receipt = checked(project.command(&["submit"]))?;
    let id = job_id(&receipt)?;
    checked(project.command(&["cancel", id]))?;
    let cancelled = bounded_output(spawn(project.command(&["wait", id]))?)?;
    assert!(!cancelled.status.success());
    let observation: Value = serde_json::from_slice(&cancelled.stdout)?;
    assert_eq!(observation["job"]["phase"], "cancelled");
    wait_for_worker_ownership(&project.state, false)?;
    Ok(())
}

#[test]
fn optional_wait_timeout_leaves_the_job_queued() -> TestResult {
    let project = Project::new()?;
    checked(project.command(&["pause"]))?;
    let receipt = checked(project.command(&["submit"]))?;
    let id = job_id(&receipt)?;
    let observed = bounded_output(spawn(project.command(&["wait", id, "--timeout", "1"]))?)?;
    assert!(!observed.status.success());
    let observation: Value = serde_json::from_slice(&observed.stdout)?;
    assert_eq!(observation["observation"], "timeout");
    let status = checked_json(project.command(&["status", id]))?;
    assert_eq!(status["phase"], "queued");
    assert_eq!(status["cancel_requested"], false);
    checked(project.command(&["resume"]))?;
    checked(project.command(&["wait", id]))?;
    wait_for_worker_ownership(&project.state, false)?;
    Ok(())
}

#[test]
fn interrupting_follow_or_wait_does_not_signal_or_cancel_the_job() -> TestResult {
    for follow in [true, false] {
        let project = Project::new()?;
        gated_workflow(&project)?;
        let receipt = if follow {
            None
        } else {
            Some(checked(project.command(&["submit"]))?)
        };
        let mut command = if let Some(receipt) = receipt.as_deref() {
            project.command(&["wait", job_id(receipt)?])
        } else {
            project.command(&["submit", "--follow"])
        };
        // A terminal sends SIGINT to the whole foreground process group.
        command.process_group(0);
        let child = spawn(command)?;
        wait_for_file(&project.root.join("started"))?;
        std::thread::sleep(Duration::from_millis(150));
        let foreground = rustix::process::Pid::from_child(&child);
        rustix::process::kill_process_group(foreground, rustix::process::Signal::INT)?;
        let output = bounded_output(child)?;
        // Release before asserting, so a failed assertion still lets cleanup
        // settle the independent command promptly.
        fs::write(project.root.join("release"), "continue")?;
        assert_eq!(output.status.code(), Some(130));
        let output = String::from_utf8(output.stdout)?;
        let id = job_id(receipt.as_deref().unwrap_or(&output))?;
        checked(project.command(&["wait", id]))?;
        assert_eq!(
            fs::read_to_string(project.root.join("finished"))?,
            "finished"
        );
        wait_for_worker_ownership(&project.state, false)?;
    }
    Ok(())
}

#[test]
fn supervisor_replaces_a_worker_while_queued_work_is_paused() -> TestResult {
    let project = Project::new()?;
    checked(project.command(&["pause"]))?;
    let receipt = checked(project.command(&["submit"]))?;
    let id = job_id(&receipt)?;
    wait_for_worker_ownership(&project.state, true)?;
    let record = project.state.join("worker.json");
    let previous = wait_for_worker_pid(&project.state, None)?;
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(previous).ok_or("invalid worker PID")?,
        rustix::process::Signal::KILL,
    )?;
    wait_for_worker_pid(&project.state, Some(previous))?;
    assert_eq!(
        checked_json(project.command(&["status", id]))?["phase"],
        "queued"
    );
    checked(project.command(&["resume"]))?;
    checked(project.command(&["wait", id]))?;
    wait_for_worker_ownership(&project.state, false)?;
    assert!(!record.exists());
    Ok(())
}

#[test]
fn replacing_a_worker_preserves_the_running_command_without_replaying_it() -> TestResult {
    let project = Project::new()?;
    gated_workflow(&project)?;
    let receipt = checked(project.command(&["submit"]))?;
    let id = job_id(&receipt)?;
    wait_for_file(&project.root.join("started"))?;
    let previous = wait_for_worker_pid(&project.state, None)?;
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(previous).ok_or("invalid worker PID")?,
        rustix::process::Signal::KILL,
    )?;
    wait_for_worker_pid(&project.state, Some(previous))?;
    assert!(!project.root.join("finished").exists());
    fs::write(project.root.join("release"), "continue")?;
    checked(project.command(&["wait", id]))?;
    assert_eq!(
        fs::read_to_string(project.root.join("finished"))?,
        "finished"
    );
    assert_eq!(
        fs::read_to_string(project.root.join("started"))?
            .lines()
            .count(),
        1,
        "replacement worker replayed an already admitted command"
    );
    wait_for_worker_ownership(&project.state, false)?;
    Ok(())
}

#[test]
fn startup_failure_keeps_the_accepted_job_and_reports_its_id() -> TestResult {
    let project = Project::new()?;
    gated_workflow(&project)?;
    checked(project.command(&["pause"]))?;
    let log = project.state.join("supervisor.log");
    fs::create_dir(&log)?;
    let output = bounded_output(spawn(project.command(&["submit"]))?)?;
    assert!(!output.status.success());
    let receipt: Value = serde_json::from_slice(&output.stdout)?;
    let id = receipt["id"].as_str().ok_or("missing accepted job ID")?;
    assert!(receipt["worker_error"].is_string());
    assert_eq!(receipt["phase"], "queued");
    assert_eq!(
        checked_json(project.command(&["status", id]))?["phase"],
        "queued"
    );
    assert!(!project.root.join("started").exists());
    fs::remove_dir(log)?;
    fs::write(project.root.join("release"), "continue")?;
    checked(project.command(&["resume"]))?;
    checked(project.command(&["wait", id]))?;
    assert_eq!(
        fs::read_to_string(project.root.join("started"))?
            .lines()
            .count(),
        1
    );
    wait_for_worker_ownership(&project.state, false)?;
    Ok(())
}

#[test]
fn worker_lifecycle_is_not_a_public_command() -> TestResult {
    let project = Project::new()?;
    for args in [&["worker"][..], &["worker", "--once"][..]] {
        let output = bounded_output(spawn(project.command(args))?)?;
        assert!(!output.status.success());
    }
    assert!(!project.state.exists());
    Ok(())
}
