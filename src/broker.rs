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
    for entry in fs::read_dir(paths.root.join("broker"))? {
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
            confined: false,
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
            confined: false,
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
