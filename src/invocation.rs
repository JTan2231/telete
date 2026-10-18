//! Frozen command policy and candidate-specific invocation context.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::paths::Paths;
use crate::process::{CommandContext, CommandSpec};
use crate::store::Job;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandPolicy {
    pub check_script: PathBuf,
    pub host_program: PathBuf,
    pub environment: BTreeMap<String, String>,
}

impl CommandPolicy {
    pub(crate) fn capture(paths: &Paths) -> Result<Self> {
        // These are tool and locale selections, not credential values. Do not
        // retain the submission shell's unrestricted environment.
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
            "TELETE_TEST_THREADS",
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
        Ok(Self {
            check_script: "pipeline/cell/check.sh".into(),
            host_program: std::env::current_exe()?,
            environment,
        })
    }
}

pub(crate) fn executable(
    program: &std::path::Path,
    environment: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    if program.is_absolute() {
        return Ok(program.to_owned());
    }
    ensure!(
        program.components().count() == 1,
        "relative executable must be a bare name"
    );
    let search = environment
        .get("PATH")
        .context("command has no controlled PATH")?;
    for directory in std::env::split_paths(search) {
        ensure!(
            directory.is_absolute(),
            "command PATH must contain absolute directories"
        );
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "executable is absent from the controlled PATH: {}",
        program.display()
    )
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
    let policy = job
        .policy
        .commands
        .as_ref()
        .context("job has no frozen command policy")?;
    let context = directory(paths, job, operation)?.join("context.json");
    command.env = policy.environment.clone();
    command
        .env
        .insert("TELETE_CONTEXT".into(), context.display().to_string());
    let parameters = job.options.workflow.as_ref().map_or_else(
        || json!({"options": job.options, "signing_policy": job.policy.signing_policy}),
        |workflow| workflow.parameters.clone(),
    );
    command.context = Some(CommandContext {
        path: context,
        value: json!({
            "id": format!("{}/{operation}", job.id),
            "job_id": job.id,
            "stage": stage,
            "checkout": command.cwd,
            "source": {"input": job.input, "base": job.base, "candidate": job.candidate},
            "parameters": parameters,
            "inputs": inputs,
            "outputs": outputs,
        }),
    });
    Ok(command)
}

pub(crate) fn run(
    paths: &Paths,
    job: &Job,
    operation: &str,
    command: &CommandSpec,
) -> Result<crate::process::ProcessResult> {
    let directory = directory(paths, job, operation)?;
    // Read an admitted invocation rather than resolving its environment again.
    let request = directory.join("request.json");
    let saved = if request.try_exists()? {
        let saved: CommandSpec = serde_json::from_slice(&std::fs::read(request)?)?;
        ensure!(
            serde_json::to_value(&saved)? == serde_json::to_value(command)?,
            "invocation identity reused with another request"
        );
        saved
    } else {
        command.clone()
    };
    crate::process::supervise(paths, &saved, &directory)
}
