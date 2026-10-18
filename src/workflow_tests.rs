// Included in manager::tests so these exercise the private worker transitions.

fn workflow_script(path: &Path, body: &str) -> Result<()> {
    fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn workflow_record_commit(
    paths: &Paths,
    repo: &Path,
    parents: &[CommitId],
    message: &str,
) -> Result<CommitId> {
    git::value(paths, repo, &["add", "."])?;
    let tree = git::value(paths, repo, &["write-tree"])?;
    let source = git::commit_tree(
        paths,
        repo,
        &tree,
        parents,
        "workflow-fixture",
        1_700_000_001,
        message,
    )?;
    git::value(paths, repo, &["update-ref", "HEAD", &source.0])?;
    Ok(source)
}

fn workflow_fixture(accepted: bool) -> Result<(tempfile::TempDir, Paths, PathBuf, CommitId)> {
    let temporary = tempfile::tempdir()?;
    let paths = Paths::for_test(temporary.path().join("telete"))?;
    let repo = temporary.path().join("ordinary-project");
    fs::create_dir(&repo)?;
    git::value(&paths, &repo, &["init", "--initial-branch=main"])?;
    fs::write(repo.join("payload.txt"), "committed\n")?;
    workflow_script(
        &repo.join("check.sh"),
        r#"[ "$1" = 'literal argument' ]
[ "$PROJECT_MODE" = frozen ]
cat payload.txt
printf '%s\n' "$1"
printf x >> "$WORKFLOW_MARKER""#,
    )?;
    let source = workflow_record_commit(&paths, &repo, &[], "Ordinary project source")?;
    if accepted {
        init(&paths, &repo, Some(&source.0))?;
    } else {
        init(&paths, &repo, None)?;
    }
    Ok((temporary, paths, repo, source))
}

fn workflow_selection(paths: &Paths) -> Workflow {
    Workflow {
        integrate: false,
        accept: false,
        check: crate::workflow::WorkflowCommand {
            program: "check.sh".into(),
            args: vec!["literal argument".into()],
            timeout_seconds: Some(10),
            env: BTreeMap::from([
                ("PROJECT_MODE".into(), "frozen".into()),
                (
                    "WORKFLOW_MARKER".into(),
                    paths.root.join("check-marker").display().to_string(),
                ),
            ]),
        },
        prepare: None,
        deploy: None,
        parameters: json!({"purpose":"ordinary-project-check"}),
    }
}

fn workflow_submit(
    paths: &Paths,
    repo: &Path,
    input: &CommitId,
    request: &str,
    workflow: Workflow,
) -> Result<Job> {
    let output = submit(
        paths,
        repo,
        &input.0,
        SubmitOptions {
            request_id: Some(request.into()),
            workflow,
        },
    )?;
    Store::open(&paths.root, false)?.job(output["id"].as_str().context("submitted job identity")?)
}

fn workflow_tick(paths: &Paths, id: &str) -> Result<Job> {
    worker_once(paths)?;
    Store::open(&paths.root, false)?.job(id)
}

fn workflow_stage_status(job: &Job, stage: &str) -> Result<String> {
    let output = job.output()?;
    Ok(output["stages"]
        .as_array()
        .context("workflow stage output")?
        .iter()
        .find(|entry| entry["stage"] == stage)
        .context("selected stage output")?["status"]
        .as_str()
        .context("stage status")?
        .to_owned())
}

#[test]
fn workflow_checks_exact_source_without_baseline() -> Result<()> {
    let (_temporary, paths, repo, source) = workflow_fixture(false)?;
    assert!(!repo.join("Cargo.toml").exists());
    assert!(!repo.join("pipeline").exists());
    assert!(configured(&paths)?.1.accepted_baseline.is_none());
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());

    // The caller's uncommitted files must not become the checked source.
    fs::write(repo.join("payload.txt"), "dirty caller payload\n")?;
    workflow_script(&repo.join("check.sh"), "exit 88")?;
    fs::write(repo.join("untracked.txt"), "caller-only\n")?;
    let queued = workflow_submit(
        &paths,
        &repo,
        &source,
        "exact-source",
        workflow_selection(&paths),
    )?;
    resume(&paths)?;
    let checking = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(checking.phase, Phase::Checking);
    assert_eq!(checking.candidate.as_ref(), Some(&source));
    assert!(checking.base.is_none());
    assert!(
        !paths
            .job(&queued.id.0)
            .join("worktree/untracked.txt")
            .exists()
    );
    let completed = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(completed.phase, Phase::Succeeded);
    assert_eq!(completed.runs.len(), 1);
    let run = &completed.runs[0];
    assert_eq!(run.candidate, source);
    assert_eq!(run.result.stdout, "committed\nliteral argument\n");
    assert!(run.result.success());
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    assert!(!completed.accepted && !completed.unresolved);
    for stage in ["integrate", "prepare", "accept", "deploy"] {
        assert_eq!(workflow_stage_status(&completed, stage)?, "not_requested");
    }
    assert_eq!(workflow_stage_status(&completed, "check")?, "succeeded");
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());
    assert_eq!(
        fs::read_to_string(repo.join("payload.txt"))?,
        "dirty caller payload\n"
    );
    Ok(())
}

