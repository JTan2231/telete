// Included in manager::tests to exercise the configured queue lifecycle.

fn script(path: &Path, body: &str) -> Result<()> {
    fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn record_commit(paths: &Paths, repo: &Path) -> Result<CommitId> {
    git::value(paths, repo, &["add", "."])?;
    git::value(paths, repo, &["-c", "user.name=Fixture", "-c", "user.email=fixture@localhost",
        "commit", "-m", "Fixture source"])?;
    git::commit(paths, repo, "HEAD")
}

fn fixture() -> Result<(tempfile::TempDir, ProjectConfig, CommitId)> {
    let temporary = tempfile::tempdir()?;
    let directory = temporary.path().canonicalize()?;
    let repo = directory.join("project");
    fs::create_dir(&repo)?;
    let paths = Paths { root:directory.join("state") };
    git::value(&paths, &repo, &["init", "--initial-branch=main"])?;
    fs::write(repo.join("payload.txt"), "selected source\n")?;
    script(&repo.join("check.sh"), "cat payload.txt >> \"$MARKER\"")?;
    let commit = record_commit(&paths, &repo)?;
    let project = ProjectConfig { repo, state:paths.root.clone(), workflow:Workflow {
        check:crate::workflow::WorkflowCommand { program:"check.sh".into(), args:Vec::new(),
            timeout_seconds:Some(10), env:BTreeMap::from([("MARKER".into(),
                paths.root.join("observed").display().to_string())]) },
        prepare:None, deploy:None, parameters:json!({"suite":"fixture"}),
    }};
    Ok((temporary, project, commit))
}

fn enqueue(project: &ProjectConfig, revision: &str) -> Result<(Paths, String)> {
    let paths = select(project, true)?;
    let value = submit(&paths, &project.repo, revision, project.workflow.clone())?;
    Ok((paths, value["id"].as_str().context("submitted id")?.into()))
}

fn finish_run(paths: &Paths, id: &str) -> Result<Value> {
    for _ in 0..8 {
        worker_once(paths)?;
        let value = status(paths, Some(id))?;
        if matches!(value["phase"].as_str(), Some("succeeded" | "failed" | "cancelled" | "blocked")) {
            return Ok(value);
        }
    }
    bail!("fixture workflow did not finish")
}

#[test]
fn cold_start_is_unpaused_and_read_only_status_creates_nothing() -> Result<()> {
    let (_temporary, project, _) = fixture()?;
    let paths = select(&project, false)?;
    assert!(!project.state.exists());
    assert_eq!(status(&paths, None)?["queue"], json!([]));
    assert_eq!(maintenance_status(&paths)?["held"], false);
    assert_eq!(crate::broker::status(&paths)?["executions"], json!([]));
    assert!(!project.state.exists());
    let (paths, id) = enqueue(&project, "HEAD")?;
    assert!(!Store::open(&paths.root)?.paused()?);
    assert_eq!(finish_run(&paths, &id)?["phase"], "succeeded");
    for implicit in ["cargo", "targets", "cache"] { assert!(!paths.root.join(implicit).exists()); }
    Ok(())
}

#[test]
fn source_is_resolved_at_submission_and_success_discards_details() -> Result<()> {
    let (_temporary, project, original) = fixture()?;
    let (paths, id) = enqueue(&project, "HEAD")?;
    assert_eq!(status(&paths, Some(&id))?["commit"], original.0);
    fs::write(project.repo.join("payload.txt"), "later source\n")?;
    let later = record_commit(&paths, &project.repo)?;
    fs::write(project.repo.join("payload.txt"), "dirty caller\n")?;
    script(&project.repo.join("check.sh"), "exit 88")?;
    fs::write(project.repo.join("untracked"), "caller only")?;
    worker_once(&paths)?;
    let checkout = paths.job(&id).join("worktree");
    assert_eq!(git::commit(&paths, &checkout, "HEAD")?, original);
    assert!(!checkout.join("untracked").exists());
    let completion = finish_run(&paths, &id)?;
    assert_eq!(completion["phase"], "succeeded");
    for detail in ["commit", "workflow", "runs", "environment"] { assert!(completion.get(detail).is_none()); }
    assert!(!paths.job(&id).exists());
    assert!(Store::open(&paths.root)?.jobs()?.is_empty());
    assert_eq!(crate::broker::status(&paths)?["executions"], json!([]));
    assert!(git::ref_value(&paths, &project.repo, &git::private_ref(&id)?)?.is_none());
    assert_eq!(git::commit(&paths, &project.repo, "HEAD")?, later);
    assert_eq!(fs::read_to_string(project.repo.join("payload.txt"))?, "dirty caller\n");
    assert_eq!(fs::read_to_string(paths.root.join("observed"))?, "selected source\n");
    Ok(())
}

#[test]
fn failure_keeps_logs_and_modified_workspace_until_a_newer_run_finishes() -> Result<()> {
    let (_temporary, mut project, source) = fixture()?;
    project.workflow.check = crate::workflow::WorkflowCommand { program:"/bin/sh".into(),
        args:vec!["-c".into(), "printf output; printf diagnostic >&2; printf generated > extra; exit 7".into()],
        env:BTreeMap::new(), timeout_seconds:Some(10) };
    let (paths, failed_id) = enqueue(&project, "HEAD")?;
    let failed = finish_run(&paths, &failed_id)?;
    assert_eq!(failed["phase"], "failed");
    assert_eq!(failed["commit"], source.0);
    assert_eq!(failed["runs"][0]["result"]["exit_code"], 7);
    assert_eq!(failed["runs"][0]["result"]["stderr"], "diagnostic");
    assert_eq!(failed["runs"][0]["result"]["stdout"], "output");
    assert_eq!(fs::read_to_string(paths.job(&failed_id).join("worktree/extra"))?, "generated");
    assert!(paths.job(&failed_id).join("workflow-check-0.log").is_file());
    assert!(!Store::open(&paths.root)?.paused()?);
    project.workflow.check.program = "/usr/bin/true".into();
    project.workflow.check.args.clear();
    let (_, cancelled) = enqueue(&project, "HEAD")?;
    cancel(&paths, &cancelled)?;
    worker_once(&paths)?;
    assert!(paths.job(&failed_id).exists());
    let (_, replacement) = enqueue(&project, "HEAD")?;
    assert!(paths.job(&failed_id).exists());
    assert_eq!(finish_run(&paths, &replacement)?["phase"], "succeeded");
    assert!(!paths.job(&failed_id).exists());
    assert!(status(&paths, Some(&failed_id))?.get("runs").is_none());
    assert!(status(&paths, None)?["failure"].is_null());
    assert!(git::ref_value(&paths, &project.repo, &git::private_ref(&failed_id)?)?.is_none());
    Ok(())
}

#[test]
fn a_new_failure_replaces_previous_failure_details() -> Result<()> {
    let (_temporary, mut project, _) = fixture()?;
    project.workflow.check.program = "/usr/bin/false".into();
    let (paths, first) = enqueue(&project, "HEAD")?;
    assert_eq!(finish_run(&paths, &first)?["phase"], "failed");
    let (_, second) = enqueue(&project, "HEAD")?;
    assert_eq!(finish_run(&paths, &second)?["phase"], "failed");
    assert!(!paths.job(&first).exists());
    assert!(paths.job(&second).exists());
    assert_eq!(status(&paths, None)?["failure"]["id"], second);
    Ok(())
}

#[test]
fn queued_commands_keep_configured_overrides_without_capturing_host_environment() -> Result<()> {
    let (_temporary, mut project, _) = fixture()?;
    project.workflow.check.program = "/bin/sh".into();
    project.workflow.check.args = vec!["-c".into(), "printf '%s' \"$MODE\" >> \"$MARKER\"".into()];
    project.workflow.check.env.insert("MODE".into(), "first".into());
    let (paths, first) = enqueue(&project, "HEAD")?;
    let queued = Store::open(&paths.root)?.job(&first)?;
    assert!(serde_json::to_value(&queued)?.get("environment").is_none());
    project.workflow.check.env.insert("MODE".into(), "second".into());
    let (_, second) = enqueue(&project, "HEAD")?;
    finish_run(&paths, &first)?;
    finish_run(&paths, &second)?;
    assert_eq!(fs::read_to_string(paths.root.join("observed"))?, "firstsecond");
    Ok(())
}

#[test]
fn preparation_hands_output_directory_directly_to_deployment() -> Result<()> {
    let (_temporary, mut project, _) = fixture()?;
    project.workflow.prepare = Some(crate::workflow::WorkflowCommand {
        program:"/usr/bin/python3".into(), args:vec!["-c".into(),
            "import json,os,pathlib; c=json.load(open(os.environ['TELETE_CONTEXT'])); pathlib.Path(c['outputs']['directory'],'artifact').write_text('prepared')".into()],
        env:BTreeMap::new(), timeout_seconds:Some(20),
    });
    project.workflow.deploy = Some(crate::workflow::WorkflowCommand {
        program:"/usr/bin/python3".into(), args:vec!["-c".into(),
            "import json,os,pathlib; c=json.load(open(os.environ['TELETE_CONTEXT'])); assert c['source']['commit']; a=pathlib.Path(c['inputs']['preparation'],'artifact').read_text(); pathlib.Path(os.environ['DEPLOYED']).write_text(a)".into()],
        env:BTreeMap::from([("DEPLOYED".into(), project.state.join("deployed").display().to_string())]),
        timeout_seconds:Some(20),
    });
    let (paths, id) = enqueue(&project, "HEAD")?;
    assert_eq!(finish_run(&paths, &id)?["phase"], "succeeded");
    assert_eq!(fs::read_to_string(paths.root.join("deployed"))?, "prepared");
    assert!(!paths.job(&id).exists());
    Ok(())
}

#[test]
fn cancellation_at_a_stage_boundary_never_deploys() -> Result<()> {
    let (_temporary, mut project, _) = fixture()?;
    project.workflow.deploy = Some(crate::workflow::WorkflowCommand {
        program:"/bin/sh".into(), args:vec!["-c".into(), "printf deployed > \"$DEPLOYED\"".into()],
        env:BTreeMap::from([("DEPLOYED".into(), project.state.join("deployed").display().to_string())]),
        timeout_seconds:Some(10),
    });
    let (paths, id) = enqueue(&project, "HEAD")?;
    worker_once(&paths)?;
    assert_eq!(worker_once(&paths)?["phase"], "deploying");
    cancel(&paths, &id)?;
    assert_eq!(finish_run(&paths, &id)?["phase"], "cancelled");
    assert!(!paths.root.join("deployed").exists());
    Ok(())
}

#[test]
fn missing_command_is_a_retained_start_failure() -> Result<()> {
    let (_temporary, mut project, _) = fixture()?;
    project.workflow.check.program = "missing-command".into();
    let (paths, id) = enqueue(&project, "HEAD")?;
    let value = finish_run(&paths, &id)?;
    assert_eq!(value["phase"], "failed");
    assert!(value["runs"][0]["result"]["start_error"].as_str().is_some());
    assert!(value["runs"][0]["result"]["exit_code"].is_null());
    Ok(())
}

#[test]
fn config_cannot_rebind_existing_queue_to_a_different_checkout() -> Result<()> {
    let (_temporary, project, _) = fixture()?;
    let (paths, _) = enqueue(&project, "HEAD")?;
    let before = fs::read(paths.root.join("queue.sqlite3"))?;
    let other_repo = project.repo.with_file_name("other-project");
    fs::create_dir(&other_repo)?;
    git::value(&paths, &other_repo, &["init", "--initial-branch=main"])?;
    let mut changed = project;
    changed.repo = other_repo;
    assert!(select(&changed, true).is_err());
    assert_eq!(fs::read(paths.root.join("queue.sqlite3"))?, before);
    Ok(())
}

#[test]
fn recovery_consumes_delayed_terminal_evidence_without_repeating_effects() -> Result<()> {
    let (_temporary, mut project, source) = fixture()?;
    project.workflow.prepare = Some(crate::workflow::WorkflowCommand {
        program:"/usr/bin/true".into(), args:Vec::new(), env:BTreeMap::new(), timeout_seconds:Some(10),
    });
    let (paths, id) = enqueue(&project, "HEAD")?;
    worker_once(&paths)?;
    assert_eq!(worker_once(&paths)?["phase"], "preparing");
    let store = Store::open(&paths.root)?;
    let mut job = store.job(&id)?;
    let run = job.runs[0].clone();
    let receipt = paths.job(&id).join(format!("{}.json", run.operation));
    let gate = paths.root.join("broker").join(format!("{id}-{}.json", run.operation));
    let completion_path = gate.with_extension("process").join("result.json");
    let completion = fs::read(&completion_path)?;
    fs::remove_file(&completion_path)?;
    fs::remove_file(&receipt)?;
    let mut execution: Value = serde_json::from_slice(&fs::read(&gate)?)?;
    execution["state"] = json!("running");
    execution["result"] = Value::Null;
    store::atomic_json(&gate, &execution)?;
    job.phase = Phase::Blocked;
    job.stopped_phase = Some(Phase::Checking);
    job.unresolved = true;
    job.outcome = None;
    job.operation = Some(Operation { phase:Phase::Checking, name:run.operation,
        commit:source, result:receipt.clone(), started:run.stamp });
    job.runs.clear();
    store.save(&mut job)?;
    assert!(recover(&paths, &id).is_err());
    assert!(resume(&paths).is_err());
    assert_eq!(worker_once(&paths)?["phase"], "blocked");
    assert!(!receipt.exists());
    assert_eq!(fs::read_to_string(paths.root.join("observed"))?, "selected source\n");
    store::atomic_bytes(&completion_path, &completion)?;
    assert_eq!(recover(&paths, &id)?["phase"], "preparing");
    assert_eq!(fs::read_to_string(paths.root.join("observed"))?, "selected source\n");
    assert_eq!(finish_run(&paths, &id)?["phase"], "succeeded");
    Ok(())
}

#[test]
fn cancellation_cannot_discard_a_command_with_unknown_outcome() -> Result<()> {
    let (_temporary, project, source) = fixture()?;
    let (paths, id) = enqueue(&project, "HEAD")?;
    worker_once(&paths)?;
    let store = Store::open(&paths.root)?;
    let mut job = store.job(&id)?;
    job.operation = Some(Operation { phase:Phase::Checking, name:"workflow-check-0".into(),
        commit:source, result:paths.job(&id).join("workflow-check-0.json"), started:store::now() });
    store.save(&mut job)?;
    assert_eq!(worker_once(&paths)?["phase"], "blocked");
    cancel(&paths, &id)?;
    assert!(recover(&paths, &id).is_err());
    assert_eq!(worker_once(&paths)?["phase"], "blocked");
    assert!(!paths.root.join("observed").exists());
    assert!(paths.job(&id).join("worktree").exists());
    Ok(())
}

#[test]
fn store_save_preserves_cancellation_and_rejects_stale_updates() -> Result<()> {
    let (_temporary, project, _) = fixture()?;
    let (paths, id) = enqueue(&project, "HEAD")?;
    let store = Store::open(&paths.root)?;
    let mut first = store.job(&id)?;
    let mut stale = first.clone();
    store.cancel(&id)?;
    first.last_error = Some("retained".into());
    store.save(&mut first)?;
    assert!(first.cancel_requested);
    assert!(store.save(&mut stale).is_err());
    assert_eq!(store.job(&id)?.last_error.as_deref(), Some("retained"));
    Ok(())
}
