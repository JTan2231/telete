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
    command.context = Some(CommandContext {
        path: context,
        value: json!({
            "id": format!("{}/{operation}", job.id),
            "job_id": job.id,
            "stage": stage,
            "checkout": command.cwd,
            "source": {"commit": job.commit},
            "parameters": job.workflow.parameters,
            "inputs": inputs,
            "outputs": outputs,
        }),
    });
    Ok(command)
}