#[test]
fn workflow_failed_exact_check_retains_failure_and_request_identity() -> Result<()> {
    let (_temporary, paths, repo, base) = workflow_fixture(false)?;
    workflow_script(
        &repo.join("check.sh"),
        "printf x >> \"$WORKFLOW_MARKER\"\nprintf 'submitted failure\\n'\nexit 7",
    )?;
    let source = workflow_record_commit(
        &paths,
        &repo,
        std::slice::from_ref(&base),
        "Failing project check",
    )?;
    let selected = workflow_selection(&paths);
    let queued = workflow_submit(&paths, &repo, &source, "failed-source", selected.clone())?;
    let repeated = workflow_submit(&paths, &repo, &source, "failed-source", selected.clone())?;
    assert_eq!(queued.id, repeated.id);
    let mut changed = selected.clone();
    changed.parameters["changed"] = json!(true);
    assert!(workflow_submit(&paths, &repo, &source, "failed-source", changed).is_err());
    assert!(workflow_submit(&paths, &repo, &base, "failed-source", selected).is_err());
    assert_eq!(Store::open(&paths.root, false)?.jobs()?.len(), 1);
    // A dirty passing replacement cannot turn the submitted failure into success.
    workflow_script(&repo.join("check.sh"), "exit 0")?;
    resume(&paths)?;
    assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, Phase::Checking);
    let completed = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(completed.phase, Phase::Failed);
    assert_eq!(completed.outcome, Some(Phase::Failed));
    assert_eq!(completed.candidate.as_ref(), Some(&source));
    assert_eq!(completed.runs[0].result.exit_code, Some(7));
    assert_eq!(completed.runs[0].result.stdout, "submitted failure\n");
    assert!(!completed.unresolved && completed.operation.is_none());
    assert_eq!(workflow_stage_status(&completed, "check")?, "failed");
    assert!(Store::open(&paths.root, false)?.paused()?);
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());
    Ok(())
}

#[test]
fn workflow_integration_checks_already_accepted_source_then_conditionally_accepts() -> Result<()> {
    let (_temporary, paths, repo, base) = workflow_fixture(true)?;
    let mut selected = workflow_selection(&paths);
    selected.integrate = true;
    selected.accept = true;
    let queued = workflow_submit(&paths, &repo, &base, "integrate-accepted", selected)?;
    resume(&paths)?;
    let checking = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(checking.phase, Phase::Checking);
    assert_eq!(checking.base.as_ref(), Some(&base));
    let candidate = checking.candidate.clone().context("integrated candidate")?;
    assert_ne!(candidate, base);
    assert!(git::ancestor(&paths, &repo, &base, &candidate)?);
    let accepting = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(accepting.phase, Phase::Accepting);
    assert_eq!(accepting.runs[0].candidate, candidate);
    assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
    let completed = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(completed.phase, Phase::Succeeded);
    assert!(completed.accepted);
    assert_eq!(
        git::ref_value(&paths, &repo, git::ACCEPTED)?,
        Some(candidate)
    );
    assert_eq!(workflow_stage_status(&completed, "integrate")?, "succeeded");
    assert_eq!(workflow_stage_status(&completed, "accept")?, "succeeded");
    Ok(())
}

