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
        init_project(&paths, &repo, Some(&source.0))?;
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
            workflow: Some(workflow),
            ..SubmitOptions::default()
        },
    )?;
    Store::open(&paths.root, false)?.job(output["id"].as_str().context("submitted job identity")?)
}

async fn workflow_tick(paths: &Paths, id: &str) -> Result<Job> {
    worker_once(paths).await?;
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

#[tokio::test]
async fn workflow_checks_exact_source_without_baseline_or_signer() -> Result<()> {
    let (_temporary, paths, repo, source) = workflow_fixture(false)?;
    assert!(!repo.join("Cargo.toml").exists());
    assert!(!repo.join("pipeline").exists());
    assert!(!paths.root.join("signing.json").exists());
    assert!(configured(&paths)?.1.accepted_baseline.is_none());
    assert!(configured(&paths)?.1.project);
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());
    assert!(git::ref_value(&paths, &repo, git::PROJECT_ACCEPTED)?.is_none());

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
    assert!(queued.policy.signing_policy.is_none());
    resume(&paths)?;
    let checking = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(checking.phase, Phase::Checking);
    assert_eq!(checking.candidate.as_ref(), Some(&source));
    assert!(checking.base.is_none());
    assert!(
        !paths
            .job(&queued.id.0)
            .join("worktree/untracked.txt")
            .exists()
    );
    let completed = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(completed.phase, Phase::Succeeded);
    assert_eq!(completed.workflow_runs.len(), 1);
    let run = &completed.workflow_runs[0];
    assert_eq!(run.candidate, source);
    assert_eq!(run.result.stdout, "committed\nliteral argument\n");
    assert!(run.result.success());
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    assert!(completed.validations.is_empty() && completed.preparation.is_none());
    assert!(!completed.accepted && !completed.unresolved);
    for stage in ["integrate", "prepare", "accept", "deploy"] {
        assert_eq!(workflow_stage_status(&completed, stage)?, "not_requested");
    }
    assert_eq!(workflow_stage_status(&completed, "check")?, "succeeded");
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());
    assert!(git::ref_value(&paths, &repo, git::PROJECT_ACCEPTED)?.is_none());
    assert_eq!(
        fs::read_to_string(repo.join("payload.txt"))?,
        "dirty caller payload\n"
    );
    Ok(())
}

#[tokio::test]
async fn workflow_failed_exact_check_retains_failure_and_request_identity() -> Result<()> {
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
    assert_eq!(
        workflow_tick(&paths, &queued.id.0).await?.phase,
        Phase::Checking
    );
    let completed = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(completed.phase, Phase::Failed);
    assert_eq!(completed.outcome, Some(Phase::Failed));
    assert_eq!(completed.candidate.as_ref(), Some(&source));
    assert_eq!(completed.workflow_runs[0].result.exit_code, Some(7));
    assert_eq!(
        completed.workflow_runs[0].result.stdout,
        "submitted failure\n"
    );
    assert!(!completed.unresolved && completed.operation.is_none());
    assert_eq!(workflow_stage_status(&completed, "check")?, "failed");
    assert!(Store::open(&paths.root, false)?.paused()?);
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());
    assert!(git::ref_value(&paths, &repo, git::PROJECT_ACCEPTED)?.is_none());
    Ok(())
}

#[test]
fn workflow_submission_cannot_change_cell_queue_authority() -> Result<()> {
    let (_temporary, paths, repo, base) = fixture()?;
    assert!(!configured(&paths)?.1.project);
    assert!(
        workflow_submit(
            &paths,
            &repo,
            &base,
            "cell-workflow-rejected",
            workflow_selection(&paths)
        )
        .is_err()
    );
    assert!(Store::open(&paths.root, false)?.jobs()?.is_empty());
    assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
    assert!(git::ref_value(&paths, &repo, git::PROJECT_ACCEPTED)?.is_none());
    Ok(())
}

#[tokio::test]
async fn workflow_integration_checks_already_accepted_source_then_conditionally_accepts()
-> Result<()> {
    let (_temporary, paths, repo, base) = workflow_fixture(true)?;
    let mut selected = workflow_selection(&paths);
    selected.integrate = true;
    selected.accept = true;
    let queued = workflow_submit(&paths, &repo, &base, "integrate-accepted", selected)?;
    resume(&paths)?;
    let checking = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(checking.phase, Phase::Checking);
    assert_eq!(checking.base.as_ref(), Some(&base));
    let candidate = checking.candidate.clone().context("integrated candidate")?;
    assert_ne!(candidate, base);
    assert!(git::ancestor(&paths, &repo, &base, &candidate)?);
    let accepting = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(accepting.phase, Phase::Accepting);
    assert_eq!(accepting.workflow_runs[0].candidate, candidate);
    assert_eq!(
        git::ref_value(&paths, &repo, git::PROJECT_ACCEPTED)?,
        Some(base)
    );
    let completed = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(completed.phase, Phase::Succeeded);
    assert!(completed.accepted);
    assert_eq!(
        git::ref_value(&paths, &repo, git::PROJECT_ACCEPTED)?,
        Some(candidate)
    );
    assert_eq!(workflow_stage_status(&completed, "integrate")?, "succeeded");
    assert_eq!(workflow_stage_status(&completed, "accept")?, "succeeded");
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());
    Ok(())
}

