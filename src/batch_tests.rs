// Included in manager::tests; commands record their source and shared release.

fn batch_python(body: &str, env: Vec<(&str, PathBuf)>) -> crate::workflow::WorkflowCommand {
    crate::workflow::WorkflowCommand {
        program: "/usr/bin/python3".into(),
        args: vec!["-c".into(), body.into()],
        timeout_seconds: Some(20),
        env: env
            .into_iter()
            .map(|(name, path)| (name.into(), path.display().to_string()))
            .collect(),
    }
}

fn batch_workflow(project: &mut ProjectConfig, base: &CommitId) {
    project.workflow.check = batch_python(
        "import json,os; c=json.load(open(os.environ['TELETE_CONTEXT'])); open(os.environ['CHECKED'],'a').write(json.dumps({'job_id':c['job_id'],'commit':c['source']['commit']})+'\\n')",
        vec![("CHECKED", project.state.join("checked.jsonl"))],
    );
    project.workflow.batch = Some(crate::workflow::BatchConfig {
        target: "production".into(),
        base: base.0.clone(),
        compose: batch_python(
            r"import json,os,pathlib,subprocess
c=json.load(open(os.environ['TELETE_CONTEXT']))
b=c['inputs']['batch']
open(os.environ['COMPOSED'],'a').write(json.dumps(b)+'\n')
for member in b['members']:
    subprocess.run(['git','-c','user.name=Fixture','-c','user.email=fixture@localhost','merge','--no-edit','--no-ff',member['commit']],check=True)
commit=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
pathlib.Path(c['outputs']['directory'],'commit').write_text(commit+'\n')
",
            vec![("COMPOSED", project.state.join("composed.jsonl"))],
        ),
    });
    project.workflow.prepare = Some(batch_python(
        "import json,os,pathlib; c=json.load(open(os.environ['TELETE_CONTEXT'])); open(os.environ['PREPARED'],'a').write(c['source']['commit']+'\\n'); pathlib.Path(c['outputs']['directory'],'artifact').write_text(c['source']['commit'])",
        vec![("PREPARED", project.state.join("prepared"))],
    ));
    project.workflow.deploy = Some(batch_python(
        "import json,os,pathlib; c=json.load(open(os.environ['TELETE_CONTEXT'])); assert pathlib.Path(c['inputs']['preparation'],'artifact').read_text()==c['source']['commit']; open(os.environ['DEPLOYED'],'a').write(json.dumps({'context':c,'files':sorted(p.name for p in pathlib.Path('.').iterdir())})+'\\n')",
        vec![("DEPLOYED", project.state.join("deployed.jsonl"))],
    ));
}

fn batch_branch(
    paths: &Paths,
    project: &ProjectConfig,
    base: &CommitId,
    name: &str,
) -> Result<CommitId> {
    git::value(paths, &project.repo, &["checkout", "-b", name, &base.0])?;
    fs::write(
        project.repo.join(format!("{name}.txt")),
        format!("{name}\n"),
    )?;
    record_commit(paths, &project.repo)
}

