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
        compose: fixture_command(
            "compose",
            [(
                "COMPOSED".into(),
                project.state.join("composed.jsonl").display().to_string(),
            )],
        ),
        merge_repair: None,
        check: None,
        repair: None,
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

fn batch_branch(project: &ProjectConfig, base: &CommitId, name: &str) -> Result<CommitId> {
    crate::test_support::branch(&project.repo, name, &base.0)?;
    crate::test_support::switch(&project.repo, name)?;
    fs::write(
        project.repo.join(format!("{name}.txt")),
        format!("{name}\n"),
    )?;
    record_commit(&project.repo)
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

fn batch_contains(repo: &Path, source: &CommitId, ancestor: &CommitId) -> Result<()> {
    let repo = git2::Repository::open(repo)?;
    ensure!(crate::test_support::contains(
        &repo,
        git2::Oid::from_str(&ancestor.0)?,
        git2::Oid::from_str(&source.0)?,
    )?);
    Ok(())
}

#[test]
fn release_batch_freezes_after_preliminary_ci_and_repairs_the_combined_tree_once() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    project.workflow.analyze = Some(batch_python(
        "import pathlib,sys; sys.exit(1 if pathlib.Path('change-a.txt').exists() else 0)",
        vec![],
    ));
    project.workflow.repair = Some(batch_python(
        "raise AssertionError('preliminary CI must not repair a member')",
        vec![],
    ));
    let batch = project.workflow.batch.as_mut().context("batch")?;
    batch.check = Some(batch_python(
        r"
import json, os, pathlib, sys
context = json.load(open(os.environ['TELETE_CONTEXT']))
assert context['stage'] == 'release_check'
with open(os.environ['CANDIDATES'], 'a') as log:
    log.write(json.dumps(context) + '\n')
if pathlib.Path('change-a.txt').exists() and pathlib.Path('change-b.txt').exists():
    if pathlib.Path('payload.txt').read_text() != 'one shared repair\n':
        sys.stdout.write('stdout')
        sys.stderr.write('stderr')
        sys.exit(1)
",
        vec![("CANDIDATES", project.state.join("candidates.jsonl"))],
    ));
    batch.repair = Some(fixture_command(
        "repair",
        [
            (
                "ATTEMPTS".into(),
                project.state.join("attempts").display().to_string(),
            ),
            (
                "DIAGNOSTICS".into(),
                project.state.join("diagnostics").display().to_string(),
            ),
            ("REPLACEMENT".into(), "one shared repair".into()),
        ],
    ));
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
    let c = batch_branch(&project, &base, "change-c")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;
    batch_freeze(&paths, &first)?;
    let (_, later) = enqueue(&project, &c.0)?;

    let completed = batch_finish(&paths, &first)?;
    assert_eq!(completed["phase"], "succeeded", "{completed}");
    let released = CommitId::new(
        completed["release_commit"]
            .as_str()
            .context("release SHA")?,
    )?;
    let checks = batch_records(&paths.root.join("candidates.jsonl"))?;
    assert_eq!(checks.len(), 2);
    let composed = CommitId::new(
        checks[0]["source"]["commit"]
            .as_str()
            .context("composed SHA")?,
    )?;
    assert_ne!(composed, released);
    batch_contains(&project.repo, &released, &composed)?;
    assert_eq!(checks[1]["source"]["commit"], released.0);
    for check in checks {
        let members = check["inputs"]["batch"]["members"]
            .as_array()
            .context("batch members")?;
        assert_eq!(members.len(), 2);
        assert_eq!(members[0]["commit"], a.0);
        assert_eq!(members[1]["commit"], b.0);
    }
    assert_eq!(fs::read_to_string(paths.root.join("attempts"))?, "repair");
    assert_eq!(
        fs::read_to_string(paths.root.join("diagnostics"))?,
        "stdoutstderr"
    );
    assert!(batch_records(&paths.root.join("checked.jsonl"))?.is_empty());
    assert_eq!(batch_finish(&paths, &second)?["release_commit"], released.0);
    assert_eq!(status(&paths, Some(&later))?["phase"], "queued");
    assert_eq!(
        fs::read_to_string(paths.root.join("prepared"))?.trim(),
        released.0
    );
    let deployments = batch_records(&paths.root.join("deployed.jsonl"))?;
    assert_eq!(deployments.len(), 1);
    assert_eq!(deployments[0]["context"]["source"]["commit"], released.0);
    Ok(())
}

