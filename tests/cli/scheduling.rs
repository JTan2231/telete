use super::*;

#[test]
fn global_repositories_share_a_slot_while_an_independent_repository_runs() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let scheduler = temporary.path().canonicalize()?.join("shared-queue");
    let mut first = Project::new()?;
    let mut second = Project::new()?;
    let mut independent = Project::new()?;
    independent.shared_queue = Some(scheduler.clone());

    for project in [&mut first, &mut second] {
        project.shared_queue = Some(scheduler.clone());
        gated_workflow(project)?;
        let path = project.root.join("telete.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
        config
            .as_object_mut()
            .ok_or("expected config object")?
            .remove("queue");
        fs::write(path, serde_json::to_vec(&config)?)?;
    }

    let first_receipt = checked_json(first.command(&["submit"]))?;
    let first_id = first_receipt["id"].as_str().ok_or("missing first job ID")?;
    wait_for_file(&first.root.join("started"))?;
    let second_receipt = checked_json(second.command(&["submit"]))?;
    let second_id = second_receipt["id"]
        .as_str()
        .ok_or("missing second job ID")?;

    let independent_receipt = checked_json(independent.command(&["submit"]))?;
    let independent_id = independent_receipt["id"]
        .as_str()
        .ok_or("missing independent job ID")?;
    let settled = checked_json(independent.command(&["wait", independent_id, "--timeout", "10"]))?;
    assert_eq!(settled["job"]["phase"], "succeeded");

    let waiting = checked_json(second.command(&["status"]))?;
    assert_eq!(waiting["queue_mode"], "global");
    assert_eq!(
        waiting["shared_slot"],
        json!({"job": first_id, "state_root": first.state})
    );
    assert_eq!(waiting["waiting_for_shared_slot"], true);
    assert_eq!(waiting["queue"][0]["id"], second_id);
    assert!(!first.root.join("finished").exists());
    assert!(!second.root.join("started").exists());

    fs::write(first.root.join("release"), "")?;
    checked(first.command(&["wait", first_id]))?;
    wait_for_file(&second.root.join("started"))?;
    let holder = checked_json(second.command(&["status"]))?;
    assert_eq!(
        holder["shared_slot"],
        json!({"job": second_id, "state_root": second.state})
    );
    assert_eq!(holder["waiting_for_shared_slot"], false);

    fs::write(second.root.join("release"), "")?;
    checked(second.command(&["wait", second_id]))?;
    wait_for_worker_ownership(&second.state, false)?;
    assert_eq!(
        checked_json(second.command(&["status"]))?["shared_slot"],
        Value::Null
    );
    Ok(())
}
