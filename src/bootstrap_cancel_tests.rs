// Retained evidence fixtures only. These tests never launch Cargo or a validator.

fn bootstrap_cancel_fixture(frozen: bool) -> Result<(tempfile::TempDir, Paths, PathBuf, Job)> {
    let (temporary, paths, repo, base) = fixture()?;
    let store = Store::open(&paths.root, false)?;
    let mut blocked = job(&paths, &base, "failed-bootstrap-cancel")?;
    blocked.base = Some(base.clone());
    blocked.candidate = Some(base.clone());
    blocked.phase = Phase::Blocked;
    blocked.stopped_phase = Some(Phase::Checking);
    blocked.outcome = Some(Phase::Failed);
    blocked.outcome_message = Some("retained validator bootstrap failure".into());
    blocked.last_error = blocked.outcome_message.clone();
    blocked.unresolved = true;
    blocked.options.run_tests = true;
    blocked.options.notify = true;
    blocked.notification = Some(Notification {
        message: email::api::Message {
            subject: "retained bootstrap failure".into(),
            body: "retained bootstrap diagnostics".into(),
            idempotency_key: Some("retained-bootstrap-failure".into()),
        },
        accepted: true,
        uncertain: false,
        attempts: Vec::new(),
        created: 1_700_000_003,
    });
    if frozen {
        let mut commands = CommandPolicy::capture(&paths)?;
        commands.environment.insert(
            "PATH".into(),
            paths
                .root
                .join("missing-retained-toolchain")
                .display()
                .to_string(),
        );
        blocked.policy.commands = Some(commands);
    }
    let directory = paths.job(&blocked.id.0);
    git::ensure_worktree(&paths, &repo, &directory.join("worktree"), &base)?;
    for kind in ["input", "candidate"] {
        git::advance(
            &paths,
            &repo,
            &git::private_ref(&blocked.id.0, kind)?,
            None,
            &base,
        )?;
    }
    blocked.operation = Some(Operation {
        phase: Phase::Checking,
        candidate: base,
        name: "validation-0".into(),
        result: directory.join("validation-0.json"),
        started: 1_700_000_002,
    });
    let blocked = store.insert(&blocked)?;
    Ok((temporary, paths, repo, blocked))
}

fn bootstrap_cancel_result(exit_code: i32) -> crate::process::ProcessResult {
    crate::process::ProcessResult {
        exit_code: Some(exit_code),
        stdout: "retained bootstrap output\n".into(),
        stderr: "retained compiler diagnostic\n".into(),
        timed_out: false,
        start_error: None,
        output_error: None,
        signal: None,
    }
}

fn bootstrap_cancel_command(paths: &Paths, job: &Job) -> Result<crate::process::CommandSpec> {
    let worktree = paths.job(&job.id.0).join("worktree");
    let mut command = validation::job_bootstrap_spec(paths, &worktree, job)?;
    if job.policy.commands.is_some() {
        // Admission already chose this executable. It need not still exist to
        // inspect its retained terminal result, and cancellation never starts it.
        command.program = paths.root.join("missing-retained-toolchain/cargo");
        assert!(!command.program.exists());
    } else {
        assert_eq!(command.program, Path::new("cargo"));
    }
    Ok(command)
}

fn bootstrap_cancel_retain(
    paths: &Paths,
    key: &str,
    class: crate::broker::ResourceClass,
    command: &crate::process::CommandSpec,
    result: Option<&crate::process::ProcessResult>,
) -> Result<PathBuf> {
    let record = paths
        .root
        .join("broker")
        .join(format!("{}.json", key.replace('/', "_")));
    store::atomic_json(
        &record,
        &json!({
            "schema": 1, "key": key, "class": class, "command": command,
            "state": if result.is_some() { "finished" } else { "running" },
            "owner_pid": 1, "result": result,
        }),
    )?;
    let process = record.with_extension("process");
    store::atomic_json(&process.join("request.json"), command)?;
    store::atomic_json(
        &process.join("intent.json"),
        &json!({"schema":1,"owner_pid":1}),
    )?;
    if let Some(context) = &command.context {
        store::atomic_json(&context.path, &context.value)?;
    }
    let terminal = process.join("result.json");
    if let Some(result) = result {
        store::atomic_json(
            &terminal,
            &json!({"schema":1,"request":command,"result":result}),
        )?;
    } else if terminal.exists() {
        fs::remove_file(terminal)?;
    }
    Ok(record)
}