#[test]
fn release_batch_reports_one_failure_and_admits_one_retry_from_either_member() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let allow_prepare = project.state.join("allow-prepare");
    project.workflow.prepare = Some(batch_python(
        r"
import json, os, pathlib, sys
if not pathlib.Path(os.environ['ALLOW_PREPARE']).exists():
    sys.exit('shared archive failure')
context = json.load(open(os.environ['TELETE_CONTEXT']))
pathlib.Path(context['outputs']['directory'], 'artifact').write_text(context['source']['commit'])
",
        vec![("ALLOW_PREPARE", allow_prepare.clone())],
    ));
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;
    batch_freeze(&paths, &first)?;
    let waiting = status(&paths, Some(&first))?["release"].clone();
    assert_eq!(waiting, status(&paths, Some(&second))?["release"]);
    assert_eq!(waiting["members"], json!([first, second]));
    assert_eq!(waiting["blocker"]["kind"], "member");
    assert_eq!(waiting["blocker"]["job"], second);

    let failed = batch_finish(&paths, &first)?;
    assert_eq!(failed["phase"], "failed");
    let failure = &failed["release"];
    assert_eq!(*failure, status(&paths, Some(&second))?["release"]);
    assert_eq!(failure["candidate_commit"], failed["release_commit"]);
    assert_eq!(failure["failure"]["stage"], "prepare");
    let path = failure["failure"]["diagnostic_path"]
        .as_str()
        .context("retained shared log")?;
    assert!(fs::read_to_string(path)?.contains("shared archive failure"));

    let mut unrelated = project.workflow.clone();
    unrelated.batch = None;
    unrelated.prepare = None;
    unrelated.deploy = None;
    let submitted = submit(&paths, &project.repo, &base.0, unrelated, project.queue)?;
    let unrelated_id = submitted["id"].as_str().context("unrelated job")?;
    assert_eq!(batch_finish(&paths, unrelated_id)?["phase"], "succeeded");
    let retired = status(&paths, Some(&second))?["release"].clone();
    assert_eq!(retired, status(&paths, Some(&first))?["release"]);
    assert_eq!(retired["failure"]["stage"], "prepare");
    assert_eq!(retired["failure"]["reason"], failure["failure"]["reason"]);
    assert!(retired["failure"]["diagnostic_path"].is_null());
    assert_eq!(retired["next_action"], "retry");

    fs::write(&allow_prepare, "")?;
    let (left, right) = std::thread::scope(|scope| -> Result<_> {
        let left = scope.spawn(|| retry(&paths, &first));
        let right = retry(&paths, &second);
        Ok((left.join().expect("retry thread panicked")?, right?))
    })?;
    assert_eq!(left, right);
    assert_eq!(left["retry_of"], first);
    assert_eq!(
        left["members"].as_array().context("retry members")?.len(),
        2
    );
    assert_eq!(left["members"][0]["retry_of"], first);
    assert_eq!(left["members"][1]["retry_of"], second);
    let retried = left["id"].as_str().context("retry release")?;
    assert_eq!(
        status(&paths, Some(&second))?["release"]["retry_id"],
        retried
    );
    let settled = batch_finish(&paths, retried)?;
    assert_eq!(settled["phase"], "succeeded", "{settled}");
    let release = CommitId::new(settled["release_commit"].as_str().context("retry SHA")?)?;
    batch_contains(&project.repo, &release, &a)?;
    batch_contains(&project.repo, &release, &b)?;
    assert_eq!(batch_records(&paths.root.join("deployed.jsonl"))?.len(), 1);
    Ok(())
}

