//! Command overrides and run-specific invocation context.

use std::path::PathBuf;

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use crate::paths::Paths;
use crate::process::{CommandContext, CommandSpec};
use crate::store::Job;

pub(crate) fn directory(paths: &Paths, job: &Job, operation: &str) -> Result<PathBuf> {
    ensure!(
        !operation.is_empty()
            && operation
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "invalid invocation operation"
    );
    Ok(paths.job(&job.id.0).join("commands").join(operation))
}

pub(crate) fn bind(
    paths: &Paths,
    job: &Job,
    operation: &str,
    stage: &str,
    inputs: &Value,
    outputs: &Value,
    mut command: CommandSpec,
) -> Result<CommandSpec> {
    let context = directory(paths, job, operation)?.join("context.json");
    command
        .env
        .insert("TELETE_CONTEXT".into(), context.display().to_string());
    let mut source = json!({"commit": job.execution_commit()});
    if job.workflow.fix.is_some()
        || job.workflow.analyze.is_some()
        || job.workflow.repair.is_some()
        || job.workflow.batch.is_some()
    {
        source["submitted_commit"] = json!(job.commit);
        source["result_ref"] = json!(crate::git::result_ref(&job.id.0)?);
    }
    if let Some(commit) = &job.analyzed_commit {
        source["analyzed_commit"] = json!(commit);
    }
    // Reconstruct a repair invocation with its original source during handoff recovery.
    if let Some(commit) = &job.repaired_commit
        && (stage != "repair" || job.resulting_commit.as_ref() == Some(commit))
    {
        source["repaired_commit"] = json!(commit);
    }
    if let Some(release) = &job.release_id {
        source["release_id"] = json!(release);
        source["checked_commit"] = json!(job.effective_commit());
    }
    command.context = Some(CommandContext {
        path: context,
        value: json!({
            "id": format!("{}/{operation}", job.id),
            "job_id": job.id,
            "stage": stage,
            "checkout": command.cwd,
            "source": source,
            "parameters": job.workflow.parameters,
            "inputs": inputs,
            "outputs": outputs,
        }),
    });
    Ok(command)
}
