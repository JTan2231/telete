//! Project workflow stages reuse retained commands and the existing gate broker.

use super::{
    BTreeMap, Context, Job, Phase, Result, StageRun, Worker, bail, ensure, fs, git, json, store,
};
use crate::broker::{self, ResourceClass};
use crate::invocation;
use crate::process::CommandSpec;

impl Worker<'_> {
    fn workflow_command(&self, job: &Job, stage: &str, operation: &str) -> Result<CommandSpec> {
        let workflow = job.options.workflow.as_ref().context("workflow absent")?;
        let selected = match stage {
            "check" => &workflow.check,
            "prepare" => workflow
                .prepare
                .as_ref()
                .context("preparation not selected")?,
            "deploy" => workflow
                .deploy
                .as_ref()
                .context("deployment not selected")?,
            _ => bail!("unknown workflow stage"),
        };
        let preparation = job
            .workflow_runs
            .iter()
            .rev()
            .find(|run| {
                run.stage == "prepare"
                    && Some(&run.candidate) == job.candidate.as_ref()
                    && run.result.success()
            })
            .map(|run| {
                self.paths
                    .job(&job.id.0)
                    .join(format!("{}.json", run.operation))
            });
        let output = self.paths.job(&job.id.0).join("outputs").join(operation);
        let mut command = invocation::bind(
            self.paths,
            job,
            operation,
            stage,
            &json!({"preparation": preparation}),
            &json!({"directory": output}),
            CommandSpec {
                program: selected.program(&self.worktree(job)),
                args: selected.args.clone(),
                cwd: self.worktree(job),
                env: BTreeMap::new(),
                timeout_seconds: selected.timeout_seconds,
                stdin: None,
                confined: stage != "deploy" && !cfg!(test),
                context: None,
            },
        )?;
        command.env.extend(selected.env.clone());
        Ok(command)
    }

    fn workflow_key(job: &Job, operation: &str) -> String {
        format!("{}-{operation}", job.id)
    }

    fn require_workflow_success(job: &Job, stage: &str) -> Result<()> {
        ensure!(
            job.workflow_runs
                .iter()
                .rev()
                .find(|run| {
                    run.stage == stage && Some(&run.candidate) == job.candidate.as_ref()
                })
                .is_some_and(|run| run.result.success()),
            "workflow has no successful {stage} for this candidate"
        );
        Ok(())
    }

    // Reconcile only terminal broker evidence. No recovery path launches commands.
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
        let expected = format!("workflow-{stage}-{}", job.workflow_runs.len());
        ensure!(
            operation.name == expected
                && operation.result == self.paths.job(&job.id.0).join(format!("{expected}.json"))
                && (job.phase == operation.phase
                    || (job.phase == Phase::Blocked && job.stopped_phase == Some(operation.phase))),
            "workflow intent does not match its stage and owned receipt"
        );
        ensure!(
            Some(&operation.candidate) == job.candidate.as_ref(),
            "workflow intent names another candidate"
        );
        let command = self.workflow_command(job, stage, &operation.name)?;
        let saved: CommandSpec = serde_json::from_slice(&fs::read(
            invocation::directory(self.paths, job, &operation.name)?.join("request.json"),
        )?)?;
        ensure!(
            serde_json::to_value(&saved)? == serde_json::to_value(&command)?,
            "workflow invocation differs from frozen policy"
        );
        let result = broker::completed(
            self.paths,
            &Self::workflow_key(job, &operation.name),
            ResourceClass::Heavy,
            &saved,
        )?;
        let run = StageRun {
            stage: stage.into(),
            candidate: operation.candidate.clone(),
            operation: operation.name.clone(),
            command: saved,
            result,
            stamp: operation.started,
        };
        if operation.result.exists() {
            let previous: StageRun = serde_json::from_slice(&fs::read(&operation.result)?)?;
            ensure!(
                serde_json::to_value(&previous)? == serde_json::to_value(&run)?,
                "workflow receipt differs from terminal process evidence"
            );
        } else {
            store::atomic_json(&operation.result, &run)?;
        }
        Ok(())
    }

    fn verify_workflow_stage(&self, job: &Job, stage: &str) -> Result<()> {
        if stage != "check" {
            Self::require_workflow_success(job, "check")?;
        }
        if stage == "deploy" {
            Self::require_workflow_success(job, "prepare")?;
            if job
                .options
                .workflow
                .as_ref()
                .is_some_and(|workflow| workflow.accept)
            {
                ensure!(
                    job.accepted
                        && git::ref_value(
                            self.paths,
                            &self.config.repository,
                            self.config.accepted_ref()
                        )?
                        .as_ref()
                            == job.candidate.as_ref(),
                    "accepted source changed before deployment"
                );
            }
        }
        Ok(())
    }

    pub(super) fn workflow_stage(&self, job: &mut Job, stage: &str) -> Result<()> {
        self.verify_workflow_stage(job, stage)?;
        let candidate = Self::candidate(job)?.clone();
        if job.cancel_requested && job.operation.is_none() {
            return self.finish(
                job,
                Phase::Cancelled,
                format!("Cancelled before workflow {stage} admission"),
                false,
            );
        }
        let operation = format!("workflow-{stage}-{}", job.workflow_runs.len());
        let receipt = self.directory(job)?.join(format!("{operation}.json"));
        let command = self.workflow_command(job, stage, &operation)?;
        let directory = invocation::directory(self.paths, job, &operation)?;
        let request = directory.join("request.json");
        if request.exists() {
            let saved: CommandSpec = serde_json::from_slice(&fs::read(&request)?)?;
            ensure!(
                serde_json::to_value(&saved)? == serde_json::to_value(&command)?,
                "workflow invocation identity changed"
            );
        } else {
            ensure!(
                job.operation.is_none(),
                "admitted workflow invocation is missing"
            );
            git::clean_candidate(self.paths, &self.worktree(job), &candidate)?;
            if stage == "check"
                && !job
                    .options
                    .workflow
                    .as_ref()
                    .context("workflow absent")?
                    .check
                    .program
                    .is_absolute()
            {
                let program = command
                    .program
                    .canonicalize()
                    .context("candidate check program is unavailable")?;
                ensure!(
                    program.starts_with(self.worktree(job).canonicalize()?),
                    "candidate check program escapes its checkout"
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
        if self.begin_operation(job, operation.clone(), receipt.clone())? {
            broker::run(
                self.paths,
                &Self::workflow_key(job, &operation),
                ResourceClass::Heavy,
                &command,
            )?;
        }
        self.reconcile_workflow(job)?;
        let run: StageRun = serde_json::from_slice(&fs::read(receipt)?)?;
        let success = run.result.success();
        let diagnostic = self.directory(job)?.join(format!("{operation}.log"));
        store::atomic_bytes(&diagnostic, run.result.diagnostics().as_bytes())?;
        job.workflow_runs.push(run);
        job.operation = None;
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if job.cancel_requested {
            return self.finish(
                job,
                Phase::Cancelled,
                format!("Cancelled after {stage} settled"),
                false,
            );
        }
        if stage != "deploy" {
            git::clean_candidate(self.paths, &self.worktree(job), &candidate)?;
        }
        if !success {
            return self.finish(
                job,
                Phase::Failed,
                format!("Workflow {stage} failed for {candidate}"),
                false,
            );
        }
        self.workflow_next(job, stage)
    }

    fn workflow_next(&self, job: &mut Job, completed: &str) -> Result<()> {
        let workflow = job.options.workflow.as_ref().context("workflow absent")?;
        if completed == "check" && workflow.prepare.is_some() {
            return self.save(job, Phase::Preparing);
        }
        if matches!(completed, "check" | "prepare") && workflow.accept {
            return self.save(job, Phase::Accepting);
        }
        if completed != "deploy" && workflow.deploy.is_some() {
            return self.save(job, Phase::Deploying);
        }
        self.finish(
            job,
            Phase::Succeeded,
            format!(
                "Requested workflow stages succeeded for {}",
                Self::candidate(job)?
            ),
            false,
        )
    }

    pub(super) fn workflow_accept(&self, job: &mut Job) -> Result<()> {
        let workflow = job.options.workflow.as_ref().context("workflow absent")?;
        ensure!(
            workflow.accept && workflow.integrate,
            "workflow did not select conditional acceptance"
        );
        Self::require_workflow_success(job, "check")?;
        if workflow.prepare.is_some() {
            Self::require_workflow_success(job, "prepare")?;
        }
        let candidate = Self::candidate(job)?.clone();
        git::clean_candidate(self.paths, &self.worktree(job), &candidate)?;
        git::advance(
            self.paths,
            &self.config.repository,
            self.config.accepted_ref(),
            Some(Self::base(job)?),
            &candidate,
        )?;
        job.accepted = true;
        self.store.save(job)?;
        self.workflow_next(job, "accept")
    }
}