#[allow(clippy::too_many_lines)]
fn assert_failed_bootstrap_cancel_preserves_failure(frozen: bool) -> Result<()> {
    let (_temporary, paths, repo, blocked) = bootstrap_cancel_fixture(frozen)?;
    let store = Store::open(&paths.root, false)?;
    let command = bootstrap_cancel_command(&paths, &blocked)?;
    let key = validation::job_bootstrap_key(&blocked)?;
    let record = bootstrap_cancel_retain(
        &paths,
        &key,
        crate::broker::ResourceClass::Heavy,
        &command,
        Some(&bootstrap_cancel_result(101)),
    )?;
    let process = record.with_extension("process");
    let retained_execution = fs::read(&record)?;
    let retained_request = fs::read(process.join("request.json"))?;
    let retained_completion = fs::read(process.join("result.json"))?;
    let retained_context = command
        .context
        .as_ref()
        .map(|context| fs::read(&context.path))
        .transpose()?;
    let base = blocked.base.as_ref().context("fixed base")?;
    let candidate = blocked.candidate.as_ref().context("candidate")?;
    let directory = paths.job(&blocked.id.0);
    let aggregate = validation::receipt_path(&paths, base, candidate, true, true);
    let abandoned = directory.join("validation-0.abandoned.json");
    assert!(!aggregate.exists() && !abandoned.exists());

    cancel(&paths, &blocked.id.0)?;

    let cancelled = store.job(&blocked.id.0)?;
    assert_eq!(cancelled.phase, Phase::Cancelled);
    assert!(cancelled.cancel_requested && !cancelled.unresolved);
    assert!(cancelled.operation.is_none());
    assert_eq!(cancelled.outcome, Some(Phase::Failed));
    assert_eq!(cancelled.outcome, blocked.outcome);
    assert_eq!(cancelled.outcome_message, blocked.outcome_message);
    assert_eq!(cancelled.outcome_generation, blocked.outcome_generation);
    assert_eq!(cancelled.last_error, blocked.last_error);
    assert_eq!(
        serde_json::to_value(&cancelled.notification)?,
        serde_json::to_value(&blocked.notification)?
    );
    assert_eq!(
        serde_json::to_value(&cancelled.policy)?,
        serde_json::to_value(&blocked.policy)?
    );
    assert_eq!(cancelled.input, blocked.input);
    assert_eq!(cancelled.base, blocked.base);
    assert_eq!(cancelled.candidate, blocked.candidate);
    assert!(cancelled.validations.is_empty() && cancelled.preparation.is_none());
    assert!(store.paused()?);
    let retained: ValidationAbandonment = serde_json::from_slice(&fs::read(&abandoned)?)?;
    assert_eq!(retained.schema, 1);
    assert_eq!(retained.job, blocked.id);
    assert_eq!(&retained.base, base);
    assert_eq!(
        serde_json::to_value(&retained.operation)?,
        serde_json::to_value(&blocked.operation)?
    );
    assert_eq!(fs::read(&record)?, retained_execution);
    assert_eq!(fs::read(process.join("request.json"))?, retained_request);
    assert_eq!(fs::read(process.join("result.json"))?, retained_completion);
    if let Some(context) = &command.context {
        assert_eq!(Some(fs::read(&context.path)?), retained_context);
    }
    assert!(!aggregate.exists());
    assert!(!directory.join("validation-0.json").exists());
    assert_eq!(
        git::ref_value(&paths, &repo, git::ACCEPTED)?.as_ref(),
        Some(base)
    );
    for kind in ["input", "candidate"] {
        assert_eq!(
            git::ref_value(&paths, &repo, &git::private_ref(&blocked.id.0, kind)?)?.as_ref(),
            Some(candidate)
        );
    }
    git::clean_candidate(&paths, &directory.join("worktree"), candidate)?;
    assert!(require_quiescent(&paths).is_ok());
    let retained_abandonment = fs::read(&abandoned)?;
    cancel(&paths, &blocked.id.0)?;
    assert_eq!(store.job(&blocked.id.0)?.revision, cancelled.revision);
    assert_eq!(fs::read(&abandoned)?, retained_abandonment);
    assert_eq!(fs::read(process.join("result.json"))?, retained_completion);
    Ok(())
}

#[test]
fn failed_legacy_bootstrap_cancellation_preserves_the_original_failure() -> Result<()> {
    assert_failed_bootstrap_cancel_preserves_failure(false)
}

#[test]
fn failed_frozen_bootstrap_cancellation_uses_only_retained_evidence() -> Result<()> {
    assert_failed_bootstrap_cancel_preserves_failure(true)
}

#[test]
fn failed_bootstrap_cancellation_refuses_missing_successful_or_running_proof() -> Result<()> {
    let (_temporary, paths, _repo, blocked) = bootstrap_cancel_fixture(false)?;
    let store = Store::open(&paths.root, false)?;
    let before = serde_json::to_value(&store.job(&blocked.id.0)?)?;
    assert!(cancel(&paths, &blocked.id.0).is_err());
    let command = bootstrap_cancel_command(&paths, &blocked)?;
    let key = validation::job_bootstrap_key(&blocked)?;
    bootstrap_cancel_retain(
        &paths,
        &key,
        crate::broker::ResourceClass::Heavy,
        &command,
        Some(&bootstrap_cancel_result(0)),
    )?;
    assert!(cancel(&paths, &blocked.id.0).is_err());
    bootstrap_cancel_retain(
        &paths,
        &key,
        crate::broker::ResourceClass::Heavy,
        &command,
        None,
    )?;
    assert!(cancel(&paths, &blocked.id.0).is_err());
    assert_eq!(serde_json::to_value(&store.job(&blocked.id.0)?)?, before);
    assert!(
        !paths
            .job(&blocked.id.0)
            .join("validation-0.abandoned.json")
            .exists()
    );
    Ok(())
}

