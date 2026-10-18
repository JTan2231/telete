//! Cell work commands. The manager retains job and source-acceptance authority.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::deployment::{self, DeploymentResult, Preparation, PrepareError};
use crate::git;
use crate::inventory::Inventory;
use crate::invocation;
use crate::model::CommitId;
use crate::paths::Paths;
use crate::process::{self, CommandSpec};
use crate::signing;
use crate::store::{self, Job, Phase, Store};
use crate::validation::ValidationState;

#[derive(Serialize, Deserialize)]
pub(crate) struct PreparationOutcome {
    pub prepared: Option<Preparation>,
    pub compilation_failure: Option<String>,
}

impl PreparationOutcome {
    fn from_result(result: Result<Preparation>) -> Result<Self> {
        match result {
            Ok(prepared) => Ok(Self {
                prepared: Some(prepared),
                compilation_failure: None,
            }),
            Err(error) => match error.downcast_ref::<PrepareError>() {
                Some(PrepareError::Compilation(diagnostic)) => Ok(Self {
                    prepared: None,
                    compilation_failure: Some(diagnostic.clone()),
                }),
                None => Err(error),
            },
        }
    }
}

fn worktree(paths: &Paths, job: &Job) -> PathBuf {
    paths.job(&job.id.0).join("worktree")
}

pub(crate) fn production_products(paths: &Paths, job: &Job) -> Result<Vec<String>> {
    let report = &job
        .validations
        .last()
        .context("production has no successful validation")?
        .report;
    ensure!(
        report.state == ValidationState::Passed
            && job.candidate.as_ref() == Some(&report.candidate),
        "production validation names another candidate"
    );
    let mut names = report.products.clone();
    names.extend(report.platform_products.iter().cloned());
    names.extend(job.options.deploy.iter().cloned());
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut selected = Inventory::load(&worktree(paths, job))?
        .select(&names)?
        .iter()
        .map(|product| {
            if product.id.to_string() == "decisions" {
                "krisis".into()
            } else {
                product.id.to_string()
            }
        })
        .collect::<Vec<_>>();
    selected.sort();
    selected.dedup();
    Ok(selected)
}

fn command(paths: &Paths, job: &Job, phase: &str, args: Vec<String>) -> Result<CommandSpec> {
    let mut arguments = vec![
        "--state".into(),
        paths.root.display().to_string(),
        phase.into(),
        "--job".into(),
        job.id.to_string(),
    ];
    arguments.extend(args);
    Ok(CommandSpec {
        program: job
            .policy
            .commands
            .as_ref()
            .map_or_else(std::env::current_exe, |policy| {
                Ok(policy.host_program.clone())
            })?,
        args: arguments,
        cwd: worktree(paths, job),
        env: paths.environment(),
        timeout_seconds: None,
        stdin: None,
        confined: false,
        context: None,
    })
}

pub(crate) fn preparation_command(paths: &Paths, job: &Job) -> Result<CommandSpec> {
    let command = command(
        paths,
        job,
        "internal-cell-prepare",
        vec![
            "--generation".into(),
            job.preparation_generation.to_string(),
            "--candidate".into(),
            job.candidate
                .as_ref()
                .context("preparation has no candidate")?
                .to_string(),
        ],
    )?;
    if job.policy.commands.is_none() {
        return Ok(command);
    }
    let operation = format!("production-{}", job.preparation_generation);
    invocation::bind(
        paths,
        job,
        &operation,
        "prepare",
        &serde_json::json!({}),
        &serde_json::json!({"receipt": paths.job(&job.id.0).join(format!("{operation}.json")), "staging": paths.job(&job.id.0).join(&operation)}),
        command,
    )
}

pub(crate) fn deployment_command(paths: &Paths, job: &Job) -> Result<CommandSpec> {
    let request = job
        .deployment_request
        .as_ref()
        .context("deployment has no frozen request")?;
    let command = command(
        paths,
        job,
        "internal-cell-deploy",
        vec![
            "--request-id".into(),
            request.id.clone(),
            "--candidate".into(),
            request.source.to_string(),
        ],
    )?;
    if job.policy.commands.is_none() {
        return Ok(command);
    }
    invocation::bind(
        paths,
        job,
        "deployment",
        "deploy",
        &serde_json::to_value(request)?,
        &serde_json::json!({"request_id": request.id}),
        command,
    )
}

