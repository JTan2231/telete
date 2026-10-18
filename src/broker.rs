pub(crate) use crate::model::ResourceClass;
use crate::{
    paths::{Paths, atomic_json, ensure_private, lock},
    process::{CommandSpec, ProcessResult},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, thread, time::Duration};

#[derive(Debug, Serialize, Deserialize)]
struct Execution {
    schema: u32,
    key: String,
    class: ResourceClass,
    command: CommandSpec,
    state: String,
    owner_pid: u32,
    result: Option<ProcessResult>,
}

fn basename(key: &str) -> Result<String> {
    ensure!(
        !key.is_empty() && key.len() <= 200,
        "invalid gate identity length"
    );
    Ok(key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect())
}

pub(crate) fn recorded(paths: &Paths, key: &str) -> Result<bool> {
    Ok(paths
        .root
        .join("broker")
        .join(format!("{}.json", basename(key)?))
        .try_exists()?)
}

pub(crate) fn request(paths: &Paths, key: &str, class: ResourceClass) -> Result<CommandSpec> {
    let directory = paths.root.join("broker");
    let name = basename(key)?;
    let _owner = lock(&directory.join(format!("{name}.lock")), false)?;
    let execution: Execution =
        serde_json::from_slice(&fs::read(directory.join(format!("{name}.json")))?)?;
    ensure!(
        execution.schema == 1 && execution.key == key && execution.class == class,
        "retained gate belongs to another execution"
    );
    Ok(execution.command)
}

fn reconcile(paths: &Paths, record: &std::path::Path, execution: &mut Execution) -> Result<()> {
    if execution.result.is_some() {
        return Ok(());
    }
    let process = paths
        .root
        .join("broker")
        .join(format!("{}.process", basename(&execution.key)?));
    if process.join("result.json").exists() {
        let request: CommandSpec =
            serde_json::from_slice(&fs::read(process.join("request.json"))?)?;
        ensure!(
            serde_json::to_value(request)? == serde_json::to_value(&execution.command)?,
            "gate process completion belongs to a different command"
        );
        if let Some(result) = crate::process::observe(&process)? {
            execution.state = "finished".into();
            execution.result = Some(result);
            atomic_json(record, execution)?;
        }
    }
    Ok(())
}

pub(crate) fn completed(
    paths: &Paths,
    key: &str,
    class: ResourceClass,
    spec: &CommandSpec,
) -> Result<ProcessResult> {
    let directory = paths.root.join("broker");
    let name = basename(key)?;
    let _owner = lock(&directory.join(format!("{name}.lock")), false)?;
    let record = directory.join(format!("{name}.json"));
    let mut execution: Execution = serde_json::from_slice(&fs::read(&record)?)?;
    ensure!(
        execution.schema == 1
            && execution.key == key
            && execution.class == class
            && serde_json::to_value(&execution.command)? == serde_json::to_value(spec)?,
        "retained gate belongs to another execution"
    );
    reconcile(paths, &record, &mut execution)?;
    ensure!(
        execution.state == "finished",
        "gate has no terminal completion"
    );
    let result = execution.result.context("terminal gate has no result")?;
    let process = directory.join(format!("{name}.process"));
    let request: CommandSpec = serde_json::from_slice(&fs::read(process.join("request.json"))?)?;
    ensure!(
        serde_json::to_value(request)? == serde_json::to_value(spec)?,
        "terminal process request names another command"
    );
    let observed = crate::process::observe(&process)?.context("gate has no process completion")?;
    ensure!(
        serde_json::to_value(&result)? == serde_json::to_value(observed)?,
        "gate result differs from its process completion"
    );
    Ok(result)
}

