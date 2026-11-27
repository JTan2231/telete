use super::*;

const PRODUCTION: &str = "refs/telete/releases/targets/production";

fn release_workflow(project: &Project) -> TestResult<Value> {
    let mut workflow = batch_release_workflow(project)?;
    let deploy = r"
import json, os, pathlib, sys
with open(os.environ['TELETE_CONTEXT']) as context, open(os.environ['DEPLOYED'], 'a') as log:
    print(json.load(context)['source']['commit'], file=log)
if pathlib.Path('reject-release').exists():
    sys.exit(23)
";
    workflow["deploy"] = json!({
        "program": "/usr/bin/python3",
        "args": ["-c", deploy],
        "env": {"DEPLOYED": project.root.join("deployments")}
    });
    Ok(workflow)
}

fn configure(project: &Project, workflow: &Value) -> TestResult {
    fs::write(
        project.root.join("telete.json"),
        serde_json::to_vec(workflow)?,
    )?;
    Ok(())
}

fn change(project: &Project, name: &str) -> TestResult<String> {
    create_branch(&project.repo, name)?;
    let revision = commit_file(&project.repo, name, name)?;
    test_support::switch(&project.repo, "main")?;
    Ok(revision)
}

fn submit(project: &Project, revision: &str) -> TestResult<String> {
    Ok(job_id(&checked(project.command(&["submit", revision]))?)?.to_owned())
}

fn failed_release(project: &Project, job: &str) -> TestResult<Value> {
    let output = bounded_output(spawn(project.command(&["wait", job, "--timeout", "20"]))?)?;
    assert_eq!(output.status.code(), Some(1));
    let observation: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(observation["observation"], "terminal");
    assert_eq!(observation["job"]["phase"], "failed");
    Ok(observation["job"].clone())
}

#[test]
fn failed_deployment_preserves_the_previous_target_after_integration() -> TestResult {
    let project = Project::new()?;
    configure(&project, &release_workflow(&project)?)?;
    let first = change(&project, "first-change")?;
    let id = submit(&project, &first)?;
    let success = checked_json(project.command(&["wait", &id]))?;
    let deployed = success["job"]["merged_commit"]
        .as_str()
        .ok_or("missing merged commit")?;
    assert_contains(&project.repo, &first, deployed)?;
    assert_eq!(commit(&project.repo, PRODUCTION)?, deployed);

    let second = change(&project, "reject-release")?;
    let id = submit(&project, &second)?;
    let failure = failed_release(&project, &id)?;
    let merged = failure["merged_commit"]
        .as_str()
        .ok_or("missing merged commit")?;
    assert_ne!(merged, deployed);
    assert_contains(&project.repo, &second, merged)?;
    assert_eq!(failure["release"]["failure"]["stage"], "deploy");
    assert_eq!(failure["release_commit"], merged);
    assert_eq!(commit(&project.repo, "refs/heads/main")?, merged);
    assert_eq!(commit(&project.repo, PRODUCTION)?, deployed);
    assert_eq!(
        fs::read_to_string(project.root.join("deployments"))?,
        format!("{deployed}\n{merged}\n")
    );
    Ok(())
}

#[test]
fn concurrent_retries_from_both_members_deploy_one_frozen_release() -> TestResult {
    let project = Project::new()?;
    let ready = project.root.join("allow-candidate");
    let mut workflow = release_workflow(&project)?;
    workflow["batch"]["check"] = json!({"program": "/bin/test", "args": ["-f", ready]});
    configure(&project, &workflow)?;
    let a = change(&project, "change-a")?;
    let b = change(&project, "change-b")?;
    checked(project.command(&["pause"]))?;
    let first = submit(&project, &a)?;
    let second = submit(&project, &b)?;
    checked(project.command(&["resume"]))?;
    let failed = failed_release(&project, &second)?;
    assert_eq!(failed["release"]["failure"]["stage"], "release_check");
    assert_eq!(failed["release"]["members"], json!([first, second]));
    wait_for_worker_ownership(&project.state, false)?;
    assert!(!project.root.join("deployments").exists());

    fs::write(ready, "")?;
    let left = spawn(project.command(&["retry", &first]))?;
    let right = spawn(project.command(&["retry", &second]))?;
    let receipt: Value = serde_json::from_str(&successful(bounded_output(left)?)?)?;
    let duplicate: Value = serde_json::from_str(&successful(bounded_output(right)?)?)?;
    assert_eq!(receipt, duplicate);
    assert_eq!(receipt["retry_of"], first);
    let members = receipt["members"]
        .as_array()
        .ok_or("missing retry members")?;
    assert_eq!(members.len(), 2);
    for (member, original, revision) in [(&members[0], &first, &a), (&members[1], &second, &b)] {
        assert_eq!(member["retry_of"], *original);
        assert_eq!(member["commit"], *revision);
        let id = member["id"].as_str().ok_or("missing retry member ID")?;
        let settled = checked_json(project.command(&["wait", id]))?;
        assert_eq!(settled["job"]["release_id"], receipt["id"]);
        assert_eq!(
            settled["job"]["release_commit"],
            commit(&project.repo, PRODUCTION)?
        );
    }
    let released = commit(&project.repo, PRODUCTION)?;
    assert_contains(&project.repo, &a, &released)?;
    assert_contains(&project.repo, &b, &released)?;
    assert_eq!(
        fs::read_to_string(project.root.join("deployments"))?,
        format!("{released}\n")
    );
    Ok(())
}