#[test]
fn failed_bootstrap_cancellation_refuses_any_dispatcher_admission() -> Result<()> {
    let (_temporary, paths, _repo, blocked) = bootstrap_cancel_fixture(false)?;
    let store = Store::open(&paths.root, false)?;
    let before = serde_json::to_value(&store.job(&blocked.id.0)?)?;
    bootstrap_cancel_retain(
        &paths,
        &validation::job_bootstrap_key(&blocked)?,
        crate::broker::ResourceClass::Heavy,
        &bootstrap_cancel_command(&paths, &blocked)?,
        Some(&bootstrap_cancel_result(101)),
    )?;
    let base = blocked.base.as_ref().context("base")?;
    let candidate = blocked.candidate.as_ref().context("candidate")?;
    let worktree = paths.job(&blocked.id.0).join("worktree");
    // Admission under another test selection still defeats the bootstrap-only proof.
    let key = validation::check_dispatcher_key(base, candidate, false, true);
    let dispatcher =
        validation::check_dispatcher_spec(&paths, &worktree, base, candidate, false, true);
    bootstrap_cancel_retain(
        &paths,
        &key,
        crate::broker::ResourceClass::Supervisor,
        &dispatcher,
        Some(&bootstrap_cancel_result(0)),
    )?;
    assert!(cancel(&paths, &blocked.id.0).is_err());
    assert_eq!(serde_json::to_value(&store.job(&blocked.id.0)?)?, before);
    assert!(
        !paths
            .job(&blocked.id.0)
            .join("validation-0.abandoned.json")
            .exists()
    );
    Ok(())
}

#[test]
fn failed_bootstrap_cancellation_keeps_resource_source_and_queue_guards() -> Result<()> {
    let (_temporary, paths, _repo, blocked) = bootstrap_cancel_fixture(false)?;
    let store = Store::open(&paths.root, false)?;
    let before = serde_json::to_value(&store.job(&blocked.id.0)?)?;
    bootstrap_cancel_retain(
        &paths,
        &validation::job_bootstrap_key(&blocked)?,
        crate::broker::ResourceClass::Heavy,
        &bootstrap_cancel_command(&paths, &blocked)?,
        Some(&bootstrap_cancel_result(101)),
    )?;
    let resource = store::lock(&paths.root.join("broker/slot-heavy-0.lock"), false)?;
    assert!(cancel(&paths, &blocked.id.0).is_err());
    drop(resource);
    let source = paths.job(&blocked.id.0).join("worktree/file.txt");
    fs::write(&source, "changed source\n")?;
    assert!(cancel(&paths, &blocked.id.0).is_err());
    fs::write(source, "base\n")?;
    let queued = job(
        &paths,
        blocked.base.as_ref().context("base")?,
        "another-queued",
    )?;
    store.insert(&queued)?;
    assert!(cancel(&paths, &blocked.id.0).is_err());
    assert_eq!(serde_json::to_value(&store.job(&blocked.id.0)?)?, before);
    assert!(
        !paths
            .job(&blocked.id.0)
            .join("validation-0.abandoned.json")
            .exists()
    );
    Ok(())
}

#[test]
fn failed_bootstrap_cancellation_without_settled_notice_keeps_validation_blocked() -> Result<()> {
    for uncertain in [false, true] {
        let (_temporary, paths, _repo, blocked) = bootstrap_cancel_fixture(false)?;
        let store = Store::open(&paths.root, false)?;
        bootstrap_cancel_retain(
            &paths,
            &validation::job_bootstrap_key(&blocked)?,
            crate::broker::ResourceClass::Heavy,
            &bootstrap_cancel_command(&paths, &blocked)?,
            Some(&bootstrap_cancel_result(101)),
        )?;
        let mut blocked = store.job(&blocked.id.0)?;
        let notice = blocked.notification.as_mut().context("failure notice")?;
        notice.accepted = uncertain;
        notice.uncertain = uncertain;
        store.save(&mut blocked)?;
        cancel(&paths, &blocked.id.0)?;
        let retained = store.job(&blocked.id.0)?;
        assert_eq!(retained.phase, Phase::Blocked);
        assert!(retained.unresolved && retained.operation.is_some());
        assert_eq!(retained.outcome, Some(Phase::Failed));
        assert_eq!(
            serde_json::to_value(&retained.notification)?,
            serde_json::to_value(&blocked.notification)?
        );
        assert!(
            !paths
                .job(&blocked.id.0)
                .join("validation-0.abandoned.json")
                .exists()
        );
    }
    Ok(())
}
