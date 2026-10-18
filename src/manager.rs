//! Serial execution of configured committed-source workflows.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::git;
use crate::model::{CommitId, JobId};
use crate::paths::Paths;
use crate::store::{self, Cleanup, Config, Job, Operation, Phase, Store};
use crate::workflow::{ProjectConfig, StageRun, Workflow};

#[path = "workflow_manager.rs"]
mod workflow_manager;

/// Validate the project before creating any operational state.
pub(crate) fn select(project: &ProjectConfig, create: bool) -> Result<Paths> {
    crate::repository_queue::select(project, create)
}

fn configured(paths: &Paths) -> Result<(Store, Config)> {
    let store = Store::open(&paths.root)?;
    let config = store.config()?;
    paths.verify_config(&config)?;
    ensure!(
        git::common(paths, &config.common_git_dir)? == config.common_git_dir,
        "configured Git repository identity changed"
    );
    Ok((store, config))
}

pub(crate) fn submit(
    paths: &Paths,
    repo: &Path,
    revision: &str,
    workflow: Workflow,
) -> Result<Value> {
    workflow.validate()?;
    let (store, config) = configured(paths)?;
    ensure!(
        git::common(paths, repo)? == config.common_git_dir,
        "submit must select a checkout of the configured Git repository"
    );
    let commit = git::commit(paths, repo, revision)?;
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let job = new_job(
        JobId::new(uuid::Uuid::now_v7().to_string())?,
        commit,
        workflow,
    );
    let mut admitted = store.transaction(|| store.insert(&job))?;
    // A queued record makes interrupted admission visible. Retain its commit
    // and private checkout before acknowledging admission, so removing the
    // submitting development worktree does not affect accepted work.
    let retained = (|| {
        git::advance(
            paths,
            &config.common_git_dir,
            &git::private_ref(&job.id.0)?,
            None,
            &job.commit,
        )?;
        git::ensure_worktree(
            paths,
            &config.common_git_dir,
            &paths.job(&job.id.0).join("worktree"),
            &job.commit,
        )
    })();
    if let Err(error) = retained {
        let message = format!("could not retain submitted source: {error:#}");
        admitted.phase = Phase::Failed;
        admitted.outcome = Some(Phase::Failed);
        admitted.stopped_phase = Some(Phase::Queued);
        admitted.last_error = Some(message.clone());
        admitted.outcome_message = Some(message.clone());
        store.save(&mut admitted)?;
        bail!("job {} failed: {message}", admitted.id);
    }
    admitted.output()
}

fn new_job(id: JobId, commit: CommitId, workflow: Workflow) -> Job {
    Job {
        id,
        sequence: 0,
        revision: 0,
        phase: Phase::Queued,
        cancel_requested: false,
        created: store::now(),
        updated: store::now(),
        commit,
        workflow,
        runs: Vec::new(),
        operation: None,
        outcome: None,
        outcome_message: None,
        stopped_phase: None,
        unresolved: false,
        last_error: None,
        cleanup: Cleanup::default(),
    }
}

fn job_status(store: &Store, id: &str) -> Result<Value> {
    if let Some(completion) = store.completion(id)? {
        return Ok(completion);
    }
    store.job(id)?.output()
}

pub(crate) fn status(paths: &Paths, id: Option<&str>) -> Result<Value> {
    if store::probe_config(&paths.root)?.is_none() {
        ensure!(id.is_none(), "no retained result for this job");
        return Ok(json!({"schema_version":3,"paused":false,"active":null,
            "queue":[],"failure":null,"completions":[],"maintenance_owners":[]}));
    }
    let (store, config) = configured(paths)?;
    if let Some(id) = id {
        return job_status(&store, id);
    }
    let jobs = store.jobs()?;
    let queue = jobs
        .iter()
        .filter(|job| job.phase == Phase::Queued)
        .map(Job::output)
        .collect::<Result<Vec<_>>>()?;
    let failure = jobs
        .iter()
        .rev()
        .find(|job| job.phase == Phase::Failed)
        .map(Job::output)
        .transpose()?;
    Ok(
        json!({"schema_version":3,"version":env!("CARGO_PKG_VERSION"),
        "paused":store.paused()?,"config":config,
        "active":store.active()?.map(|job|job.output()).transpose()?,
        "queue":queue,"failure":failure,"completions":store.completion_list()?,
        "maintenance_owners":store.holds()?}),
    )
}

pub(crate) async fn wait(paths: &Paths, id: &str, timeout_seconds: u64) -> Result<Value> {
    ensure!(timeout_seconds > 0, "wait timeout must be positive");
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let value = status(paths, Some(id))?;
        let phase = value["phase"].as_str().context("job has no phase")?;
        if matches!(phase, "succeeded" | "failed" | "cancelled" | "blocked") {
            return Ok(json!({"observation":"terminal","job":value}));
        }
        if Instant::now() >= deadline {
            return Ok(json!({"observation":"timeout","job":value}));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) fn pause(paths: &Paths) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = configured(paths)?;
    store.set("paused", &true)?;
    status(paths, None)
}

