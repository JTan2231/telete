use crate::{
    paths::{Paths, atomic_json, ensure_private, lock},
    process::{CommandSpec, ProcessResult},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Execution {
    schema: u32,
    key: String,
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

#[cfg(test)]
pub(crate) fn recorded(paths: &Paths, key: &str) -> Result<bool> {
    Ok(paths
        .root
        .join("broker")
        .join(format!("{}.json", basename(key)?))
        .try_exists()?)
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

pub(crate) fn completed(paths: &Paths, key: &str, spec: &CommandSpec) -> Result<ProcessResult> {
    let directory = paths.root.join("broker");
    let name = basename(key)?;
    let _owner = lock(&directory.join(format!("{name}.lock")), false)?;
    let record = directory.join(format!("{name}.json"));
    let mut execution: Execution = serde_json::from_slice(&fs::read(&record)?)?;
    ensure!(
        execution.schema == 1
            && execution.key == key
            && serde_json::to_value(&execution.command)? == serde_json::to_value(spec)?,
        "retained gate belongs to another execution"
    );
    terminal(paths, &record, &mut execution)
}

fn terminal(
    paths: &Paths,
    record: &std::path::Path,
    execution: &mut Execution,
) -> Result<ProcessResult> {
    ensure!(execution.schema == 1, "unsupported gate record");
    if execution.state == "pruning" {
        // This state is written only after both terminal records correlate. It
        // permits cleanup to finish after part of the process directory is gone.
        return execution
            .result
            .clone()
            .context("pruning gate has no terminal result");
    }
    reconcile(paths, record, execution)?;
    ensure!(
        execution.state == "finished",
        "gate has no terminal completion"
    );
    let result = execution
        .result
        .clone()
        .context("terminal gate has no result")?;
    let process = paths
        .root
        .join("broker")
        .join(format!("{}.process", basename(&execution.key)?));
    let request: CommandSpec = serde_json::from_slice(&fs::read(process.join("request.json"))?)?;
    ensure!(
        serde_json::to_value(request)? == serde_json::to_value(&execution.command)?,
        "terminal process request names another command"
    );
    let observed = crate::process::observe(&process)?.context("gate has no process completion")?;
    ensure!(
        serde_json::to_value(&result)? == serde_json::to_value(observed)?,
        "gate result differs from its process completion"
    );
    Ok(result)
}

/// Forget a settled gate without deleting evidence for an uncertain command.
pub(crate) fn forget(paths: &Paths, key: &str) -> Result<()> {
    let directory = paths.root.join("broker");
    let name = basename(key)?;
    let record = directory.join(format!("{name}.json"));
    let process = directory.join(format!("{name}.process"));
    let lock_path = directory.join(format!("{name}.lock"));
    if !record.try_exists()? {
        ensure!(!process.try_exists()?, "process has no gate record: {key}");
        return Ok(());
    }
    let _owner = lock(&lock_path, false)?;
    let _slot = lock(&directory.join("slot.lock"), false)?;
    let mut execution: Execution = serde_json::from_slice(&fs::read(&record)?)?;
    ensure!(
        execution.key == key,
        "retained gate belongs to another execution"
    );
    terminal(paths, &record, &mut execution)?;
    execution.state = "pruning".into();
    atomic_json(&record, &execution)?;
    let slot = directory.join("slot.json");
    if slot.try_exists()? {
        let selected: PathBuf = serde_json::from_slice(&fs::read(&slot)?)?;
        if selected == record {
            fs::remove_file(&slot)?;
        }
    }
    if process.try_exists()? {
        fs::remove_dir_all(&process)?;
    }
    fs::remove_file(&lock_path)?;
    fs::remove_file(&record)?;
    std::fs::File::open(&directory)?.sync_all()?;
    Ok(())
}

/// Remove all of a job's gates only when every retained execution is settled.
pub(crate) fn forget_job(paths: &Paths, job_id: &str) -> Result<()> {
    let directory = paths.root.join("broker");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let prefix = format!("{job_id}-");
    let file_prefix = basename(&prefix)?;
    let mut keys = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|value| value == "json")
            && path
                .file_name()
                .is_some_and(|value| value.as_encoded_bytes().starts_with(file_prefix.as_bytes()))
        {
            let mut execution: Execution = serde_json::from_slice(&fs::read(&path)?)?;
            ensure!(
                execution.key.starts_with(&prefix),
                "job gate filename belongs to another execution"
            );
            let _owner = lock(
                &directory.join(format!("{}.lock", basename(&execution.key)?)),
                false,
            )?;
            terminal(paths, &path, &mut execution)?;
            keys.push(execution.key);
        }
    }
    for key in keys {
        forget(paths, &key)?;
    }
    Ok(())
}

pub(crate) fn run(paths: &Paths, key: &str, spec: &CommandSpec) -> Result<ProcessResult> {
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
        command: spec.clone(),
        state: "queued".into(),
        owner_pid: std::process::id(),
        result: None,
    };
    atomic_json(&record, &execution)?;
    let guard = lock(&directory.join("slot.lock"), true)?;
    let slot_path = directory.join("slot.json");
    if slot_path.exists() {
        let prior: PathBuf = serde_json::from_slice(&fs::read(&slot_path)?)?;
        ensure!(
            prior.parent() == Some(directory.as_path()) && prior != slot_path,
            "foreign broker slot record"
        );
        let mut previous: Execution = serde_json::from_slice(&fs::read(&prior)?)?;
        reconcile(paths, &prior, &mut previous)?;
        ensure!(
            previous.result.is_some(),
            "execution slot has unresolved execution {}; no new command is admitted",
            previous.key
        );
    }
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
    let entries = match fs::read_dir(paths.root.join("broker")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::json!({"schema":1,"executions":records}));
        }
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|v| v == "json")
            && path.file_name().is_none_or(|v| v != "slot.json")
        {
            records.push(serde_json::from_slice::<serde_json::Value>(&fs::read(
                path,
            )?)?);
        }
    }
    Ok(serde_json::json!({"schema":1,"executions":records}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixture() -> (tempfile::TempDir, Paths, CommandSpec) {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(temporary.path().join("work")).unwrap();
        let spec = CommandSpec {
            program: "/usr/bin/true".into(),
            args: vec![],
            cwd: paths.root.clone(),
            env: BTreeMap::new(),
            timeout_seconds: Some(5),
            stdin: None,
            context: None,
        };
        (temporary, paths, spec)
    }

    #[test]
    fn forgetting_a_job_prunes_its_gates_and_preserves_another_slots_owner() {
        let (_temporary, paths, spec) = fixture();
        assert!(run(&paths, "job-check", &spec).unwrap().success());
        assert!(run(&paths, "job-prepare", &spec).unwrap().success());
        assert!(run(&paths, "another-check", &spec).unwrap().success());
        let slot = paths.root.join("broker/slot.json");
        let retained_slot = fs::read(&slot).unwrap();
        forget_job(&paths, "job").unwrap();
        assert!(!recorded(&paths, "job-check").unwrap());
        assert!(!recorded(&paths, "job-prepare").unwrap());
        assert!(!paths.root.join("broker/job-check.process").exists());
        assert!(!paths.root.join("broker/job-check.lock").exists());
        assert_eq!(fs::read(&slot).unwrap(), retained_slot);
        assert!(recorded(&paths, "another-check").unwrap());
        forget_job(&paths, "another").unwrap();
        assert!(!slot.exists());
        forget_job(&paths, "job").unwrap();
    }

    #[test]
    fn forgetting_a_job_refuses_any_unresolved_gate_before_removing_diagnostics() {
        let (_temporary, paths, spec) = fixture();
        assert!(run(&paths, "job-check", &spec).unwrap().success());
        assert!(run(&paths, "job-prepare", &spec).unwrap().success());
        let record = paths.root.join("broker/job-prepare.json");
        let mut uncertain: Execution = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        uncertain.result = None;
        uncertain.state = "running".into();
        atomic_json(&record, &uncertain).unwrap();
        fs::remove_file(paths.root.join("broker/job-prepare.process/result.json")).unwrap();
        assert!(forget_job(&paths, "job").is_err());
        assert!(recorded(&paths, "job-check").unwrap());
        assert!(recorded(&paths, "job-prepare").unwrap());
        assert!(paths.root.join("broker/slot.json").exists());
    }

    #[test]
    fn forgetting_checks_terminal_correlation_and_can_finish_interrupted_cleanup() {
        let (_temporary, paths, spec) = fixture();
        assert!(run(&paths, "job-check", &spec).unwrap().success());
        let record = paths.root.join("broker/job-check.json");
        let mut execution: Execution = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        execution.result.as_mut().unwrap().stdout = "changed".into();
        atomic_json(&record, &execution).unwrap();
        assert!(forget(&paths, "job-check").is_err());
        assert!(record.exists());
        assert!(paths.root.join("broker/slot.json").exists());

        execution.result.as_mut().unwrap().stdout.clear();
        execution.state = "pruning".into();
        atomic_json(&record, &execution).unwrap();
        fs::remove_dir_all(paths.root.join("broker/job-check.process")).unwrap();
        forget(&paths, "job-check").unwrap();
        assert!(!record.exists());
        assert!(!paths.root.join("broker/slot.json").exists());
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
            context: None,
        };
        assert!(run(&paths, "one/one", &spec).unwrap().success());
        assert!(run(&paths, "one/one", &spec).unwrap().success());
        // Different keys that share a filename cannot reuse one execution.
        assert!(run(&paths, "one?one", &spec).is_err());
        let mut changed = spec;
        changed.args.push("changed".into());
        assert!(run(&paths, "one/one", &changed).is_err());
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
            context: None,
        };
        let failed = run(&paths, "lost", &spec).unwrap();
        assert!(failed.start_error.is_some());
        assert!(!failed.success());
        let next = CommandSpec {
            program: "/usr/bin/true".into(),
            ..spec.clone()
        };
        assert!(run(&paths, "after-start-failure", &next).unwrap().success());

        // Model an admitted gate whose process has no retained terminal result.
        let record = paths.root.join("broker/lost.json");
        let mut lost: Execution = serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        lost.state = "running".into();
        lost.result = None;
        atomic_json(&record, &lost).unwrap();
        fs::remove_file(record.with_extension("process").join("result.json")).unwrap();
        atomic_json(&paths.root.join("broker/slot.json"), &record).unwrap();
        assert!(run(&paths, "lost", &spec).is_err());
        assert!(run(&paths, "next", &next).is_err());
        assert!(run(&paths, "lost", &next).is_err());
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
            context: None,
        };
        assert!(run(&paths, "prior", &spec).unwrap().success());
        let record = paths.root.join("broker/prior.json");
        let mut interrupted: Execution =
            serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
        interrupted.result = None;
        interrupted.state = "running".into();
        atomic_json(&record, &interrupted).unwrap();
        assert!(run(&paths, "next", &spec).unwrap().success());
        let settled: Execution = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
        assert!(settled.result.unwrap().success());
    }
}