#[tokio::test]
async fn workflow_acceptance_conflict_keeps_the_fixed_checked_candidate() -> Result<()> {
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
    let checking = workflow_tick(&paths, &queued.id.0).await?;
    let candidate = checking.candidate.clone().context("integrated candidate")?;
    assert_ne!(candidate, source);
    assert_eq!(
        workflow_tick(&paths, &queued.id.0).await?.phase,
        Phase::Accepting
    );
    let competing = workflow_record_commit(
        &paths,
        &repo,
        std::slice::from_ref(&base),
        "Competing acceptance",
    )?;
    git::advance(
        &paths,
        &repo,
        git::PROJECT_ACCEPTED,
        Some(&base),
        &competing,
    )?;
    let completed = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(completed.phase, Phase::Failed);
    assert!(!completed.accepted && !completed.unresolved);
    assert_eq!(completed.base.as_ref(), Some(&base));
    assert_eq!(completed.candidate.as_ref(), Some(&candidate));
    assert_eq!(completed.workflow_runs.len(), 1);
    assert_eq!(completed.workflow_runs[0].candidate, candidate);
    assert!(completed.workflow_runs[0].result.success());
    assert_eq!(
        git::ref_value(&paths, &repo, git::PROJECT_ACCEPTED)?,
        Some(competing)
    );
    assert_eq!(workflow_stage_status(&completed, "check")?, "succeeded");
    assert_eq!(workflow_stage_status(&completed, "accept")?, "failed");
    assert!(git::ref_value(&paths, &repo, git::ACCEPTED)?.is_none());
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflow_deploy_consumes_retained_preparation_outputs_outside_checkout() -> Result<()> {
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
    assert_eq!(
        workflow_tick(&paths, &queued.id.0).await?.phase,
        Phase::Checking
    );
    assert_eq!(
        workflow_tick(&paths, &queued.id.0).await?.phase,
        Phase::Preparing
    );
    let prepared = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(prepared.phase, Phase::Deploying);
    let prepare_run = &prepared.workflow_runs[1];
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
    let completed = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(completed.phase, Phase::Succeeded);
    assert_eq!(completed.workflow_runs.len(), 3);
    let deploy_run = &completed.workflow_runs[2];
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

#[tokio::test]
async fn workflow_cancel_after_preparation_never_admits_deployment() -> Result<()> {
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
    assert_eq!(
        workflow_tick(&paths, &queued.id.0).await?.phase,
        Phase::Checking
    );
    assert_eq!(
        workflow_tick(&paths, &queued.id.0).await?.phase,
        Phase::Preparing
    );
    let prepared = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(prepared.phase, Phase::Deploying);
    assert_eq!(prepared.workflow_runs.len(), 2);
    assert!(prepared.workflow_runs[1].result.success());
    assert!(prepared.operation.is_none());
    cancel(&paths, &queued.id.0)?;
    let cancelled = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(cancelled.phase, Phase::Cancelled);
    assert_eq!(cancelled.outcome, Some(Phase::Cancelled));
    assert!(cancelled.operation.is_none() && !cancelled.unresolved);
    assert_eq!(cancelled.workflow_runs.len(), 2);
    assert!(!marker.exists());
    assert!(!crate::broker::recorded(
        &paths,
        &format!("{}-workflow-deploy-2", queued.id)
    )?);
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflow_recovery_requires_exact_completion_and_never_repeats_check() -> Result<()> {
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
    assert_eq!(
        workflow_tick(&paths, &queued.id.0).await?.phase,
        Phase::Checking
    );
    let prepared = workflow_tick(&paths, &queued.id.0).await?;
    assert_eq!(prepared.phase, Phase::Preparing);
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    let run = &prepared.workflow_runs[0];
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
    blocked.workflow_runs.clear();
    store.save(&mut blocked)?;
    assert_eq!(workflow_stage_status(&blocked, "check")?, "unresolved");
    let before = fs::read(&broker_record)?;
    assert!(recover(&paths, &queued.id.0).await.is_err());
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
    assert!(recover(&paths, &queued.id.0).await.is_err());
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
    assert!(recover(&paths, &queued.id.0).await.is_err());
    assert!(!receipt.exists());
    let mut changed = store.job(&queued.id.0)?;
    changed
        .operation
        .as_mut()
        .context("retained check intent")?
        .name = run.operation.clone();
    store.save(&mut changed)?;
    let recovered = recover(&paths, &queued.id.0).await?;
    assert_eq!(recovered["phase"], "preparing");
    let recovered = store.job(&queued.id.0)?;
    assert!(!recovered.unresolved && recovered.operation.is_none());
    assert_eq!(recovered.workflow_runs.len(), 1);
    assert!(recovered.workflow_runs[0].result.success());
    assert_eq!(recovered.workflow_runs[0].candidate, source);
    assert!(receipt.exists());
    assert_eq!(fs::read_to_string(paths.root.join("check-marker"))?, "x");
    assert_eq!(workflow_stage_status(&recovered, "check")?, "succeeded");
    Ok(())
}
