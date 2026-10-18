//! Current command selection and reconciliation; recovery never launches work.

use super::{Context, Job, Phase, Result, StageRun, Worker, bail, ensure, fs, json, store};
use crate::{broker, invocation, process::CommandSpec};

impl Worker<'_> {
    fn workflow_command(&self, job: &Job, stage: &str, operation: &str) -> Result<CommandSpec> {
        let selected = match stage {
            "check" => &job.workflow.check,
            "prepare" => job
                .workflow
                .prepare
                .as_ref()
                .context("preparation not selected")?,
            "deploy" => job
                .workflow
                .deploy
                .as_ref()
                .context("deployment not selected")?,
            _ => bail!("unknown workflow stage"),
        };
        let preparation = job
            .runs
            .iter()
            .find(|run| run.stage == "prepare" && run.result.success())
            .and_then(|run| run.command.context.as_ref())
            .map(|context| context.value["outputs"]["directory"].clone());
        let output = self.paths.job(&job.id.0).join("outputs").join(operation);
        crate::invocation::bind(
            self.paths,
            job,
            operation,
            stage,
            &json!({"preparation":preparation}),
            &json!({"directory":output}),
            CommandSpec {
                program: selected.program(&self.worktree(job)),
                args: selected.args.clone(),
                cwd: self.worktree(job),
                env: selected.env.clone(),
                timeout_seconds: selected.timeout_seconds,
                stdin: None,
                context: None,
            },
        )
    }

    fn workflow_key(job: &Job, operation: &str) -> String {
        format!("{}-{operation}", job.id)
    }

    pub(super) fn reconcile_workflow(&self, job: &Job) -> Result<()> {
        let operation = job
            .operation
            .as_ref()
            .context("workflow has no retained intent")?;
        let stage = match operation.phase {
            Phase::Checking => "check",
            Phase::Preparing => "prepare",
            Phase::Deploying => "deploy",
            _ => bail!("workflow operation has another phase"),
        };
        let expected = format!("workflow-{stage}-{}", job.runs.len());
        ensure!(
            operation.name == expected
                && operation.result == self.paths.job(&job.id.0).join(format!("{expected}.json"))
                && operation.commit == job.commit
                && (job.phase == operation.phase
                    || (job.phase == Phase::Blocked && job.stopped_phase == Some(operation.phase))),
            "workflow intent does not match its stage and owned result"
        );
        let command = self.workflow_command(job, stage, &operation.name)?;
        let saved: CommandSpec = serde_json::from_slice(&fs::read(
            invocation::directory(self.paths, job, &operation.name)?.join("request.json"),
        )?)?;
        ensure!(
            serde_json::to_value(&saved)? == serde_json::to_value(&command)?,
            "workflow command differs from selected configuration"
        );
        let result = broker::completed(
            self.paths,
            &Self::workflow_key(job, &operation.name),
            &saved,
        )?;
        let run = StageRun {
            stage: stage.into(),
            commit: job.commit.clone(),
            operation: operation.name.clone(),
            command: saved,
            result,
            stamp: operation.started,
        };
        if operation.result.exists() {
            let previous: StageRun = serde_json::from_slice(&fs::read(&operation.result)?)?;
            ensure!(
                serde_json::to_value(&previous)? == serde_json::to_value(&run)?,
                "command result differs from terminal process evidence"
            );
        } else {
            store::atomic_json(&operation.result, &run)?;
        }
        Ok(())
    }

    pub(super) fn workflow_stage(&self, job: &mut Job, stage: &str) -> Result<()> {
        if stage != "check" {
            ensure!(
                job.runs
                    .iter()
                    .any(|run| run.stage == "check" && run.result.success()),
                "workflow has no successful check"
            );
        }
        let operation = format!("workflow-{stage}-{}", job.runs.len());
        let result_path = self.directory(job)?.join(format!("{operation}.json"));
        let command = self.workflow_command(job, stage, &operation)?;
        let request = invocation::directory(self.paths, job, &operation)?.join("request.json");
        if request.exists() {
            let saved: CommandSpec = serde_json::from_slice(&fs::read(&request)?)?;
            ensure!(
                serde_json::to_value(saved)? == serde_json::to_value(&command)?,
                "workflow invocation identity changed"
            );
        } else {
            ensure!(
                job.operation.is_none(),
                "admitted command request is missing"
            );
            let selected = match stage {
                "check" => &job.workflow.check,
                "prepare" => job
                    .workflow
                    .prepare
                    .as_ref()
                    .context("preparation not selected")?,
                "deploy" => job
                    .workflow
                    .deploy
                    .as_ref()
                    .context("deployment not selected")?,
                _ => bail!("unknown workflow stage"),
            };
            // A missing absolute executable is a recorded start failure. A
            // relative path must also remain inside its source checkout.
            if !selected.program.is_absolute() && command.program.exists() {
                ensure!(
                    command
                        .program
                        .canonicalize()?
                        .starts_with(self.worktree(job).canonicalize()?),
                    "relative workflow program escapes its checkout"
                );
            }
            store::private_directory(&self.paths.job(&job.id.0).join("outputs").join(&operation))?;
            store::atomic_json(&request, &command)?;
        }
        let _deployment = if stage == "deploy" {
            Some(store::lock(
                &self.paths.root.join("deployments/deployment.lock"),
                false,
            )?)
        } else {
            None
        };
        if self.begin_operation(job, operation.clone(), result_path.clone())? {
            broker::run(self.paths, &Self::workflow_key(job, &operation), &command)?;
        }
        self.reconcile_workflow(job)?;
        let run: StageRun = serde_json::from_slice(&fs::read(result_path)?)?;
        let success = run.result.success();
        store::atomic_bytes(
            &self.directory(job)?.join(format!("{operation}.log")),
            run.result.diagnostics().as_bytes(),
        )?;
        job.runs.push(run);
        job.operation = None;
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if !success {
            return self.finish(
                job,
                Phase::Failed,
                format!("Workflow {stage} failed for {}", job.commit),
                false,
            );
        }
        if job.cancel_requested {
            return self.finish(
                job,
                Phase::Cancelled,
                format!("Cancelled after {stage} settled"),
                false,
            );
        }
        if stage == "check" && job.workflow.prepare.is_some() {
            return self.save(job, Phase::Preparing);
        }
        if stage != "deploy" && job.workflow.deploy.is_some() {
            return self.save(job, Phase::Deploying);
        }
        self.finish(
            job,
            Phase::Succeeded,
            "Configured workflow succeeded".into(),
            false,
        )
    }
}