fn batch_records(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_to_string(path)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn batch_finish(paths: &Paths, id: &str) -> Result<Value> {
    for _ in 0..48 {
        let value = status(paths, Some(id))?;
        if matches!(
            value["phase"].as_str(),
            Some("succeeded" | "failed" | "cancelled" | "blocked")
        ) {
            return Ok(value);
        }
        worker_once(paths)?;
    }
    bail!("batch fixture did not settle: {}", status(paths, Some(id))?)
}

fn batch_freeze(paths: &Paths, id: &str) -> Result<()> {
    for _ in 0..12 {
        if Store::open(&paths.root)?.job(id)?.batch.is_some() {
            return Ok(());
        }
        worker_once(paths)?;
    }
    bail!("batch fixture did not freeze")
}

fn batch_contains(
    paths: &Paths,
    repo: &Path,
    source: &CommitId,
    ancestor: &CommitId,
) -> Result<()> {
    git::value(
        paths,
        repo,
        &["merge-base", "--is-ancestor", &ancestor.0, &source.0],
    )?;
    Ok(())
}

#[test]
fn release_batch_checks_each_divergent_source_once_and_prepares_and_deploys_once() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    let b = batch_branch(&paths, &project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;

    let completed = batch_finish(&paths, &first)?;
    assert_eq!(completed["phase"], "succeeded");
    let other = batch_finish(&paths, &second)?;
    assert_eq!(other["phase"], "succeeded");
    assert_eq!(completed["resulting_commit"], a.0);
    assert_eq!(other["resulting_commit"], b.0);
    assert_eq!(completed["release_id"], first);
    assert_eq!(other["release_id"], first);
    assert_eq!(other["release_commit"], completed["release_commit"]);
    let release = CommitId::new(
        completed["release_commit"]
            .as_str()
            .context("release source")?,
    )?;
    batch_contains(&paths, &project.repo, &release, &a)?;
    batch_contains(&paths, &project.repo, &release, &b)?;
    let checks = batch_records(&paths.root.join("checked.jsonl"))?;
    assert_eq!(
        checks,
        vec![
            json!({"job_id":first,"commit":a.0}),
            json!({"job_id":second,"commit":b.0})
        ]
    );
    assert_eq!(batch_records(&paths.root.join("composed.jsonl"))?.len(), 1);
    assert_eq!(
        fs::read_to_string(paths.root.join("prepared"))?
            .lines()
            .count(),
        1
    );
    let deployments = batch_records(&paths.root.join("deployed.jsonl"))?;
    assert_eq!(deployments.len(), 1);
    assert_eq!(deployments[0]["context"]["source"]["commit"], release.0);
    let files = deployments[0]["files"]
        .as_array()
        .context("deployed files")?;
    assert!(files.contains(&json!("change-a.txt")));
    assert!(files.contains(&json!("change-b.txt")));
    assert_eq!(git::commit(&paths, &project.repo, "main")?, base);
    assert_eq!(
        git::commit(
            &paths,
            &project.repo,
            &format!("refs/telete/releases/{first}")
        )?,
        release
    );
    assert_eq!(
        git::commit(
            &paths,
            &project.repo,
            "refs/telete/releases/targets/production"
        )?,
        release
    );
    assert!(!paths.job(&first).exists());
    assert!(!paths.job(&second).exists());
    Ok(())
}

#[test]
fn release_batch_freezes_before_later_arrivals_and_next_release_is_cumulative() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    let b = batch_branch(&paths, &project, &base, "change-b")?;
    let c = batch_branch(&paths, &project, &base, "change-c")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;
    batch_freeze(&paths, &first)?;
    let (_, later) = enqueue(&project, &c.0)?;
    let released = batch_finish(&paths, &first)?;
    assert_eq!(released["phase"], "succeeded");
    assert_eq!(batch_finish(&paths, &second)?["release_id"], first);
    let next = batch_finish(&paths, &later)?;
    assert_eq!(next["phase"], "succeeded");
    assert_eq!(next["release_id"], later);
    let previous = CommitId::new(
        released["release_commit"]
            .as_str()
            .context("previous release")?,
    )?;
    let current = CommitId::new(next["release_commit"].as_str().context("next release")?)?;
    batch_contains(&paths, &project.repo, &current, &previous)?;
    batch_contains(&paths, &project.repo, &current, &c)?;
    let compositions = batch_records(&paths.root.join("composed.jsonl"))?;
    assert_eq!(compositions.len(), 2);
    assert_eq!(
        compositions[0]["members"]
            .as_array()
            .context("first members")?
            .len(),
        2
    );
    assert_eq!(
        compositions[1]["members"]
            .as_array()
            .context("next members")?
            .len(),
        1
    );
    assert_eq!(compositions[1]["base"], previous.0);
    assert_eq!(batch_records(&paths.root.join("checked.jsonl"))?.len(), 3);
    let deployments = batch_records(&paths.root.join("deployed.jsonl"))?;
    assert_eq!(deployments.len(), 2);
    let files = deployments[1]["files"]
        .as_array()
        .context("cumulative deployment")?;
    for name in ["change-a.txt", "change-b.txt", "change-c.txt"] {
        assert!(files.contains(&json!(name)));
    }
    Ok(())
}

#[test]
fn release_batch_does_not_mix_incompatible_deployment_commands_for_the_same_target() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    let b = batch_branch(&paths, &project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    project
        .workflow
        .deploy
        .as_mut()
        .context("deploy")?
        .env
        .insert("MODE".into(), "different frozen deployment".into());
    let (_, second) = enqueue(&project, &b.0)?;
    let released = batch_finish(&paths, &first)?;
    assert_eq!(released["phase"], "succeeded");
    let next = batch_finish(&paths, &second)?;
    assert_eq!(next["phase"], "succeeded");
    assert_eq!(released["release_id"], first);
    assert_eq!(next["release_id"], second);
    let compositions = batch_records(&paths.root.join("composed.jsonl"))?;
    assert_eq!(compositions.len(), 2);
    assert_eq!(
        compositions[0]["members"]
            .as_array()
            .context("first members")?
            .len(),
        1
    );
    assert_eq!(
        compositions[1]["members"]
            .as_array()
            .context("next members")?
            .len(),
        1
    );
    assert_eq!(compositions[1]["base"], released["release_commit"]);
    assert_eq!(batch_records(&paths.root.join("deployed.jsonl"))?.len(), 2);
    Ok(())
}

