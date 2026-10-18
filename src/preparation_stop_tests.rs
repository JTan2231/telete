// Exercise the stop guard without stopping a service or settling preparation.

fn preparation_stop_fixture() -> Result<(tempfile::TempDir, Paths, PathBuf, Job)> {
    let (temporary, paths, repo, base) = fixture()?;
    let store = Store::open(&paths.root, false)?;
    let mut blocked = job(&paths, &base, "stop-preparation")?;
    blocked.base = Some(base.clone());
    blocked.candidate = Some(base.clone());
    blocked.phase = Phase::Blocked;
    blocked.stopped_phase = Some(Phase::Preparing);
    blocked.outcome = Some(Phase::Failed);
    blocked.outcome_message = Some("retained preparation failure".into());
    blocked.last_error = blocked.outcome_message.clone();
    blocked.unresolved = true;
    blocked.preparation_generation = 2;
    let directory = paths.job(&blocked.id.0);
    let worktree = directory.join("worktree");
    git::ensure_worktree(&paths, &repo, &worktree, &base)?;
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
        phase: Phase::Preparing,
        candidate: base,
        name: "production-2".into(),
        result: directory.join("production-2.json"),
        started: 1_700_000_002,
    });
    store::atomic_bytes(
        &directory.join("production-2/partial.log"),
        b"retained partial staging\n",
    )?;
    let blocked = store.insert(&blocked)?;
    Ok((temporary, paths, repo, blocked))
}

fn preparation_stop_rejects_job(
    paths: &Paths,
    original: &Job,
    mut changed: Job,
    label: &str,
) -> Result<()> {
    let store = Store::open(&paths.root, false)?;
    changed.revision = store.job(&original.id.0)?.revision;
    store.save(&mut changed)?;
    let before = serde_json::to_value(&store.job(&original.id.0)?)?;
    assert!(require_service_stoppable(paths).is_err(), "{label}");
    assert_eq!(
        serde_json::to_value(&store.job(&original.id.0)?)?,
        before,
        "{label}"
    );
    let mut restored = original.clone();
    restored.revision = changed.revision;
    store.save(&mut restored)?;
    Ok(())
}

