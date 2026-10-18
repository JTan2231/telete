//! Frozen command environment and candidate-specific invocation context.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use crate::paths::Paths;
use crate::process::{CommandContext, CommandSpec};
use crate::store::Job;

pub(crate) fn capture_environment(paths: &Paths) -> Result<BTreeMap<String, String>> {
    // Retain tool and locale selections, not the unrestricted submission shell.
    let mut environment = BTreeMap::new();
    for name in [
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "RUSTUP_TOOLCHAIN",
        "SDKROOT",
        "DEVELOPER_DIR",
        "MACOSX_DEPLOYMENT_TARGET",
    ] {
        if let Some(value) = std::env::var_os(name) {
            environment.insert(
                name.into(),
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("non-UTF-8 command environment: {name}"))?,
            );
        }
    }
    environment.extend(paths.environment());
    environment.insert("CARGO_BUILD_JOBS".into(), "2".into());
    Ok(environment)
}

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
    command.env = job.environment.clone();
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
            "source": {"input": job.input, "base": job.base, "candidate": job.candidate},
            "parameters": job.workflow.parameters,
            "inputs": inputs,
            "outputs": outputs,
        }),
    });
    Ok(command)
}