#[test]
fn release_batch_waits_for_member_repairs_while_unrelated_ci_runs() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    project.workflow.analyze = Some(batch_python(
        "import json,os,pathlib,sys; c=json.load(open(os.environ['TELETE_CONTEXT'])); open(os.environ['ANALYZED'],'a').write(c['job_id']+'\\n'); sys.exit(1 if pathlib.Path('needs-repair').exists() else 0)",
        vec![("ANALYZED", project.state.join("analyzed"))],
    ));
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    batch_branch(&paths, &project, &base, "change-b")?;
    fs::write(project.repo.join("needs-repair"), "findings\n")?;
    let b = record_commit(&paths, &project.repo)?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;
    for _ in 0..20 {
        if status(&paths, Some(&second))?["phase"] == "awaiting_repair" {
            break;
        }
        worker_once(&paths)?;
    }
    assert_eq!(status(&paths, Some(&second))?["phase"], "awaiting_repair");
    assert_eq!(status(&paths, Some(&first))?["phase"], "waiting_release");
    assert!(batch_records(&paths.root.join("deployed.jsonl"))?.is_empty());
    assert!(Store::open(&paths.root)?.active()?.is_none());
    let mut other = project.workflow.clone();
    other.batch = None;
    other.analyze = None;
    other.prepare = None;
    other.deploy = None;
    let unrelated = submit(&paths, &project.repo, &base.0, other)?;
    let unrelated_id = unrelated["id"].as_str().context("unrelated job")?;
    assert_eq!(batch_finish(&paths, unrelated_id)?["phase"], "succeeded");
    assert!(batch_records(&paths.root.join("deployed.jsonl"))?.is_empty());
    fs::remove_file(project.repo.join("needs-repair"))?;
    fs::write(project.repo.join("repaired.txt"), "accepted repair\n")?;
    let repaired = record_commit(&paths, &project.repo)?;
    let store = Store::open(&paths.root)?;
    let mut interrupted = store.job(&second)?;
    interrupted.repaired_commit = Some(repaired.clone());
    store.save(&mut interrupted)?;
    assert!(cancel(&paths, &first).is_err());
    assert!(!store.job(&first)?.cancel_requested);
    assert!(!store.job(&second)?.cancel_requested);
    repair(&paths, &project.repo, &second, &repaired.0)?;
    let released = batch_finish(&paths, &first)?;
    assert_eq!(released["phase"], "succeeded");
    let repaired_job = batch_finish(&paths, &second)?;
    assert_eq!(repaired_job["phase"], "succeeded");
    assert_eq!(repaired_job["resulting_commit"], repaired.0);
    assert_eq!(repaired_job["analyzed_commit"], b.0);
    let release = CommitId::new(
        released["release_commit"]
            .as_str()
            .context("repaired release")?,
    )?;
    batch_contains(&paths, &project.repo, &release, &a)?;
    batch_contains(&paths, &project.repo, &release, &repaired)?;
    assert_eq!(
        fs::read_to_string(paths.root.join("analyzed"))?
            .lines()
            .count(),
        2
    );
    let compositions = batch_records(&paths.root.join("composed.jsonl"))?;
    assert_eq!(compositions.len(), 1);
    let members = compositions[0]["members"]
        .as_array()
        .context("repaired members")?;
    assert!(
        members
            .iter()
            .any(|member| member["job_id"] == second && member["commit"] == repaired.0)
    );
    assert_eq!(batch_records(&paths.root.join("deployed.jsonl"))?.len(), 1);
    Ok(())
}