#[test]
fn blocked_preparation_service_stop_preserves_journal_and_maintenance_fence() -> Result<()> {
    let (_temporary, paths, repo, blocked) = preparation_stop_fixture()?;
    let store = Store::open(&paths.root, false)?;
    let config = store.config()?;
    let base = blocked.base.as_ref().context("base")?;
    let candidate = blocked.candidate.as_ref().context("candidate")?;
    let directory = paths.job(&blocked.id.0);
    // Match the ownership already held by installation::stop.
    let _resources = crate::broker::settled_guards(&paths)?;
    let _deployment = store::lock(&paths.root.join("deployments/deployment.lock"), false)?;
    let before = serde_json::to_value(&store.job(&blocked.id.0)?)?;
    assert!(require_quiescent(&paths).is_err());
    require_service_stoppable(&paths)?;
    require_service_stoppable(&paths)?;
    assert_eq!(serde_json::to_value(&store.job(&blocked.id.0)?)?, before);
    assert_eq!(store.config()?, config);
    assert!(store.paused()?);
    assert!(require_quiescent(&paths).is_err());
    assert_eq!(
        fs::read(directory.join("production-2/partial.log"))?,
        b"retained partial staging\n"
    );
    assert!(!directory.join("production-2.json").exists());
    assert!(!directory.join("production-2/result.json").exists());
    assert!(!directory.join("production-2.abandoned.json").exists());
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
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn blocked_preparation_stop_rejects_changed_job_or_operation_authority() -> Result<()> {
    let (_temporary, paths, _repo, blocked) = preparation_stop_fixture()?;
    let original = serde_json::to_value(&blocked)?;
    for (field, value) in [
        ("phase", json!("preparing")),
        ("stopped_phase", json!("checking")),
        ("outcome", json!("succeeded")),
        ("unresolved", json!(false)),
        ("accepted", json!(true)),
        ("base", Value::Null),
        ("candidate", Value::Null),
        ("operation", Value::Null),
        ("preparation_generation", json!(3)),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        preparation_stop_rejects_job(&paths, &blocked, serde_json::from_value(changed)?, field)?;
    }
    for (field, value) in [
        ("phase", json!("checking")),
        ("name", json!("production-99")),
        ("candidate", json!("b".repeat(40))),
        ("result", json!(paths.root.join("foreign-preparation.json"))),
    ] {
        let mut changed = original.clone();
        changed["operation"][field] = value;
        preparation_stop_rejects_job(&paths, &blocked, serde_json::from_value(changed)?, field)?;
    }
    let mut changed = original.clone();
    changed["options"]["workflow"] = json!({"check":{"program":"check.sh"}});
    preparation_stop_rejects_job(
        &paths,
        &blocked,
        serde_json::from_value(changed)?,
        "generic workflow",
    )?;
    let preparation = Preparation {
        schema: 1,
        source: blocked.candidate.clone().context("candidate")?,
        products: Vec::new(),
        signing_policy: blocked.policy.signing()?.clone(),
        candidates: BTreeMap::new(),
        release_check: true,
    };
    let mut changed = blocked.clone();
    changed.preparation = Some(preparation.clone());
    preparation_stop_rejects_job(&paths, &blocked, changed, "prepared record")?;
    let mut changed = blocked.clone();
    changed.deployment_request = Some(DeploymentRequest {
        id: "unsettled-deployment".into(),
        source: preparation.source.clone(),
        products: Vec::new(),
        prepared: preparation,
    });
    preparation_stop_rejects_job(&paths, &blocked, changed, "deployment request")?;
    let store = Store::open(&paths.root, false)?;
    let mut config = store.config()?;
    config.project = true;
    store.set("config", &config)?;
    assert!(require_service_stoppable(&paths).is_err());
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn blocked_preparation_stop_rejects_unsettled_queue_source_or_deployment() -> Result<()> {
    let (_temporary, paths, repo, blocked) = preparation_stop_fixture()?;
    let store = Store::open(&paths.root, false)?;
    let before = serde_json::to_value(&blocked)?;
    store.set("paused", &false)?;
    assert!(require_service_stoppable(&paths).is_err());
    store.set("paused", &true)?;
    let worktree = paths.job(&blocked.id.0).join("worktree");
    fs::write(worktree.join("file.txt"), "changed candidate\n")?;
    assert!(require_service_stoppable(&paths).is_err());
    fs::write(worktree.join("file.txt"), "base\n")?;
    let base = blocked.base.as_ref().context("base")?;
    let tree = git::value(&paths, &repo, &["rev-parse", &format!("{base}^{{tree}}")])?;
    let other = git::commit_tree(
        &paths,
        &repo,
        &tree,
        std::slice::from_ref(base),
        "stop-conflict",
        1_700_000_010,
        "Competing source",
    )?;
    for reference in [
        git::ACCEPTED.to_owned(),
        git::private_ref(&blocked.id.0, "candidate")?,
    ] {
        git::advance(&paths, &repo, &reference, Some(base), &other)?;
        assert!(require_service_stoppable(&paths).is_err());
        git::advance(&paths, &repo, &reference, Some(&other), base)?;
    }
    let active = paths.root.join("deployments/active.json");
    store::atomic_json(&active, &json!({"request":"active-deployment"}))?;
    assert!(require_service_stoppable(&paths).is_err());
    fs::remove_file(&active)?;
    std::os::unix::fs::symlink(paths.root.join("missing-deployment"), &active)?;
    assert!(!active.exists());
    assert!(require_service_stoppable(&paths).is_err());
    fs::remove_file(active)?;
    let queued = job(&paths, base, "queued-during-preparation")?;
    let mut queued = store.insert(&queued)?;
    assert!(require_service_stoppable(&paths).is_err());
    queued.phase = Phase::Failed;
    queued.outcome = Some(Phase::Failed);
    queued.unresolved = true;
    store.save(&mut queued)?;
    assert!(require_service_stoppable(&paths).is_err());
    queued.unresolved = false;
    store.save(&mut queued)?;
    require_service_stoppable(&paths)?;
    assert_eq!(serde_json::to_value(&store.job(&blocked.id.0)?)?, before);

    // A clean directory is insufficient if its Git identity was replaced.
    fs::remove_dir_all(&worktree)?;
    fs::create_dir(&worktree)?;
    git::value(&paths, &worktree, &["init", "--initial-branch=main"])?;
    assert!(require_service_stoppable(&paths).is_err());
    assert_eq!(serde_json::to_value(&store.job(&blocked.id.0)?)?, before);
    Ok(())
}

#[test]
fn blocked_preparation_stop_rejects_final_receipts_and_dangling_receipt_links() -> Result<()> {
    let (_temporary, paths, _repo, blocked) = preparation_stop_fixture()?;
    let store = Store::open(&paths.root, false)?;
    let before = serde_json::to_value(&blocked)?;
    let directory = paths.job(&blocked.id.0);
    for receipt in [
        directory.join("production-2.json"),
        directory.join("production-2/result.json"),
    ] {
        store::atomic_json(&receipt, &json!({"retained":"terminal-receipt"}))?;
        assert!(require_service_stoppable(&paths).is_err());
        fs::remove_file(&receipt)?;
        std::os::unix::fs::symlink(directory.join("missing-receipt-target"), &receipt)?;
        assert!(!receipt.exists());
        assert!(fs::symlink_metadata(&receipt)?.file_type().is_symlink());
        assert!(require_service_stoppable(&paths).is_err());
        fs::remove_file(receipt)?;
    }
    require_service_stoppable(&paths)?;
    assert_eq!(serde_json::to_value(&store.job(&blocked.id.0)?)?, before);
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn frozen_preparation_stop_requires_exact_ordinary_failed_outer_completion() -> Result<()> {
    let (_temporary, paths, _repo, blocked) = preparation_stop_fixture()?;
    let store = Store::open(&paths.root, false)?;
    let mut blocked = store.job(&blocked.id.0)?;
    blocked.policy.commands = Some(CommandPolicy::capture(&paths)?);
    store.save(&mut blocked)?;
    let before = serde_json::to_value(&blocked)?;
    let command = cell_pipeline::preparation_command(&paths, &blocked)?;
    let directory = crate::invocation::directory(&paths, &blocked, "production-2")?;
    let request = directory.join("request.json");
    let terminal = directory.join("result.json");
    assert!(require_service_stoppable(&paths).is_err());
    store::atomic_json(&request, &command)?;
    store::atomic_json(
        &directory.join("intent.json"),
        &json!({"schema":1,"owner_pid":1}),
    )?;
    if let Some(context) = &command.context {
        store::atomic_json(&context.path, &context.value)?;
    }
    assert!(require_service_stoppable(&paths).is_err());
    let failed = crate::process::ProcessResult {
        exit_code: Some(101),
        stdout: "retained preparation output\n".into(),
        stderr: "retained staging failure\n".into(),
        timed_out: false,
        start_error: None,
        output_error: None,
        signal: None,
    };
    for result in [
        crate::process::ProcessResult {
            exit_code: Some(0),
            ..failed.clone()
        },
        crate::process::ProcessResult {
            exit_code: Some(-1),
            ..failed.clone()
        },
        crate::process::ProcessResult {
            exit_code: None,
            ..failed.clone()
        },
        crate::process::ProcessResult {
            timed_out: true,
            ..failed.clone()
        },
        crate::process::ProcessResult {
            signal: Some(9),
            ..failed.clone()
        },
        crate::process::ProcessResult {
            start_error: Some("start failed".into()),
            ..failed.clone()
        },
        crate::process::ProcessResult {
            output_error: Some("capture failed".into()),
            ..failed.clone()
        },
    ] {
        store::atomic_json(
            &terminal,
            &json!({"schema":1,"request":command,"result":result}),
        )?;
        assert!(require_service_stoppable(&paths).is_err());
    }
    let mut changed = command.clone();
    changed.args.push("another-operation".into());
    store::atomic_json(&request, &changed)?;
    store::atomic_json(
        &terminal,
        &json!({"schema":1,"request":changed,"result":failed}),
    )?;
    assert!(require_service_stoppable(&paths).is_err());
    store::atomic_json(&request, &command)?;
    // A terminal result for a different invocation is still uncorrelated.
    assert!(require_service_stoppable(&paths).is_err());
    let mut changed = command.clone();
    changed
        .context
        .as_mut()
        .context("frozen preparation context")?
        .value["source"]["candidate"] = json!("b".repeat(40));
    store::atomic_json(&request, &changed)?;
    store::atomic_json(
        &terminal,
        &json!({"schema":1,"request":changed,"result":failed}),
    )?;
    assert!(require_service_stoppable(&paths).is_err());

    store::atomic_json(&request, &command)?;
    store::atomic_json(
        &terminal,
        &json!({"schema":1,"request":command,"result":failed}),
    )?;
    let request_bytes = fs::read(&request)?;
    let terminal_bytes = fs::read(&terminal)?;
    require_service_stoppable(&paths)?;
    assert!(require_quiescent(&paths).is_err());
    assert_eq!(fs::read(request)?, request_bytes);
    assert_eq!(fs::read(terminal)?, terminal_bytes);
    assert_eq!(serde_json::to_value(&store.job(&blocked.id.0)?)?, before);
    assert!(!paths.job(&blocked.id.0).join("production-2.json").exists());
    assert!(
        !paths
            .job(&blocked.id.0)
            .join("production-2/result.json")
            .exists()
    );
    Ok(())
}