#[test]
fn release_batch_checks_each_divergent_source_once_and_prepares_and_deploys_once() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
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
    batch_contains(&project.repo, &release, &a)?;
    batch_contains(&project.repo, &release, &b)?;
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
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
    let c = batch_branch(&project, &base, "change-c")?;
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
    batch_contains(&project.repo, &current, &previous)?;
    batch_contains(&project.repo, &current, &c)?;
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
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
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
    let a = batch_branch(&project, &base, "change-a")?;
    batch_branch(&project, &base, "change-b")?;
    fs::write(project.repo.join("needs-repair"), "findings\n")?;
    let b = record_commit(&project.repo)?;
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
    let unrelated = submit(&paths, &project.repo, &base.0, other, project.queue)?;
    let unrelated_id = unrelated["id"].as_str().context("unrelated job")?;
    assert_eq!(batch_finish(&paths, unrelated_id)?["phase"], "succeeded");
    assert!(batch_records(&paths.root.join("deployed.jsonl"))?.is_empty());
    fs::remove_file(project.repo.join("needs-repair"))?;
    fs::write(project.repo.join("repaired.txt"), "accepted repair\n")?;
    let repaired = record_commit(&project.repo)?;
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
    batch_contains(&project.repo, &release, &a)?;
    batch_contains(&project.repo, &release, &repaired)?;
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
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
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
    let batch = project.workflow.batch.as_mut().context("batch")?;
    batch.compose = batch_python("raise SystemExit(7)", vec![]);
    batch.merge_repair = Some(batch_python(
        "import os,pathlib; pathlib.Path(os.environ['REPAIRED']).touch()",
        vec![("REPAIRED", project.state.join("repaired"))],
    ));
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;
    assert_eq!(batch_finish(&paths, &first)?["phase"], "failed");
    assert_eq!(batch_finish(&paths, &second)?["phase"], "failed");
    assert_eq!(batch_records(&paths.root.join("checked.jsonl"))?.len(), 2);
    assert!(!paths.root.join("repaired").exists());
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
    let submitted = submit(&paths, &project.repo, &base.0, unrelated, project.queue)?;
    let unrelated_id = submitted["id"].as_str().context("unrelated CI")?;
    assert_eq!(batch_finish(&paths, unrelated_id)?["phase"], "succeeded");
    Ok(())
}

#[test]
fn release_batch_repairs_each_conflict_and_reuses_member_checks() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    project
        .workflow
        .batch
        .as_mut()
        .context("batch")?
        .merge_repair = Some(fixture_command(
        "merge_repair",
        [(
            "REPAIRED".into(),
            project.state.join("repaired.jsonl").display().to_string(),
        )],
    ));
    let paths = Paths::selected(project.state.clone(), false)?;
    let mut sources = Vec::new();
    for name in ["first", "second", "third"] {
        crate::test_support::branch(&project.repo, name, &base.0)?;
        crate::test_support::switch(&project.repo, name)?;
        fs::write(project.repo.join("payload.txt"), format!("{name}\n"))?;
        sources.push(record_commit(&project.repo)?);
    }
    let mut ids = Vec::new();
    for commit in &sources {
        ids.push(enqueue(&project, &commit.0)?.1);
    }
    let completed = batch_finish(&paths, &ids[0])?;
    assert_eq!(completed["phase"], "succeeded", "{completed}");
    let release = CommitId::new(
        completed["release_commit"]
            .as_str()
            .context("release source")?,
    )?;
    for (id, commit) in ids.iter().zip(&sources) {
        let member = batch_finish(&paths, id)?;
        assert_eq!(member["phase"], "succeeded");
        assert_eq!(member["resulting_commit"], commit.0);
        assert_eq!(member["release_commit"], release.0);
        batch_contains(&project.repo, &release, commit)?;
    }
    let repairs = batch_records(&paths.root.join("repaired.jsonl"))?;
    assert_eq!(repairs.len(), 2);
    assert_eq!(repairs[0]["member"], sources[1].0);
    assert_eq!(repairs[1]["member"], sources[2].0);
    assert_eq!(batch_records(&paths.root.join("checked.jsonl"))?.len(), 3);
    assert_eq!(batch_records(&paths.root.join("composed.jsonl"))?.len(), 3);
    assert_eq!(
        fs::read_to_string(paths.root.join("prepared"))?
            .lines()
            .count(),
        1
    );
    assert_eq!(batch_records(&paths.root.join("deployed.jsonl"))?.len(), 1);
    Ok(())
}