#[test]
fn release_batch_retains_a_members_failed_check_until_a_newer_workflow_finishes() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    let b = batch_branch(&paths, &project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    project.workflow.check = crate::workflow::WorkflowCommand {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "printf 'build output'; printf 'member check failed' >&2; exit 7".into(),
        ],
        timeout_seconds: Some(10),
        env: BTreeMap::new(),
    };
    let (_, failed_id) = enqueue(&project, &b.0)?;
    assert_eq!(batch_finish(&paths, &first)?["phase"], "failed");
    let failed = batch_finish(&paths, &failed_id)?;
    assert_eq!(failed["phase"], "failed");
    assert_eq!(failed["release_id"], first);
    assert_eq!(failed["runs"][0]["stage"], "check");
    assert_eq!(failed["runs"][0]["commit"], b.0);
    assert_eq!(failed["runs"][0]["result"]["exit_code"], 7);
    assert_eq!(failed["runs"][0]["result"]["stdout"], "build output");
    assert_eq!(failed["runs"][0]["result"]["stderr"], "member check failed");
    assert!(
        paths
            .job(&failed_id)
            .join("worktree/change-b.txt")
            .is_file()
    );
    assert!(
        fs::read_to_string(paths.job(&failed_id).join("workflow-check-0.log"))?
            .contains("member check failed")
    );
    assert!(batch_records(&paths.root.join("composed.jsonl"))?.is_empty());
    assert!(batch_records(&paths.root.join("deployed.jsonl"))?.is_empty());
    worker_once(&paths)?;
    assert!(!needs_worker(&paths)?);
    assert!(paths.job(&first).is_dir());
    assert!(paths.job(&failed_id).is_dir());

    project.workflow.batch = None;
    project.workflow.prepare = None;
    project.workflow.deploy = None;
    project.workflow.check.program = "/usr/bin/true".into();
    project.workflow.check.args.clear();
    let (_, unrelated) = enqueue(&project, &base.0)?;
    assert_eq!(batch_finish(&paths, &unrelated)?["phase"], "succeeded");
    assert!(!paths.job(&first).exists());
    assert!(!paths.job(&failed_id).exists());
    assert!(status(&paths, Some(&failed_id))?.get("runs").is_none());
    Ok(())
}

#[test]
fn release_batch_composition_failure_never_prepares_or_deploys() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    project
        .workflow
        .batch
        .as_mut()
        .context("batch")?
        .compose
        .args = vec!["-c".into(), "raise SystemExit(7)".into()];
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    let b = batch_branch(&paths, &project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;
    assert_eq!(batch_finish(&paths, &first)?["phase"], "failed");
    assert_eq!(batch_finish(&paths, &second)?["phase"], "failed");
    assert_eq!(batch_records(&paths.root.join("checked.jsonl"))?.len(), 2);
    assert!(!paths.root.join("prepared").exists());
    assert!(batch_records(&paths.root.join("deployed.jsonl"))?.is_empty());
    assert!(
        git::ref_value(
            &paths,
            &project.repo,
            "refs/telete/releases/targets/production"
        )?
        .is_none()
    );
    worker_once(&paths)?;
    let mut unrelated = project.workflow.clone();
    unrelated.batch = None;
    unrelated.prepare = None;
    unrelated.deploy = None;
    let submitted = submit(&paths, &project.repo, &base.0, unrelated)?;
    let unrelated_id = submitted["id"].as_str().context("unrelated CI")?;
    assert_eq!(batch_finish(&paths, unrelated_id)?["phase"], "succeeded");
    Ok(())
}

#[test]
fn release_batch_rejects_a_successful_composer_that_omits_checked_members() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    project.workflow.batch.as_mut().context("batch")?.compose.args = vec![
        "-c".into(),
        "import json,os,pathlib,subprocess; c=json.load(open(os.environ['TELETE_CONTEXT'])); commit=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(); pathlib.Path(c['outputs']['directory'],'commit').write_text(commit+'\\n')".into(),
    ];
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    let b = batch_branch(&paths, &project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;
    let failed = batch_finish(&paths, &first)?;
    assert_eq!(failed["phase"], "failed");
    assert!(
        failed["last_error"]
            .as_str()
            .context("composition rejection")?
            .contains("must include source")
    );
    assert_eq!(batch_finish(&paths, &second)?["phase"], "failed");
    assert_eq!(batch_records(&paths.root.join("checked.jsonl"))?.len(), 2);
    assert!(!paths.root.join("prepared").exists());
    assert!(batch_records(&paths.root.join("deployed.jsonl"))?.is_empty());
    assert!(
        git::ref_value(
            &paths,
            &project.repo,
            &format!("refs/telete/releases/{first}")
        )?
        .is_none()
    );
    Ok(())
}

