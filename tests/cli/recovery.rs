use super::{
    Project, TestResult, Value, bounded_output, checked, checked_json, fs, gated_workflow, json,
    spawn, wait_for_file, wait_for_worker_ownership, wait_for_worker_pid,
};
use rustix::process::{Pid, Signal, kill_process};
use std::path::PathBuf;

// Undo the fault even if an assertion fails while the command or job is blocked.
struct DelayedCompletion<'a> {
    project: &'a Project,
    job: &'a str,
    stopped_worker: Option<Pid>,
    receipt: PathBuf,
    held: PathBuf,
}

impl DelayedCompletion<'_> {
    fn resume_worker(&mut self) -> TestResult {
        if let Some(worker) = self.stopped_worker {
            kill_process(worker, Signal::CONT)?;
            self.stopped_worker = None;
        }
        Ok(())
    }
}

impl Drop for DelayedCompletion<'_> {
    fn drop(&mut self) {
        let restored = fs::rename(&self.held, &self.receipt).is_ok();
        let _ = self.resume_worker();
        if restored {
            let _ = checked(self.project.command(&["recover", self.job]));
        }
    }
}

#[test]
fn recover_waits_for_matching_terminal_evidence_without_replaying_the_check() -> TestResult {
    let project = Project::new()?;
    gated_workflow(&project)?;
    let config = project.root.join("telete.json");
    let mut workflow: Value = serde_json::from_slice(&fs::read(&config)?)?;
    let prepared = project.root.join("prepared");
    workflow["prepare"] = json!({"program":"/usr/bin/touch","args":[prepared]});
    fs::write(config, serde_json::to_vec(&workflow)?)?;
    let submission = checked_json(project.command(&["submit"]))?;
    let id = submission["id"].as_str().ok_or("missing submission ID")?;
    wait_for_file(&project.root.join("started"))?;

    // Stop the worker while it waits for the executor. Let the executor finish
    // normally, then move its actual completion before the worker can read it.
    let worker =
        Pid::from_raw(wait_for_worker_pid(&project.state, None)?).ok_or("invalid worker PID")?;
    let gate_key = format!("{id}-workflow-check-0");
    let mut delayed = DelayedCompletion {
        project: &project,
        job: id,
        stopped_worker: None,
        receipt: project
            .state
            .join("broker")
            .join(format!("{gate_key}.process/result.json")),
        held: project.root.join("held-completion.json"),
    };
    kill_process(worker, Signal::STOP)?;
    delayed.stopped_worker = Some(worker);
    fs::write(project.root.join("release"), "continue")?;
    wait_for_file(&delayed.receipt)?;
    fs::rename(&delayed.receipt, &delayed.held)?;
    delayed.resume_worker()?;

    let observed = bounded_output(spawn(project.command(&["wait", id, "--timeout", "10"]))?)?;
    assert_eq!(observed.status.code(), Some(1));
    let observation: Value = serde_json::from_slice(&observed.stdout)?;
    assert_eq!(observation["observation"], "terminal");
    assert_eq!(observation["job"]["phase"], "blocked");
    let gates = checked_json(project.command(&["gates"]))?;
    let executions = gates["executions"].as_array().ok_or("missing executions")?;
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0]["key"], gate_key);
    assert_eq!(executions[0]["state"], "running");
    assert!(executions[0]["result"].is_null());
    let refused = bounded_output(spawn(project.command(&["recover", id]))?)?;
    assert_eq!(refused.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&refused.stderr)?;
    assert_eq!(error["ok"], false);
    assert!(
        error["error"]
            .as_str()
            .is_some_and(|msg| msg.contains("terminal"))
    );
    let status = checked_json(project.command(&["status", id]))?;
    assert_eq!(status["phase"], "blocked");
    assert!(!prepared.exists());

    fs::rename(&delayed.held, &delayed.receipt)?;
    checked(project.command(&["recover", id]))?;
    let settled = checked_json(project.command(&["wait", id, "--timeout", "10"]))?;
    assert_eq!(settled["job"]["phase"], "succeeded");
    assert!(prepared.is_file());
    assert_eq!(
        fs::read_to_string(project.root.join("started"))?,
        "started\n"
    );
    wait_for_worker_ownership(&project.state, false)?;
    assert_eq!(
        checked_json(project.command(&["gates"]))?["executions"],
        json!([])
    );
    Ok(())
}