pub(crate) fn resume(paths: &Paths) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = configured(paths)?;
    if let Some(active) = store.active()? {
        ensure!(
            !active.unresolved,
            "recover the unresolved job before resuming"
        );
    }
    store.set("paused", &false)?;
    status(paths, None)
}

pub(crate) fn cancel(paths: &Paths, id: &str) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = configured(paths)?;
    store.transaction(|| {
        let mut job = store.job(id)?;
        store.cancel(id)?;
        if job.phase == Phase::Queued {
            job.cancel_requested = true;
            job.phase = Phase::Cancelled;
            job.outcome = Some(Phase::Cancelled);
            job.outcome_message = Some("Cancelled before execution".into());
            store.save(&mut job)?;
        }
        Ok(())
    })?;
    job_status(&store, id)
}

fn check_identity(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "maintenance owner must have 1 to 256 non-control characters"
    );
    Ok(())
}

pub(crate) fn maintenance_hold(paths: &Paths, owner: &str) -> Result<Value> {
    maintenance(paths, Some(owner), "hold")
}
pub(crate) fn maintenance_status(paths: &Paths) -> Result<Value> {
    maintenance(paths, None, "status")
}
pub(crate) fn maintenance_release(paths: &Paths, owner: &str) -> Result<Value> {
    maintenance(paths, Some(owner), "release")
}

fn maintenance(paths: &Paths, owner: Option<&str>, action: &str) -> Result<Value> {
    if let Some(owner) = owner {
        check_identity(owner)?;
    }
    if store::probe_config(&paths.root)?.is_none() {
        return Ok(json!({"held":false,"drained":true,"owners":[],"active_job":null}));
    }
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = configured(paths)?;
    if let Some(owner) = owner {
        if action == "hold" {
            store.hold(owner)?;
        } else if action == "release" {
            store.release(owner)?;
        }
    }
    let active = store.active()?;
    let owners = store.holds()?;
    Ok(json!({"held":!owners.is_empty(),"drained":active.is_none(),
        "owner_held":owner.map(|owner|owners.iter().any(|held|held == owner)),
        "owners":owners,"active_job":active.map(|job|job.id)}))
}

pub(crate) fn recover(paths: &Paths, id: &str) -> Result<Value> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    let mut job = store.job(id)?;
    ensure!(
        job.phase == Phase::Blocked,
        "only a blocked job can be recovered"
    );
    let phase = job
        .stopped_phase
        .context("blocked job has no operation phase")?;
    ensure!(
        matches!(phase, Phase::Checking | Phase::Preparing | Phase::Deploying),
        "blocked job has no command to reconcile"
    );
    let mut worker = Worker {
        paths,
        store,
        config,
    };
    worker.reconcile_workflow(&job)?;
    job.phase = phase;
    job.outcome = None;
    job.outcome_message = None;
    job.unresolved = false;
    job.last_error = None;
    worker.store.save(&mut job)?;
    worker.tick()?;
    job_status(&worker.store, id)
}

pub(crate) fn worker_once(paths: &Paths) -> Result<Value> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    Worker {
        paths,
        store,
        config,
    }
    .tick()
}

pub(crate) async fn worker(paths: &Paths) -> Result<()> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    let mut worker = Worker {
        paths,
        store,
        config,
    };
    loop {
        worker.tick()?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

struct Worker<'a> {
    paths: &'a Paths,
    store: Store,
    config: Config,
}