pub(crate) fn run(
    paths: &Paths,
    key: &str,
    class: ResourceClass,
    spec: &CommandSpec,
) -> Result<ProcessResult> {
    let directory = paths.root.join("broker");
    ensure_private(&directory)?;
    let name = basename(key)?;
    let record = directory.join(format!("{name}.json"));
    let _owner = lock(&directory.join(format!("{name}.lock")), true)?;
    if record.exists() {
        let mut saved: Execution = serde_json::from_slice(&fs::read(&record)?)?;
        ensure!(
            saved.schema == 1
                && saved.key == key
                && saved.class == class
                && serde_json::to_value(&saved.command)? == serde_json::to_value(spec)?,
            "gate identity reused with different execution"
        );
        reconcile(paths, &record, &mut saved)?;
        if let Some(result) = saved.result {
            return Ok(result);
        }
        ensure!(
            saved.state == "queued",
            "gate execution is unresolved: {key}; retain its source and inspect broker records"
        );
    }
    let mut execution = Execution {
        schema: 1,
        key: key.to_owned(),
        class,
        command: spec.clone(),
        state: "queued".into(),
        owner_pid: std::process::id(),
        result: None,
    };
    atomic_json(&record, &execution)?;
    if class == ResourceClass::Supervisor {
        execution.state = "running".into();
        atomic_json(&record, &execution)?;
        let result =
            crate::process::supervise(paths, spec, &directory.join(format!("{name}.process")))?;
        execution.state = "finished".into();
        execution.result = Some(result.clone());
        atomic_json(&record, &execution)?;
        return Ok(result);
    }
    let slots = if class == ResourceClass::Heavy { 1 } else { 2 };
    let (guard, slot_path) = loop {
        let mut selected = None;
        for index in 0..slots {
            let slot = format!(
                "{}-{index}",
                if class == ResourceClass::Heavy {
                    "heavy"
                } else {
                    "light"
                }
            );
            if let Ok(guard) = lock(&directory.join(format!("slot-{slot}.lock")), false) {
                let slot_path = directory.join(format!("slot-{slot}.json"));
                if slot_path.exists() {
                    let prior: PathBuf = serde_json::from_slice(&fs::read(&slot_path)?)?;
                    ensure!(
                        prior.parent() == Some(directory.as_path()),
                        "foreign broker slot record"
                    );
                    let mut execution: Execution = serde_json::from_slice(&fs::read(&prior)?)?;
                    reconcile(paths, &prior, &mut execution)?;
                    ensure!(
                        execution.result.is_some(),
                        "compiler/resource slot has unresolved execution {}; no new gate is admitted",
                        execution.key
                    );
                }
                selected = Some((guard, slot_path));
                break;
            }
        }
        if let Some(selected) = selected {
            break selected;
        }
        thread::sleep(Duration::from_millis(100));
    };
    atomic_json(&slot_path, &record)?;
    execution.state = "running".into();
    atomic_json(&record, &execution)?;
    let result =
        match crate::process::supervise(paths, spec, &directory.join(format!("{name}.process"))) {
            Ok(result) => result,
            Err(error) => {
                // Once admitted, missing completion is uncertain. Keep slot intent
                // even when the invoking worker disappears or output cannot be read.
                drop(guard);
                bail!("gate {key} has no retained terminal result: {error:#}");
            }
        };
    execution.state = "finished".into();
    execution.result = Some(result.clone());
    atomic_json(&record, &execution)?;
    drop(guard);
    Ok(result)
}

pub(crate) fn status(paths: &Paths) -> Result<serde_json::Value> {
    let mut records = Vec::new();
    for entry in fs::read_dir(paths.root.join("broker"))? {
        let path = entry?.path();
        if path.extension().is_some_and(|v| v == "json")
            && !path
                .file_name()
                .is_some_and(|v| v.to_string_lossy().starts_with("slot-"))
        {
            records.push(serde_json::from_slice::<serde_json::Value>(&fs::read(
                path,
            )?)?);
        }
    }
    Ok(serde_json::json!({"schema":1,"executions":records}))
}