#[test]
fn repaired_candidate_is_rechecked_and_late_member_enters_the_next_release() -> TestResult {
    let project = Project::new()?;
    let a = change(&project, "change-a")?;
    let b = change(&project, "change-b")?;
    let c = change(&project, "change-c")?;
    let candidates = project.root.join("candidates");
    let proceed = project.root.join("continue");
    let attempts = project.root.join("repairs");
    let mut workflow = release_workflow(&project)?;
    let check_candidate = r"
import json, os, pathlib, sys, time
with open(os.environ['TELETE_CONTEXT']) as context, open(os.environ['CANDIDATES'], 'a') as log:
    print(json.load(context)['source']['commit'], file=log)
payload = pathlib.Path('payload.txt')
if not payload.exists() or payload.read_text() != 'shared repair\n':
    while not pathlib.Path(os.environ['PROCEED']).exists():
        time.sleep(0.02)
    sys.stdout.write('stdout')
    sys.stderr.write('stderr')
    sys.exit(1)
";
    workflow["batch"]["check"] = json!({
        "program": "/usr/bin/python3", "timeout_seconds": 15,
        "args": ["-c", check_candidate],
        "env": {"CANDIDATES": candidates, "PROCEED": proceed}
    });
    workflow["batch"]["repair"] = json!({
        "program": std::env::current_exe()?, "args": test_support::workflow_args(),
        "env": {"FIXTURE_ACTION": "repair", "ATTEMPTS": attempts,
            "DIAGNOSTICS": project.root.join("repair-input"), "REPLACEMENT": "shared repair"}
    });
    workflow["prepare"] = json!({
        "program": std::env::current_exe()?, "args": test_support::workflow_args(),
        "env": {"FIXTURE_ACTION": "prepare_head", "PREPARED": project.root.join("prepared")}
    });
    configure(&project, &workflow)?;
    checked(project.command(&["pause"]))?;
    let first = submit(&project, &a)?;
    let second = submit(&project, &b)?;
    checked(project.command(&["resume"]))?;
    wait_for_file(&candidates)?;
    let late = submit(&project, &c)?;
    fs::write(proceed, "")?;
    let first_result = checked_json(project.command(&["wait", &first]))?;
    let second_result = checked_json(project.command(&["wait", &second]))?;
    let next_result = checked_json(project.command(&["wait", &late]))?;
    let release = first_result["job"]["release_commit"]
        .as_str()
        .ok_or("missing repaired commit")?;
    let next = next_result["job"]["release_commit"]
        .as_str()
        .ok_or("missing next commit")?;
    assert_eq!(
        first_result["job"]["release"]["members"],
        json!([first, second])
    );
    assert_eq!(second_result["job"]["release_commit"], release);
    assert_eq!(next_result["job"]["release"]["members"], json!([late]));
    assert_contains(&project.repo, release, next)?;
    assert_contains(&project.repo, &c, next)?;
    assert!(!test_support::contains(
        &Repository::open(&project.repo)?,
        Oid::from_str(&c)?,
        Oid::from_str(release)?
    )?);
    let candidate_log = fs::read_to_string(candidates)?;
    let checks: Vec<_> = candidate_log.lines().collect();
    assert_eq!(checks.len(), 3);
    assert_ne!(checks[0], release);
    for source in [&a, &b] {
        assert_contains(&project.repo, source, checks[0])?;
    }
    assert_contains(&project.repo, checks[0], release)?;
    assert_eq!(checks[1..], [release, next]);
    assert_eq!(fs::read_to_string(attempts)?, "repair");
    assert_eq!(
        fs::read_to_string(project.root.join("checked"))?,
        format!("{a}\n{c}\n")
    );
    for name in ["prepared", "deployments"] {
        assert_eq!(
            fs::read_to_string(project.root.join(name))?,
            format!("{release}\n{next}\n")
        );
    }
    Ok(())
}