pub(crate) fn prepare(paths: &Paths, job: &Job) -> Result<()> {
    let command = preparation_command(paths, job)?;
    let result = if job.policy.commands.is_some() {
        invocation::run(
            paths,
            job,
            &format!("production-{}", job.preparation_generation),
            &command,
        )?
    } else {
        process::run(paths, &command)?
    };
    ensure!(
        result.success(),
        "Cell preparation command failed: {}",
        result.diagnostics()
    );
    Ok(())
}

pub(crate) fn deploy(paths: &Paths, job: &Job) -> Result<DeploymentResult> {
    let command = deployment_command(paths, job)?;
    let result = if job.policy.commands.is_some() {
        invocation::run(paths, job, "deployment", &command)
    } else {
        process::run(paths, &command)
    };
    let request = job
        .deployment_request
        .as_ref()
        .context("deployment has no frozen request")?;
    if let Some(observed) = deployment::observe(paths, &request.id)? {
        return Ok(observed);
    }
    let result = result?;
    ensure!(
        result.success(),
        "Cell deployment command failed: {}",
        result.diagnostics()
    );
    anyhow::bail!("Cell deployment command has no authoritative receipt")
}

fn retained_job(paths: &Paths, id: &str) -> Result<Job> {
    let store = Store::open(&paths.root, false)?;
    let config = store.config()?;
    ensure!(
        git::common(paths, &config.repository)? == config.common_git_dir,
        "configured Git repository identity changed"
    );
    store.job(id)
}

pub(crate) fn execute_prepare(
    paths: &Paths,
    id: &str,
    generation: u64,
    candidate: &CommitId,
) -> Result<()> {
    let job = retained_job(paths, id)?;
    let directory = paths.job(&job.id.0);
    let name = format!("production-{generation}");
    let receipt = directory.join(format!("{name}.json"));
    let operation = job
        .operation
        .as_ref()
        .context("Cell preparation has no admitted operation")?;
    ensure!(
        job.phase == Phase::Preparing
            && !job.accepted
            && job.preparation_generation == generation
            && job.candidate.as_ref() == Some(candidate)
            && operation.phase == Phase::Preparing
            && operation.name == name
            && operation.candidate == *candidate
            && operation.result == receipt,
        "Cell preparation command does not match the admitted operation"
    );
    ensure!(
        !receipt.try_exists()?,
        "Cell preparation already has a terminal receipt"
    );
    ensure!(
        signing::selected(paths)? == *job.policy.signing()?,
        "host signing selection changed after admission"
    );
    let products = production_products(paths, &job)?;
    let result = if products.is_empty() {
        Ok(Preparation {
            schema: 1,
            source: candidate.clone(),
            products,
            signing_policy: job.policy.signing()?.clone(),
            candidates: BTreeMap::new(),
            release_check: true,
        })
    } else {
        deployment::prepare(
            paths,
            &worktree(paths, &job),
            candidate,
            &products,
            job.policy.signing()?,
            &directory.join(name),
        )
    };
    store::atomic_json(&receipt, &PreparationOutcome::from_result(result)?)
}

pub(crate) fn execute_deploy(
    paths: &Paths,
    id: &str,
    request_id: &str,
    candidate: &CommitId,
) -> Result<()> {
    let job = retained_job(paths, id)?;
    let request = job
        .deployment_request
        .as_ref()
        .context("Cell deployment has no admitted request")?;
    ensure!(
        job.phase == Phase::Deploying
            && job.accepted
            && !job.options.no_deploy
            && job.candidate.as_ref() == Some(candidate)
            && request.id == request_id
            && request.source == *candidate
            && request.prepared.source == *candidate
            && !request.products.is_empty(),
        "Cell deployment command does not match the admitted request"
    );
    let config = Store::open(&paths.root, false)?.config()?;
    ensure!(
        git::ref_value(paths, &config.repository, git::ACCEPTED)?.as_ref() == Some(candidate),
        "accepted source changed before deployment"
    );
    ensure!(
        request.prepared.signing_policy == *job.policy.signing()?,
        "deployment preparation names another signing policy"
    );
    deployment::execute(
        paths,
        &worktree(paths, &job),
        &request.id,
        &request.prepared,
        &request.products,
        &job.options.settings,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_compilation_failures_become_repair_outcomes() -> Result<()> {
        let outcome = PreparationOutcome::from_result(Err(PrepareError::Compilation(
            "compiler diagnostic".into(),
        )
        .into()))?;
        assert_eq!(
            serde_json::to_value(outcome)?,
            serde_json::json!({"prepared":null,"compilation_failure":"compiler diagnostic"})
        );
        assert!(
            PreparationOutcome::from_result(Err(anyhow::anyhow!("signing identity unavailable")))
                .is_err()
        );
        Ok(())
    }
}
