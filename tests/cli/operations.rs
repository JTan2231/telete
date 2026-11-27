use super::*;

#[test]
fn analysis_findings_allow_other_work_before_a_committed_repair() -> TestResult {
    let project = Project::new()?;
    let config = project.root.join("analysis.json");
    let audit = project.root.join("stages");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "repo": "repo", "state": "state", "queue": "repository",
            "analyze": {
                "program": "/bin/sh",
                "args": ["-c", "echo analyze >> \"$AUDIT\"; echo finding >&2; exit 1"],
                "env": {"AUDIT": audit}
            },
            "check": {
                "program": "/bin/sh",
                "args": ["-c", "echo check >> \"$AUDIT\"; cat source.txt >> \"$AUDIT\""],
                "env": {"AUDIT": audit}
            }
        }))?,
    )?;
    let follow = bounded_output(spawn(configured_command(
        &project.root,
        &config,
        &["submit", "--follow"],
    ))?)?;
    assert_eq!(follow.status.code(), Some(1));
    let values = serde_json::Deserializer::from_slice(&follow.stdout)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(values.len(), 2);
    let id = values[0]["id"].as_str().ok_or("missing submission ID")?;
    assert_eq!(values[1]["observation"], "terminal");
    assert_eq!(values[1]["job"]["id"], id);
    assert_eq!(values[1]["job"]["phase"], "awaiting_repair");

    let other = checked_json(project.command(&["submit"]))?;
    let other_id = other["id"].as_str().ok_or("missing other submission ID")?;
    let finished = checked_json(project.command(&["wait", other_id]))?;
    assert_eq!(finished["job"]["phase"], "succeeded");
    let waiting = checked_json(project.command(&["status", id]))?;
    assert_eq!(waiting["phase"], "awaiting_repair");
    assert_eq!(waiting["analyzed_commit"], project.commit);
    assert_eq!(waiting["runs"][0]["result"]["stderr"], "finding\n");

    let rejected = bounded_output(spawn(project.command(&["repair", id]))?)?;
    assert_eq!(rejected.status.code(), Some(1));
    let rejected: Value = serde_json::from_slice(&rejected.stderr)?;
    assert!(
        rejected["error"]
            .as_str()
            .is_some_and(|message| message.contains("new committed source revision"))
    );
    let repair = commit_source(&project.repo, "corrected\n")?;
    let receipt = checked_json(project.command(&["repair", id, &repair]))?;
    assert_eq!(receipt["id"], id);
    assert_eq!(receipt["repaired_commit"], repair);
    let finished = checked_json(project.command(&["wait", id]))?;
    assert_eq!(finished["job"]["phase"], "succeeded");
    assert_eq!(finished["job"]["commit"], project.commit);
    assert_eq!(finished["job"]["analyzed_commit"], project.commit);
    assert_eq!(finished["job"]["repaired_commit"], repair);
    assert_eq!(finished["job"]["resulting_commit"], repair);
    assert_eq!(fs::read_to_string(&audit)?, "analyze\ncheck\ncorrected\n");
    Ok(())
}

#[test]
fn maintenance_holds_drain_active_work_and_release_by_owner() -> TestResult {
    let resume = ["resume"].as_slice();
    let release = ["maintenance", "release", "--owner", "release"].as_slice();
    for (first_action, final_action, still_paused) in
        [(resume, release, false), (release, resume, true)]
    {
        let project = Project::new()?;
        gated_workflow(&project)?;
        let first = checked_json(project.command(&["submit"]))?;
        let first_id = first["id"].as_str().ok_or("missing first submission ID")?;
        wait_for_file(&project.root.join("started"))?;
        let first_hold =
            checked_json(project.command(&["maintenance", "hold", "--owner", "build"]))?;
        assert_eq!(first_hold["active_job"], first_id);
        assert_eq!(first_hold["drained"], false);
        checked(project.command(&["maintenance", "hold", "--owner", "build"]))?;
        let second_hold =
            checked_json(project.command(&["maintenance", "hold", "--owner", "release"]))?;
        assert_eq!(second_hold["owners"], json!(["build", "release"]));
        let next = checked_json(project.command(&["submit"]))?;
        let next_id = next["id"].as_str().ok_or("missing second submission ID")?;
        fs::write(project.root.join("release"), "")?;
        let finished = checked_json(project.command(&["wait", first_id]))?;
        assert_eq!(finished["job"]["phase"], "succeeded");
        let drained = checked_json(project.command(&["maintenance", "status"]))?;
        assert_eq!(drained["drained"], true);
        assert_eq!(drained["held"], true);

        checked(project.command(&["pause"]))?;
        let remaining =
            checked_json(project.command(&["maintenance", "release", "--owner", "build"]))?;
        assert_eq!(remaining["owners"], json!(["release"]));
        checked(project.command(first_action))?;
        let waiting = checked_json(project.command(&["status"]))?;
        assert_eq!(waiting["paused"], still_paused);
        let owners = if still_paused {
            json!([])
        } else {
            json!(["release"])
        };
        assert_eq!(waiting["maintenance_owners"], owners);
        let observed = bounded_output(spawn(project.command(&[
            "wait",
            next_id,
            "--timeout",
            "1",
        ]))?)?;
        assert_eq!(observed.status.code(), Some(1));
        let observed: Value = serde_json::from_slice(&observed.stdout)?;
        assert_eq!(observed["observation"], "timeout");
        assert_eq!(observed["job"]["phase"], "queued");
        assert_eq!(
            fs::read_to_string(project.root.join("started"))?,
            "started\n"
        );

        checked(project.command(final_action))?;
        let finished = checked_json(project.command(&["wait", next_id]))?;
        assert_eq!(finished["job"]["phase"], "succeeded");
        assert_eq!(
            fs::read_to_string(project.root.join("started"))?,
            "started\nstarted\n"
        );
    }
    Ok(())
}