#[test]
fn release_batch_rejects_a_successful_composer_that_omits_checked_members() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    project.workflow.batch.as_mut().context("batch")?.compose =
        fixture_command("empty_compose", []);
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
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
    let member = batch_finish(&paths, &second)?;
    assert_eq!(member["phase"], "failed");
    assert_eq!(member["release"]["failure"]["stage"], "compose");
    let log = member["release"]["failure"]["diagnostic_path"]
        .as_str()
        .context("rejected composition retains its command log")?;
    assert!(Path::new(log).is_file());
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
fn release_batch_reports_the_active_job_ahead_of_a_ready_release() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
    let shared_dir = tempfile::tempdir()?;
    let shared = Queue::at(shared_dir.path().canonicalize()?.join("shared"), true)?
        .context("test shared queue")?;
    let paths = select(&project, true)?;
    let submit = |commit: &CommitId, workflow: Workflow| {
        submit_in(
            &paths,
            &project.repo,
            &commit.0,
            workflow,
            QueueMode::Global,
            Some(&shared),
        )
    };
    let first = submit(&a, project.workflow.clone())?;
    let second = submit(&b, project.workflow.clone())?;
    let mut check_only = project.workflow.clone();
    check_only.batch = None;
    check_only.prepare = None;
    check_only.deploy = None;
    let blocker = submit(&base, check_only)?;
    let (store, config) = configured(&paths)?;
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let mut worker = Worker::new(&paths, store, config);
    worker.shared = Some(shared);
    for phase in [
        "checking",
        "waiting_release",
        "checking",
        "waiting_release",
        "checking",
    ] {
        assert_eq!(worker.tick()?["phase"], phase);
    }
    let member = second["id"]
        .as_str()
        .context("submitted member has no ID")?;
    let release = status(&paths, Some(member))?["release"].clone();
    assert_eq!(release["id"], first["id"]);
    assert_eq!(release["members"], json!([first["id"], second["id"]]));
    assert_eq!(
        release["blocker"],
        json!({"kind":"job","job":blocker["id"],"phase":"checking"})
    );
    assert_eq!(release["next_action"], "wait");
    assert_eq!(status(&paths, None)?["releases"], json!([release]));
    Ok(())
}

#[test]
fn release_batch_never_replays_a_composition_without_terminal_evidence() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let a = batch_branch(&project, &base, "change-a")?;
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
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
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
    batch_contains(&project.repo, &current, &released)?;
    batch_contains(&project.repo, &current, &b)?;
    let compositions = batch_records(&paths.root.join("composed.jsonl"))?;
    assert_eq!(compositions.len(), 2);
    assert_eq!(compositions[1]["base"], released.0);
    assert_eq!(batch_records(&paths.root.join("deployed.jsonl"))?.len(), 2);
    Ok(())
}

#[test]
fn release_batch_can_be_cancelled_from_a_member_before_its_candidate_check() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    project.workflow.batch.as_mut().context("batch")?.check = Some(batch_python(
        "import os,pathlib; pathlib.Path(os.environ['CANDIDATE_CHECK']).touch()",
        vec![("CANDIDATE_CHECK", project.state.join("candidate-check"))],
    ));
    let a = batch_branch(&project, &base, "change-a")?;
    let b = batch_branch(&project, &base, "change-b")?;
    let (paths, first) = enqueue(&project, &a.0)?;
    let (_, second) = enqueue(&project, &b.0)?;
    batch_freeze(&paths, &first)?;
    assert_eq!(worker_once(&paths)?["phase"], "checking_release");

    cancel(&paths, &second)?;
    assert_eq!(batch_finish(&paths, &first)?["phase"], "cancelled");
    assert_eq!(batch_finish(&paths, &second)?["phase"], "cancelled");
    assert!(!paths.root.join("candidate-check").exists());
    assert!(!paths.root.join("prepared").exists());
    assert!(batch_records(&paths.root.join("deployed.jsonl"))?.is_empty());
    Ok(())
}

