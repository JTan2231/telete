//! Current command selection and reconciliation; recovery never launches work.

use super::{Context, Job, Phase, Result, StageRun, Worker, bail, ensure, fs, json, store};
use crate::{broker, invocation, process::CommandSpec};

impl Worker<'_> {
    fn workflow_command(&self, job: &Job, stage: &str, operation: &str) -> Result<CommandSpec> {
        let selected = job.workflow.command(stage)?;
        let preparation = job
            .runs
            .iter()
            .find(|run| run.stage == "prepare" && run.result.success())
            .and_then(|run| run.command.context.as_ref())
            .map(|context| context.value["outputs"]["directory"].clone());
        let fix = job
            .runs
            .iter()
            .find(|run| run.stage == "fix")
            .and_then(|run| run.command.context.as_ref())
            .map(|context| context.value["outputs"]["directory"].clone());
        let analysis = job
            .runs
            .iter()
            .find(|run| run.stage == "analyze")
            .and_then(|run| run.command.context.as_ref())
            .map(|context| context.value["outputs"]["directory"].clone());
        let mut inputs = json!({"preparation":preparation});
        if job.workflow.analyze.is_some() {
            inputs["analysis"] = json!(analysis);
        }
        if job.workflow.fix.is_some() {
            inputs["fix"] = json!(fix);
        }
        if job.batch.is_some() {
            inputs["batch"] = self.batch_input(job)?;
        }
        let output = self.paths.job(&job.id.0).join("outputs").join(operation);
        crate::invocation::bind(
            self.paths,
            job,
            operation,
            stage,
            &inputs,
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
            Phase::Fixing => "fix",
            Phase::Analyzing => "analyze",
            Phase::Checking => "check",
            Phase::Composing => "compose",
            Phase::Preparing => "prepare",
            Phase::Deploying => "deploy",
            _ => bail!("workflow operation has another phase"),
        };
        let expected = format!("workflow-{stage}-{}", job.runs.len());
        ensure!(
            operation.name == expected
                && operation.result == self.paths.job(&job.id.0).join(format!("{expected}.json"))
                && operation.commit == *job.execution_commit()
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
            commit: job.execution_commit().clone(),
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
        if stage == "analyze" && job.analyzed_commit.is_none() {
            let commit = job.effective_commit().clone();
            crate::git::advance(
                self.paths,
                &self.config.common_git_dir,
                &crate::git::result_ref(&job.id.0)?,
                None,
                &commit,
            )?;
            job.resulting_commit = Some(commit.clone());
            job.analyzed_commit = Some(commit);
            self.store.save(job)?;
        }
        if !matches!(stage, "fix" | "analyze" | "check") {
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
            let selected = job.workflow.command(stage)?;
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
        self.complete_stage(job, stage, run)
    }

    fn complete_stage(&self, job: &mut Job, stage: &str, run: StageRun) -> Result<()> {
        let success = run.result.success();
        let repairable = (stage == "analyze"
            || (stage == "check" && job.workflow.analyze.is_some()))
            && run
                .result
                .exit_code
                .is_some_and(|code| code != 0 && run.result.exited_with(&[code]));
        store::atomic_bytes(
            &self.directory(job)?.join(format!("{}.log", run.operation)),
            run.result.diagnostics().as_bytes(),
        )?;
        // Terminal process evidence is known before source finalization.
        job.operation = None;
        if stage == "fix" {
            job.resulting_commit = Some(crate::git::retain_fixes(
                self.paths,
                &self.config.common_git_dir,
                &self.worktree(job),
                job,
            )?);
        } else if stage == "compose" {
            if success {
                self.retain_composition(job, &run)?;
            }
        } else if job.workflow.fix.is_some()
            || job.workflow.analyze.is_some()
            || job.workflow.batch.is_some()
        {
            crate::git::unchanged(self.paths, &self.worktree(job), job.execution_commit())?;
        }
        job.runs.push(run);
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if !success && repairable && !job.cancel_requested {
            job.stopped_phase = Some(job.phase);
            job.outcome_message = Some(format!(
                "Workflow {stage} awaits agent repair for {}",
                job.effective_commit()
            ));
            return self.save(job, Phase::AwaitingRepair);
        }
        if !success {
            return self.finish(
                job,
                Phase::Failed,
                format!("Workflow {stage} failed for {}", job.effective_commit()),
                false,
            );
        }
        // A successful deployment is the cumulative base even if cancellation
        // was requested while that command was already running.
        if stage == "deploy" && job.batch.is_some() {
            self.record_release(job)?;
        }
        if job.cancel_requested {
            return self.finish(
                job,
                Phase::Cancelled,
                format!("Cancelled after {stage} settled"),
                false,
            );
        }
        if stage == "fix" {
            return self.save(
                job,
                if job.workflow.analyze.is_some() {
                    Phase::Analyzing
                } else {
                    Phase::Checking
                },
            );
        }
        if stage == "analyze" {
            return self.save(job, Phase::Checking);
        }
        if stage == "check" && job.workflow.batch.is_some() {
            return self.defer_release(job);
        }
        if matches!(stage, "check" | "compose") && job.workflow.prepare.is_some() {
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