#[test]
fn release_batch_never_replays_a_composition_without_terminal_evidence() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    for _ in 0..16 {
        if status(&paths, Some(&first))?["phase"] == "preparing" {
            break;
        }
        worker_once(&paths)?;
    }
    assert_eq!(status(&paths, Some(&first))?["phase"], "preparing");
    let store = Store::open(&paths.root)?;
    let mut job = store.job(&first)?;
    let run = job
        .runs
        .iter()
        .find(|run| run.stage == "compose")
        .context("completed composition")?
        .clone();
    let candidate = job.release_commit.clone().context("retained release")?;
    let receipt = paths.job(&first).join(format!("{}.json", run.operation));
    let gate = paths
        .root
        .join("broker")
        .join(format!("{first}-{}.json", run.operation));
    let completion_path = gate.with_extension("process").join("result.json");
    let completion = fs::read(&completion_path)?;
    fs::remove_file(&completion_path)?;
    fs::remove_file(&receipt)?;
    let mut execution: Value = serde_json::from_slice(&fs::read(&gate)?)?;
    execution["state"] = json!("running");
    execution["result"] = Value::Null;
    store::atomic_json(&gate, &execution)?;
    job.phase = Phase::Blocked;
    job.stopped_phase = Some(Phase::Composing);
    job.unresolved = true;
    job.outcome = None;
    job.release_commit = None;
    job.batch.as_mut().context("closed release")?.commit = None;
    job.operation = Some(Operation {
        phase: Phase::Composing,
        name: run.operation,
        commit: run.commit,
        result: receipt,
        started: run.stamp,
    });
    job.runs.retain(|run| run.stage != "compose");
    store.save(&mut job)?;
    assert_eq!(worker_once(&paths)?["phase"], "blocked");
    assert!(recover(&paths, &first).is_err());
    assert!(resume(&paths).is_err());
    assert_eq!(worker_once(&paths)?["phase"], "blocked");
    assert_eq!(batch_records(&paths.root.join("composed.jsonl"))?.len(), 1);
    assert!(batch_records(&paths.root.join("deployed.jsonl"))?.is_empty());
    assert!(paths.job(&first).join("worktree").is_dir());
    store::atomic_bytes(&completion_path, &completion)?;
    assert_eq!(recover(&paths, &first)?["phase"], "preparing");
    assert_eq!(store.job(&first)?.release_commit, Some(candidate));
    assert_eq!(batch_records(&paths.root.join("composed.jsonl"))?.len(), 1);
    assert_eq!(batch_finish(&paths, &first)?["phase"], "succeeded");
    assert_eq!(batch_records(&paths.root.join("deployed.jsonl"))?.len(), 1);
    Ok(())
}

#[test]
fn release_batch_records_a_successful_deployment_even_when_cancelled_during_the_command()
-> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let normal_deploy = project.workflow.deploy.clone().context("deploy")?;
    let cancelling_deploy = project.workflow.deploy.as_mut().context("deploy")?;
    cancelling_deploy.args[1] = format!(
        "import sqlite3; {}; db=sqlite3.connect(os.path.join(os.environ['STATE'],'queue.sqlite3')); db.execute('UPDATE jobs SET cancel_requested=1 WHERE id=?',(c['job_id'],)); db.commit(); db.close()",
        cancelling_deploy.args[1]
    );
    cancelling_deploy
        .env
        .insert("STATE".into(), project.state.display().to_string());
    let paths = Paths::selected(project.state.clone(), false)?;
    let a = batch_branch(&paths, &project, &base, "change-a")?;
    let b = batch_branch(&paths, &project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let cancelled = batch_finish(&paths, &first)?;
    assert_eq!(cancelled["phase"], "cancelled");
    let released = CommitId::new(
        cancelled["release_commit"]
            .as_str()
            .context("deployed source")?,
    )?;
    assert_eq!(
        git::commit(
            &paths,
            &project.repo,
            "refs/telete/releases/targets/production"
        )?,
        released
    );
    assert_eq!(batch_records(&paths.root.join("deployed.jsonl"))?.len(), 1);

    project.workflow.deploy = Some(normal_deploy);
    let (_, next) = enqueue(&project, &b.0)?;
    let completed = batch_finish(&paths, &next)?;
    assert_eq!(completed["phase"], "succeeded");
    let current = CommitId::new(
        completed["release_commit"]
            .as_str()
            .context("next source")?,
    )?;
    batch_contains(&paths, &project.repo, &current, &released)?;
    batch_contains(&paths, &project.repo, &current, &b)?;
    let compositions = batch_records(&paths.root.join("composed.jsonl"))?;
    assert_eq!(compositions.len(), 2);
    assert_eq!(compositions[1]["base"], released.0);
    assert_eq!(batch_records(&paths.root.join("deployed.jsonl"))?.len(), 2);
    Ok(())
}