#[test]
fn release_batch_recovers_a_candidate_repair_without_launching_it_again() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    let batch = project.workflow.batch.as_mut().context("batch")?;
    batch.check = Some(batch_python(
        r"
import pathlib, sys
if pathlib.Path('payload.txt').read_text() != 'done\n':
    sys.stdout.write('stdout')
    sys.stderr.write('stderr')
    sys.exit(1)
",
        vec![],
    ));
    batch.repair = Some(fixture_command(
        "repair",
        [
            (
                "ATTEMPTS".into(),
                project.state.join("attempts").display().to_string(),
            ),
            (
                "DIAGNOSTICS".into(),
                project.state.join("diagnostics").display().to_string(),
            ),
            ("REPLACEMENT".into(), "done".into()),
        ],
    ));
    let (paths, id) = enqueue(&project, &base.0)?;
    batch_freeze(&paths, &id)?;
    assert_eq!(worker_once(&paths)?["phase"], "checking_release");
    assert_eq!(worker_once(&paths)?["phase"], "repairing_release");
    assert_eq!(worker_once(&paths)?["phase"], "checking_release");

    // Leave the Git ref advanced as if the worker stopped before saving the
    // repaired source to the queue. Withhold the terminal command evidence.
    let store = Store::open(&paths.root)?;
    let mut job = store.job(&id)?;
    let repaired = job
        .release_commit
        .clone()
        .context("retained release repair")?;
    let run = job.runs.pop().context("release repair execution")?;
    assert_eq!(run.stage, "release_repair");
    let receipt = paths.job(&id).join(format!("{}.json", run.operation));
    let gate = paths
        .root
        .join("broker")
        .join(format!("{id}-{}.json", run.operation));
    let completion_path = gate.with_extension("process").join("result.json");
    let completion = fs::read(&completion_path)?;
    fs::remove_file(&completion_path)?;
    fs::remove_file(&receipt)?;
    let mut execution: Value = serde_json::from_slice(&fs::read(&gate)?)?;
    execution["state"] = json!("running");
    execution["result"] = Value::Null;
    store::atomic_json(&gate, &execution)?;
    job.phase = Phase::Blocked;
    job.stopped_phase = Some(Phase::RepairingRelease);
    job.unresolved = true;
    job.release_commit = Some(run.commit.clone());
    job.batch.as_mut().context("closed release")?.commit = Some(run.commit.clone());
    job.operation = Some(Operation {
        phase: Phase::RepairingRelease,
        name: run.operation,
        commit: run.commit,
        result: receipt,
        started: run.stamp,
    });
    store.save(&mut job)?;

    assert!(recover(&paths, &id).is_err());
    assert_eq!(worker_once(&paths)?["phase"], "blocked");
    assert!(!paths.root.join("prepared").exists());
    store::atomic_bytes(&completion_path, &completion)?;
    assert_eq!(recover(&paths, &id)?["phase"], "checking_release");
    assert_eq!(store.job(&id)?.release_commit, Some(repaired.clone()));
    let finished = batch_finish(&paths, &id)?;
    assert_eq!(finished["phase"], "succeeded");
    assert_eq!(finished["release_commit"], repaired.0);
    assert_eq!(fs::read_to_string(paths.root.join("attempts"))?, "repair");
    Ok(())
}

#[test]
fn release_batch_rechecks_the_candidate_after_the_destination_moves() -> Result<()> {
    let (_temporary, mut project, base) = fixture()?;
    batch_workflow(&mut project, &base);
    project.workflow.integration = Some(crate::workflow::IntegrationConfig {
        mode: crate::workflow::IntegrationMode::Manual,
        ..Default::default()
    });
    project.workflow.batch.as_mut().context("batch")?.check = Some(batch_python(
        r"import json,os; c=json.load(open(os.environ['TELETE_CONTEXT'])); open(os.environ['CANDIDATE_LOG'],'a').write(c['source']['commit']+'\n')",
        vec![("CANDIDATE_LOG", project.state.join("candidate-log"))],
    ));
    let source = batch_branch(&project, &base, "change")?;
    let (paths, id) = enqueue(&project, &source.0)?;
    batch_freeze(&paths, &id)?;
    worker_once(&paths)?;
    worker_once(&paths)?;
    let waiting = worker_once(&paths)?;
    assert_eq!(waiting["phase"], "waiting_merge");

    crate::test_support::switch(&project.repo, "main")?;
    fs::write(project.repo.join("advanced.txt"), "unrelated change\n")?;
    let advanced = record_commit(&project.repo)?;
    merge(&paths, &id)?;
    assert_eq!(worker_once(&paths)?["phase"], "composing");
    assert_eq!(worker_once(&paths)?["phase"], "checking_release");
    assert_eq!(worker_once(&paths)?["phase"], "preparing");
    let recomposed = worker_once(&paths)?;
    assert_eq!(recomposed["phase"], "waiting_merge");
    let candidate = CommitId::new(recomposed["release_commit"].as_str().context("new release")?)?;
    assert_ne!(waiting["release_commit"], candidate.0);
    batch_contains(&project.repo, &candidate, &advanced)?;
    batch_contains(&project.repo, &candidate, &source)?;

    merge(&paths, &id)?;
    assert_eq!(batch_finish(&paths, &id)?["phase"], "succeeded");
    let checks = fs::read_to_string(paths.root.join("candidate-log"))?;
    assert_eq!(
        checks.lines().collect::<Vec<_>>(),
        [waiting["release_commit"].as_str().context("first release")?, &candidate.0]
    );
    Ok(())
}