#[test]
fn workflow_acceptance_conflict_keeps_the_fixed_checked_candidate() -> Result<()> {
    let (_temporary, paths, repo, base) = workflow_fixture(true)?;
    fs::write(repo.join("payload.txt"), "submitted\n")?;
    let source = workflow_record_commit(
        &paths,
        &repo,
        std::slice::from_ref(&base),
        "Submitted source",
    )?;
    let mut selected = workflow_selection(&paths);
    selected.integrate = true;
    selected.accept = true;
    let queued = workflow_submit(&paths, &repo, &source, "accept-race", selected)?;
    resume(&paths)?;
    let checking = workflow_tick(&paths, &queued.id.0)?;
    let candidate = checking.candidate.clone().context("integrated candidate")?;
    assert_ne!(candidate, source);
    assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, Phase::Accepting);
    let competing = workflow_record_commit(
        &paths,
        &repo,
        std::slice::from_ref(&base),
        "Competing acceptance",
    )?;
    git::advance(&paths, &repo, git::ACCEPTED, Some(&base), &competing)?;
    let completed = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(completed.phase, Phase::Failed);
    assert!(!completed.accepted && !completed.unresolved);
    assert_eq!(completed.base.as_ref(), Some(&base));
    assert_eq!(completed.candidate.as_ref(), Some(&candidate));
    assert_eq!(completed.runs.len(), 1);
    assert_eq!(completed.runs[0].candidate, candidate);
    assert!(completed.runs[0].result.success());
    assert_eq!(
        git::ref_value(&paths, &repo, git::ACCEPTED)?,
        Some(competing)
    );
    assert_eq!(workflow_stage_status(&completed, "check")?, "succeeded");
    assert_eq!(workflow_stage_status(&completed, "accept")?, "failed");
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn workflow_deploy_consumes_retained_preparation_outputs_outside_checkout() -> Result<()> {
    let (_temporary, paths, repo, source) = workflow_fixture(false)?;
    let prepare = paths.root.join("host-prepare.sh");
    let deploy = paths.root.join("host-deploy.sh");
    workflow_script(
        &prepare,
        r#"output=$(/usr/bin/sed -n 's/^ *"directory": "\(.*\)"$/\1/p' "$TELETE_CONTEXT")
[ -n "$output" ]
printf prepared > "$output/artifact.txt"
printf 'prepared output retained\n'"#,
    )?;
    workflow_script(
        &deploy,
        r#"receipt=$(/usr/bin/sed -n 's/^ *"preparation": "\(.*\)"$/\1/p' "$TELETE_CONTEXT")
[ -f "$receipt" ]
output=$(/usr/bin/sed -n 's/^ *"directory": "\(.*\)"$/\1/p' "$receipt")
[ "$(cat "$output/artifact.txt")" = prepared ]
printf x >> "$1"
printf 'deployed retained preparation\n'"#,
    )?;
    let mut selected = workflow_selection(&paths);
    selected.prepare = Some(crate::workflow::WorkflowCommand {
        program: prepare.clone(),
        args: Vec::new(),
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    let marker = paths.root.join("deploy-marker");
    selected.deploy = Some(crate::workflow::WorkflowCommand {
        program: deploy.clone(),
        args: vec![marker.display().to_string()],
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    let queued = workflow_submit(&paths, &repo, &source, "prepare-and-deploy", selected)?;
    resume(&paths)?;
    assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, Phase::Checking);
    assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, Phase::Preparing);
    let prepared = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(prepared.phase, Phase::Deploying);
    let prepare_run = &prepared.runs[1];
    assert_eq!(prepare_run.command.program, prepare);
    let prepared_output = PathBuf::from(
        prepare_run
            .command
            .context
            .as_ref()
            .context("prepare context")?
            .value["outputs"]["directory"]
            .as_str()
            .context("prepare output directory")?,
    );
    assert!(prepared_output.starts_with(paths.job(&queued.id.0).join("outputs")));
    assert!(!prepared_output.starts_with(paths.job(&queued.id.0).join("worktree")));
    assert_eq!(
        fs::read_to_string(prepared_output.join("artifact.txt"))?,
        "prepared"
    );
    let completed = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(completed.phase, Phase::Succeeded);
    assert_eq!(completed.runs.len(), 3);
    let deploy_run = &completed.runs[2];
    assert_eq!(deploy_run.command.program, deploy);
    assert_eq!(deploy_run.result.stdout, "deployed retained preparation\n");
    assert_eq!(
        deploy_run
            .command
            .context
            .as_ref()
            .context("deploy context")?
            .value["inputs"]["preparation"],
        paths
            .job(&queued.id.0)
            .join(format!("{}.json", prepare_run.operation))
            .display()
            .to_string()
    );
    assert_eq!(fs::read_to_string(marker)?, "x");
    assert_eq!(
        fs::read_to_string(prepared_output.join("artifact.txt"))?,
        "prepared"
    );
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());
    Ok(())
}

#[test]
fn workflow_cancel_after_preparation_never_admits_deployment() -> Result<()> {
    let (_temporary, paths, repo, source) = workflow_fixture(false)?;
    let deploy = paths.root.join("cancelled-deploy.sh");
    let marker = paths.root.join("cancelled-deploy-marker");
    workflow_script(&deploy, "printf x >> \"$1\"")?;
    let mut selected = workflow_selection(&paths);
    selected.prepare = Some(crate::workflow::WorkflowCommand {
        program: "/usr/bin/true".into(),
        args: Vec::new(),
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    selected.deploy = Some(crate::workflow::WorkflowCommand {
        program: deploy,
        args: vec![marker.display().to_string()],
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    let queued = workflow_submit(&paths, &repo, &source, "cancel-before-deploy", selected)?;
    resume(&paths)?;
    assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, Phase::Checking);
    assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, Phase::Preparing);
    let prepared = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(prepared.phase, Phase::Deploying);
    assert_eq!(prepared.runs.len(), 2);
    assert!(prepared.runs[1].result.success());
    assert!(prepared.operation.is_none());
    cancel(&paths, &queued.id.0)?;
    let cancelled = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(cancelled.phase, Phase::Cancelled);
    assert_eq!(cancelled.outcome, Some(Phase::Cancelled));
    assert!(cancelled.operation.is_none() && !cancelled.unresolved);
    assert_eq!(cancelled.runs.len(), 2);
    assert!(!marker.exists());
    assert!(!crate::broker::recorded(
        &paths,
        &format!("{}-workflow-deploy-2", queued.id)
    )?);
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn workflow_recovery_requires_exact_completion_and_never_repeats_check() -> Result<()> {
    let (_temporary, paths, repo, source) = workflow_fixture(false)?;
    let mut selected = workflow_selection(&paths);
    // Leave another stage pending so the completed check's checkout remains.
    selected.prepare = Some(crate::workflow::WorkflowCommand {
        program: "/usr/bin/true".into(),
        args: Vec::new(),
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    let queued = workflow_submit(&paths, &repo, &source, "recover-exact-check", selected)?;
    resume(&paths)?;
    assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, Phase::Checking);
    let prepared = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(prepared.phase, Phase::Preparing);
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    let run = &prepared.runs[0];
    let receipt = paths
        .job(&queued.id.0)
        .join(format!("{}.json", run.operation));
    let broker_record = paths
        .root
        .join("broker")
        .join(format!("{}-{}.json", queued.id, run.operation));
    let completion_path = broker_record.with_extension("process").join("result.json");
    let completion = fs::read(&completion_path)?;
    fs::remove_file(&completion_path)?;
    fs::remove_file(&receipt)?;
    let mut execution: Value = serde_json::from_slice(&fs::read(&broker_record)?)?;
    execution["state"] = json!("running");
    execution["result"] = Value::Null;
    store::atomic_json(&broker_record, &execution)?;
    let store = Store::open(&paths.root, false)?;
    let mut blocked = store.job(&queued.id.0)?;
    blocked.phase = Phase::Blocked;
    blocked.stopped_phase = Some(Phase::Checking);
    blocked.outcome = Some(Phase::Failed);
    blocked.unresolved = true;
    blocked.operation = Some(Operation {
        phase: Phase::Checking,
        name: run.operation.clone(),
        candidate: source.clone(),
        result: receipt.clone(),
        started: run.stamp,
    });
    blocked.runs.clear();
    store.save(&mut blocked)?;
    assert_eq!(workflow_stage_status(&blocked, "check")?, "unresolved");
    let before = fs::read(&broker_record)?;
    assert!(recover(&paths, &queued.id.0).is_err());
    assert_eq!(fs::read(&broker_record)?, before);
    assert_eq!(store.job(&queued.id.0)?.phase, Phase::Blocked);
    assert!(!receipt.exists());
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");

    // A delayed, correlated terminal completion authorizes reconciliation only.
    store::atomic_bytes(&completion_path, &completion)?;
    let foreign_result = paths.root.join("foreign-workflow-result.json");
    let mut changed = store.job(&queued.id.0)?;
    changed
        .operation
        .as_mut()
        .context("retained check intent")?
        .result = foreign_result.clone();
    store.save(&mut changed)?;
    assert!(recover(&paths, &queued.id.0).is_err());
    assert!(!foreign_result.exists());
    assert!(!receipt.exists());
    let mut changed = store.job(&queued.id.0)?;
    let operation = changed
        .operation
        .as_mut()
        .context("retained check intent")?;
    operation.result = receipt.clone();
    operation.name = "workflow-check-999".into();
    store.save(&mut changed)?;
    assert!(recover(&paths, &queued.id.0).is_err());
    assert!(!receipt.exists());
    let mut changed = store.job(&queued.id.0)?;
    changed
        .operation
        .as_mut()
        .context("retained check intent")?
        .name = run.operation.clone();
    store.save(&mut changed)?;
    let recovered = recover(&paths, &queued.id.0)?;
    assert_eq!(recovered["phase"], "preparing");
    let recovered = store.job(&queued.id.0)?;
    assert!(!recovered.unresolved && recovered.operation.is_none());
    assert_eq!(recovered.runs.len(), 1);
    assert!(recovered.runs[0].result.success());
    assert_eq!(recovered.runs[0].candidate, source);
    assert!(receipt.exists());
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    assert_eq!(workflow_stage_status(&recovered, "check")?, "succeeded");
    Ok(())
}

#[test]
fn workflow_acceptance_reconciles_existing_promotion_before_cancellation() -> Result<()> {
    for (promoted, cancelled) in [(true, false), (true, true), (false, true)] {
        let (_temporary, paths, repo, base) = workflow_fixture(true)?;
        let mut selected = workflow_selection(&paths);
        selected.integrate = true;
        selected.accept = true;
        let queued = workflow_submit(&paths, &repo, &base, "accept-interruption", selected)?;
        resume(&paths)?;
        workflow_tick(&paths, &queued.id.0)?;
        let accepting = workflow_tick(&paths, &queued.id.0)?;
        assert_eq!(accepting.phase, Phase::Accepting);
        let candidate = accepting.candidate.context("checked candidate")?;
        if promoted {
            // Termination after Git promotion but before the journal update.
            git::advance(&paths, &repo, git::ACCEPTED, Some(&base), &candidate)?;
        }
        if cancelled {
            cancel(&paths, &queued.id.0)?;
        }
        let completed = workflow_tick(&paths, &queued.id.0)?;
        assert_eq!(
            completed.phase,
            if cancelled {
                Phase::Cancelled
            } else {
                Phase::Succeeded
            }
        );
        assert_eq!(completed.accepted, promoted);
        assert!(!completed.unresolved);
        assert_eq!(completed.runs.len(), 1);
        assert_eq!(completed.runs[0].candidate, candidate);
        assert_eq!(
            git::ref_value(&paths, &repo, git::ACCEPTED)?,
            Some(if promoted { candidate } else { base })
        );
        assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    }
    Ok(())
}

#[test]
fn workflow_cancel_cannot_clear_an_unresolved_command_or_repeat_it() -> Result<()> {
    let (_temporary, paths, repo, source) = workflow_fixture(false)?;
    let queued = workflow_submit(
        &paths,
        &repo,
        &source,
        "unknown-completion",
        workflow_selection(&paths),
    )?;
    resume(&paths)?;
    let mut checking = workflow_tick(&paths, &queued.id.0)?;
    let store = Store::open(&paths.root, false)?;
    checking.operation = Some(Operation {
        phase: Phase::Checking,
        name: "workflow-check-0".into(),
        candidate: source,
        result: paths.job(&queued.id.0).join("workflow-check-0.json"),
        started: store::now(),
    });
    store.save(&mut checking)?;
    let blocked = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(blocked.phase, Phase::Blocked);
    assert!(blocked.unresolved && blocked.operation.is_some());
    cancel(&paths, &queued.id.0)?;
    assert!(recover(&paths, &queued.id.0).is_err());
    assert!(resume(&paths).is_err());
    let retained = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(retained.phase, Phase::Blocked);
    assert!(retained.cancel_requested && retained.unresolved);
    assert!(retained.operation.is_some() && retained.runs.is_empty());
    assert!(!paths.root.join("check-marker").exists());
    Ok(())
}

#[test]
fn workflow_failed_deployment_keeps_accepted_source() -> Result<()> {
    let (_temporary, paths, repo, base) = workflow_fixture(true)?;
    let mut selected = workflow_selection(&paths);
    selected.integrate = true;
    selected.accept = true;
    selected.prepare = Some(crate::workflow::WorkflowCommand {
        program: "/usr/bin/true".into(),
        args: Vec::new(),
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    selected.deploy = Some(crate::workflow::WorkflowCommand {
        program: "/usr/bin/false".into(),
        args: Vec::new(),
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    let queued = workflow_submit(&paths, &repo, &base, "failed-deployment", selected)?;
    resume(&paths)?;
    for expected in [
        Phase::Checking,
        Phase::Preparing,
        Phase::Accepting,
        Phase::Deploying,
    ] {
        assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, expected);
    }
    let completed = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(completed.phase, Phase::Failed);
    assert!(completed.accepted && !completed.unresolved && completed.operation.is_none());
    assert_eq!(completed.runs.len(), 3);
    assert_eq!(completed.runs[2].result.exit_code, Some(1));
    assert_eq!(
        git::ref_value(&paths, &repo, git::ACCEPTED)?,
        completed.candidate
    );
    assert_eq!(workflow_stage_status(&completed, "accept")?, "succeeded");
    assert_eq!(workflow_stage_status(&completed, "deploy")?, "failed");
    Ok(())
}

#[test]
fn workflow_changed_acceptance_fails_before_deployment_admission() -> Result<()> {
    let (_temporary, paths, repo, base) = workflow_fixture(true)?;
    let marker = paths.root.join("unexpected-deployment");
    let program = paths.root.join("deployment.sh");
    workflow_script(&program, "printf x >> \"$1\"")?;
    let mut selected = workflow_selection(&paths);
    selected.integrate = true;
    selected.accept = true;
    selected.prepare = Some(crate::workflow::WorkflowCommand {
        program: "/usr/bin/true".into(),
        args: Vec::new(),
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    selected.deploy = Some(crate::workflow::WorkflowCommand {
        program,
        args: vec![marker.display().to_string()],
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    });
    let queued = workflow_submit(&paths, &repo, &base, "deploy-ref-race", selected)?;
    resume(&paths)?;
    for expected in [
        Phase::Checking,
        Phase::Preparing,
        Phase::Accepting,
        Phase::Deploying,
    ] {
        assert_eq!(workflow_tick(&paths, &queued.id.0)?.phase, expected);
    }
    let candidate = Store::open(&paths.root, false)?
        .job(&queued.id.0)?
        .candidate;
    let competing = workflow_record_commit(&paths, &repo, &[base], "Competing accepted source")?;
    git::advance(&paths, &repo, git::ACCEPTED, candidate.as_ref(), &competing)?;
    let completed = workflow_tick(&paths, &queued.id.0)?;
    assert_eq!(completed.phase, Phase::Failed);
    assert!(completed.accepted && !completed.unresolved && completed.operation.is_none());
    assert_eq!(completed.runs.len(), 2);
    assert_eq!(
        git::ref_value(&paths, &repo, git::ACCEPTED)?,
        Some(competing)
    );
    assert!(!marker.exists());
    assert!(!crate::broker::recorded(
        &paths,
        &format!("{}-workflow-deploy-2", queued.id)
    )?);
    Ok(())
}