impl Worker<'_> {
    fn directory(&self, job: &Job) -> Result<PathBuf> {
        let directory = self.paths.job(&job.id.0);
        store::private_directory(&directory)?;
        Ok(directory)
    }

    fn worktree(&self, job: &Job) -> PathBuf {
        self.paths.job(&job.id.0).join("worktree")
    }

    fn save(&self, job: &mut Job, phase: Phase) -> Result<()> {
        job.phase = phase;
        self.store.save(job)
    }

    fn claim(&self) -> Result<Option<Job>> {
        let _admission = store::lock(&self.paths.root.join("admission.lock"), true)?;
        self.store.transaction(|| {
            if let Some(job) = self.store.active()? {
                return Ok(Some(job));
            }
            if self.store.paused()? || !self.store.holds()?.is_empty() {
                return Ok(None);
            }
            let Some(mut job) = self.store.queued()? else {
                return Ok(None);
            };
            self.save(&mut job, Phase::Starting)?;
            Ok(Some(job))
        })
    }

    fn tick(&mut self) -> Result<Value> {
        self.cleanup_settled()?;
        let Some(mut job) = self.claim()? else {
            return Ok(json!({"state":"idle","paused":self.store.paused()?}));
        };
        if job.phase != Phase::Blocked
            && let Err(error) = self.step(&mut job)
        {
            let message = format!("{error:#}");
            job.last_error = Some(message.clone());
            let unresolved = job.operation.is_some();
            self.finish(&mut job, Phase::Failed, message, unresolved)?;
        }
        self.cleanup_settled()?;
        job_status(&self.store, &job.id.0)
    }

    fn step(&self, job: &mut Job) -> Result<()> {
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if job.cancel_requested && job.operation.is_none() && job.phase != Phase::Blocked {
            return self.finish(
                job,
                Phase::Cancelled,
                "Cancelled before the next command".into(),
                false,
            );
        }
        match job.phase {
            Phase::Starting => {
                git::advance(
                    self.paths,
                    &self.config.common_git_dir,
                    &git::private_ref(&job.id.0)?,
                    None,
                    &job.commit,
                )?;
                git::ensure_worktree(
                    self.paths,
                    &self.config.common_git_dir,
                    &self.worktree(job),
                    &job.commit,
                )?;
                self.save(job, Phase::Checking)
            }
            Phase::Checking => self.workflow_stage(job, "check"),
            Phase::Preparing => self.workflow_stage(job, "prepare"),
            Phase::Deploying => self.workflow_stage(job, "deploy"),
            Phase::Blocked => Ok(()),
            _ => bail!("unexpected active phase: {:?}", job.phase),
        }
    }

    fn finish(
        &self,
        job: &mut Job,
        outcome: Phase,
        message: String,
        unresolved: bool,
    ) -> Result<()> {
        ensure!(outcome.terminal(), "invalid terminal outcome");
        job.stopped_phase = Some(job.phase);
        job.outcome = (!unresolved).then_some(outcome);
        job.outcome_message = Some(message);
        job.unresolved = unresolved;
        self.save(job, if unresolved { Phase::Blocked } else { outcome })
    }

    /// Keep only the newest finished workflow's failure details.
    fn cleanup_settled(&self) -> Result<()> {
        let jobs = self.store.jobs()?;
        let completions = self.store.completion_list()?;
        let previous = self.store.get::<u64>("latest_finished_sequence")?;
        let latest = jobs
            .iter()
            .filter(|job| matches!(job.phase, Phase::Failed | Phase::Succeeded))
            .map(|job| job.sequence)
            .chain(
                completions
                    .iter()
                    .filter(|value| matches!(value["phase"].as_str(), Some("failed" | "succeeded")))
                    .filter_map(|value| value["sequence"].as_u64()),
            )
            .chain(previous)
            .max();
        if latest != previous
            && let Some(sequence) = latest
        {
            self.store.set("latest_finished_sequence", &sequence)?;
        }
        for mut job in jobs {
            if job.phase.terminal()
                && !job.unresolved
                && (job.phase != Phase::Failed || Some(job.sequence) != latest)
            {
                self.cleanup(&mut job)?;
            }
        }
        Ok(())
    }

    fn cleanup(&self, job: &mut Job) -> Result<()> {
        let result = (|| {
            let worktree = self.worktree(job);
            if !job.cleanup.recorded {
                job.cleanup.registration =
                    git::registration(self.paths, &self.config.common_git_dir, &worktree)?;
                job.cleanup.recorded = true;
                self.store.save(job)?;
            }
            crate::broker::forget_job(self.paths, &job.id.0)?;
            git::remove_worktree(
                self.paths,
                &self.config.common_git_dir,
                &worktree,
                job.cleanup.registration.as_deref(),
            )?;
            git::forget(
                self.paths,
                &self.config.common_git_dir,
                &job.id.0,
                &job.commit,
            )?;
            let directory = self.paths.job(&job.id.0);
            if crate::paths::private_directory_exists(&directory)? {
                fs::remove_dir_all(directory)?;
            }
            self.store.retire(job)
        })();
        if let Err(error) = result {
            job.cleanup.state = "failed".into();
            job.cleanup.error = Some(format!("{error:#}"));
            job.cleanup.updated = store::now();
            self.store.save(job)?;
        }
        Ok(())
    }

    fn begin_operation(&self, job: &mut Job, name: String, result: PathBuf) -> Result<bool> {
        if let Some(operation) = &job.operation {
            ensure!(
                operation.name == name
                    && operation.commit == job.commit
                    && operation.phase == job.phase
                    && operation.result == result,
                "retained operation does not match current stage"
            );
            return Ok(false);
        }
        ensure!(
            !result.exists(),
            "unattributed command result already exists"
        );
        job.operation = Some(Operation {
            phase: job.phase,
            name,
            commit: job.commit.clone(),
            result,
            started: store::now(),
        });
        self.store.save(job)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    include!("workflow_tests.rs");
}