// The caller holds admission ownership and prevents new work. Reconcile only
// exact retained process completions; missing evidence is never fabricated.
pub(crate) fn require_settled(paths: &Paths) -> Result<()> {
    let _guards = settled_guards(paths)?;
    Ok(())
}

pub(crate) fn settled_guards(paths: &Paths) -> Result<Vec<crate::paths::FileLock>> {
    let directory = paths.root.join("broker");
    let slots = ["heavy-0", "light-0", "light-1"];
    let guards = slots
        .iter()
        .map(|slot| lock(&directory.join(format!("slot-{slot}.lock")), false))
        .collect::<Result<Vec<_>>>()?;
    let slot_records = slots.map(|slot| directory.join(format!("slot-{slot}.json")));
    for slot in &slot_records {
        if slot.try_exists()? {
            let record: PathBuf = serde_json::from_slice(&fs::read(slot)?)?;
            ensure!(
                record.parent() == Some(directory.as_path())
                    && record.extension().is_some_and(|v| v == "json")
                    && !slot_records.contains(&record)
                    && record.is_file(),
                "broker slot has a foreign or missing execution record"
            );
        }
    }
    for entry in fs::read_dir(&directory)? {
        let record = entry?.path();
        if record.extension().is_some_and(|v| v == "process") {
            ensure!(
                record.with_extension("json").is_file(),
                "broker process has no retained execution record"
            );
        }
        if record.extension().is_none_or(|v| v != "json") || slot_records.contains(&record) {
            continue;
        }
        let mut execution: Execution = serde_json::from_slice(&fs::read(&record)?)?;
        let name = basename(&execution.key)?;
        let _owner = lock(&directory.join(format!("{name}.lock")), false)?;
        ensure!(
            execution.schema == 1 && record == directory.join(format!("{name}.json")),
            "broker execution record has another identity"
        );
        reconcile(paths, &record, &mut execution)?;
        ensure!(
            execution.state == "finished",
            "broker execution is not a matching terminal record: {}",
            execution.key
        );
        let retained = execution
            .result
            .context("terminal broker execution has no retained result")?;
        let process = directory.join(format!("{name}.process"));
        let request: CommandSpec =
            serde_json::from_slice(&fs::read(process.join("request.json"))?)?;
        ensure!(
            serde_json::to_value(request)? == serde_json::to_value(&execution.command)?,
            "broker terminal process request names another command: {}",
            execution.key
        );
        let observed = crate::process::observe(&process)?
            .context("terminal broker execution has no process completion")?;
        ensure!(
            serde_json::to_value(retained)? == serde_json::to_value(observed)?,
            "broker result differs from its process completion: {}",
            execution.key
        );
    }
    Ok(guards)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn request_reads_saved_command_and_requires_the_same_gate_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(temporary.path().join("work")).unwrap();
        let command = CommandSpec {
            program: "/usr/bin/true".into(),
            args: vec!["literal argument".into()],
            cwd: paths.root.clone(),
            env: BTreeMap::from([("SAVED_VALUE".into(), "retained".into())]),
            timeout_seconds: None,
            stdin: None,
            confined: false,
            context: None,
        };
        let key = "request/one";
        let record = paths
            .root
            .join("broker")
            .join(format!("{}.json", basename(key).unwrap()));
        let mut execution = Execution {
            schema: 1,
            key: key.into(),
            class: ResourceClass::Supervisor,
            command: command.clone(),
            state: "queued".into(),
            owner_pid: std::process::id(),
            result: None,
        };
        atomic_json(&record, &execution).unwrap();
        let saved = request(&paths, key, ResourceClass::Supervisor).unwrap();
        assert_eq!(
            serde_json::to_value(saved).unwrap(),
            serde_json::to_value(command).unwrap()
        );
        assert!(request(&paths, key, ResourceClass::Heavy).is_err());
        // These keys share a filesystem basename but remain distinct identities.
        assert!(request(&paths, "request?one", ResourceClass::Supervisor).is_err());
        execution.schema = 2;
        atomic_json(&record, &execution).unwrap();
        assert!(request(&paths, key, ResourceClass::Supervisor).is_err());
    }

    #[test]
    fn a_completed_gate_joins_but_changed_command_cannot_reuse_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(temporary.path().join("work")).unwrap();
        let spec = CommandSpec {
            program: "/usr/bin/true".into(),
            args: vec![],
            cwd: temporary.path().to_path_buf(),
            env: BTreeMap::new(),
            timeout_seconds: Some(5),
            stdin: None,
            confined: false,
            context: None,
        };
        assert!(
            run(&paths, "one", ResourceClass::Light, &spec)
                .unwrap()
                .success()
        );
        assert!(
            run(&paths, "one", ResourceClass::Light, &spec)
                .unwrap()
                .success()
        );
        let mut changed = spec;
        changed.args.push("changed".into());
        assert!(run(&paths, "one", ResourceClass::Light, &changed).is_err());
    }
    #[test]
    fn lost_gate_blocks_its_resource_slot() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(temporary.path().join("work")).unwrap();
        let spec = CommandSpec {
            program: "/missing/command".into(),
            args: vec![],
            cwd: temporary.path().to_path_buf(),
            env: BTreeMap::new(),
            timeout_seconds: Some(5),
            stdin: None,
            confined: false,
            context: None,
        };
        let failed = run(&paths, "lost", ResourceClass::Heavy, &spec).unwrap();
        assert!(failed.start_error.is_some());
        assert!(!failed.success());
        let next = CommandSpec {
            program: "/usr/bin/true".into(),
            ..spec.clone()
        };
        assert!(
            run(&paths, "after-start-failure", ResourceClass::Heavy, &next)
                .unwrap()
                .success()
        );

        // Model an admitted gate whose process has no retained terminal result.
        let record = paths.root.join("broker/lost.json");
        let mut lost: Execution = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        lost.state = "running".into();
        lost.result = None;
        atomic_json(&record, &lost).unwrap();
        fs::remove_file(record.with_extension("process").join("result.json")).unwrap();
        atomic_json(&paths.root.join("broker/slot-heavy-0.json"), &record).unwrap();
        assert!(run(&paths, "lost", ResourceClass::Heavy, &spec).is_err());
        assert!(run(&paths, "next", ResourceClass::Heavy, &next).is_err());
        assert!(run(&paths, "lost", ResourceClass::Heavy, &next).is_err());
    }

    #[test]
    fn later_gate_reconciles_a_completed_child_before_reusing_the_slot() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(temporary.path().join("work")).unwrap();
        let spec = CommandSpec {
            program: "/usr/bin/true".into(),
            args: vec![],
            cwd: paths.root.clone(),
            env: BTreeMap::new(),
            timeout_seconds: Some(5),
            stdin: None,
            confined: false,
            context: None,
        };
        assert!(
            run(&paths, "prior", ResourceClass::Heavy, &spec)
                .unwrap()
                .success()
        );
        let record = paths.root.join("broker/prior.json");
        let mut interrupted: Execution =
            serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        interrupted.result = None;
        interrupted.state = "running".into();
        atomic_json(&record, &interrupted).unwrap();
        assert!(
            run(&paths, "next", ResourceClass::Heavy, &spec)
                .unwrap()
                .success()
        );
        let settled: Execution = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
        assert!(settled.result.unwrap().success());
    }

    fn settled_fixture() -> (tempfile::TempDir, Paths, CommandSpec) {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(temporary.path().join("work")).unwrap();
        let command = CommandSpec {
            program: "/usr/bin/false".into(),
            args: vec![],
            cwd: paths.root.clone(),
            env: BTreeMap::new(),
            timeout_seconds: Some(5),
            stdin: None,
            confined: false,
            context: None,
        };
        let result = run(&paths, "terminal", ResourceClass::Light, &command).unwrap();
        assert_eq!(result.exit_code, Some(1));
        (temporary, paths, command)
    }

    // Parallel fixtures can inherit each other's lock descriptors. Create this
    // test's fixture only after re-executing the single test in its own process.
    fn run_isolated(test: &str) -> bool {
        const CHILD: &str = "TELETE_TEST_BROKER_SETTLEMENT_CHILD";
        if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new(test)) {
            return false;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(CHILD, test)
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated broker test failed: {test}");
                return true;
            }
            if started.elapsed() >= Duration::from_secs(120) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("isolated broker test timed out: {test}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn settlement_accepts_correlated_terminal_failure_without_changing_records() {
        if run_isolated(
            "broker::tests::settlement_accepts_correlated_terminal_failure_without_changing_records",
        ) {
            return;
        }
        let (_temporary, paths, _command) = settled_fixture();
        let record = paths.root.join("broker/terminal.json");
        let before = fs::read(&record).unwrap();
        require_settled(&paths).unwrap();
        assert_eq!(fs::read(record).unwrap(), before);
        let _released = lock(&paths.root.join("broker/slot-heavy-0.lock"), false).unwrap();
    }

    #[test]
    fn settlement_reconciles_exact_completions_and_rejects_missing_evidence() {
        if run_isolated(
            "broker::tests::settlement_reconciles_exact_completions_and_rejects_missing_evidence",
        ) {
            return;
        }
        let (_temporary, paths, _command) = settled_fixture();
        let record = paths.root.join("broker/terminal.json");
        let mut execution: Execution = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        for state in ["queued", "running", "finished"] {
            execution.result = None;
            execution.state = state.into();
            atomic_json(&record, &execution).unwrap();
            require_settled(&paths).unwrap();
            let settled: Execution = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
            assert_eq!(settled.state, "finished");
            assert_eq!(settled.result.unwrap().exit_code, Some(1));
        }
        fs::remove_file(paths.root.join("broker/terminal.process/result.json")).unwrap();
        for state in ["queued", "running", "finished"] {
            execution.result = None;
            execution.state = state.into();
            atomic_json(&record, &execution).unwrap();
            let before = fs::read(&record).unwrap();
            assert!(require_settled(&paths).is_err());
            assert_eq!(fs::read(&record).unwrap(), before);
        }
        fs::remove_file(record).unwrap();
        assert!(require_settled(&paths).is_err());
    }

    #[test]
    fn settlement_rejects_changed_requests_results_and_missing_completion() {
        if run_isolated(
            "broker::tests::settlement_rejects_changed_requests_results_and_missing_completion",
        ) {
            return;
        }
        let (_temporary, paths, command) = settled_fixture();
        let directory = paths.root.join("broker/terminal.process");
        let mut changed = command.clone();
        changed.args.push("changed".into());
        atomic_json(&directory.join("request.json"), &changed).unwrap();
        assert!(require_settled(&paths).is_err());
        atomic_json(&directory.join("request.json"), &command).unwrap();
        let result = directory.join("result.json");
        let mut completion: serde_json::Value =
            serde_json::from_slice(&fs::read(&result).unwrap()).unwrap();
        completion["result"]["stdout"] = "changed".into();
        atomic_json(&result, &completion).unwrap();
        assert!(require_settled(&paths).is_err());
        fs::remove_file(result).unwrap();
        assert!(require_settled(&paths).is_err());
    }

    #[test]
    fn settlement_refuses_an_owned_resource_slot() {
        if run_isolated("broker::tests::settlement_refuses_an_owned_resource_slot") {
            return;
        }
        let (_temporary, paths, _command) = settled_fixture();
        let owned = lock(&paths.root.join("broker/slot-light-1.lock"), false).unwrap();
        assert!(require_settled(&paths).is_err());
        drop(owned);
        require_settled(&paths).unwrap();
    }
}
