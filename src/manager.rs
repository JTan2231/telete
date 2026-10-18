//! Serial workflow orchestration, with intent retained before effects.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::git;
use crate::invocation;
use crate::model::{CommitId, JobId};
use crate::paths::Paths;
use crate::store::{self, Cleanup, Config, Job, Operation, Phase, Store};
use crate::workflow::{StageRun, Workflow};

#[path = "workflow_manager.rs"]
mod workflow_manager;

#[derive(Clone, Debug)]
pub(crate) struct SubmitOptions {
    pub request_id: Option<String>,
    pub workflow: Workflow,
}

fn check_identity(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "{label} must have 1 to 256 non-control characters"
    );
    Ok(())
}

fn configured(paths: &Paths) -> Result<(Store, Config)> {
    let store = Store::open(&paths.root, false)?;
    let config = store.config()?;
    paths.verify_config(&config)?;
    ensure!(
        git::common(paths, &config.repository)? == config.common_git_dir,
        "configured Git repository identity changed"
    );
    Ok((store, config))
}

pub(crate) fn init(paths: &Paths, repo: &Path, accepted_baseline: Option<&str>) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let store = Store::open(&paths.root, true)?;
    let repository = git::repository(paths, repo)?;
    let baseline = accepted_baseline
        .map(|value| git::commit(paths, &repository, value))
        .transpose()?;
    let config = Config {
        state_root: paths.root.clone(),
        common_git_dir: git::common(paths, &repository)?,
        repository,
        accepted_baseline: baseline.clone(),
    };
    if let Some(previous) = store.get::<Config>("config")? {
        ensure!(
            previous == config,
            "initialization cannot replace Telete's repository, baseline, or state directory"
        );
        if let Some(baseline) = &baseline {
            ensure!(
                git::ref_value(paths, &config.repository, git::ACCEPTED)?.as_ref()
                    == Some(baseline),
                "accepted history already advanced; initialization cannot rewind it"
            );
        }
        return Ok(
            json!({"state":"initialized","paused":store.paused()?,"accepted":baseline,"config":config}),
        );
    }
    if let Some(baseline) = &baseline {
        git::advance(paths, &config.repository, git::ACCEPTED, None, baseline)?;
    }
    store.transaction(|| {
        store.set("config", &config)?;
        store.set("paused", &true)
    })?;
    Ok(json!({"state":"initialized","paused":true,"accepted":baseline,"config":config}))
}

pub(crate) fn submit(
    paths: &Paths,
    repo: &Path,
    revision: &str,
    options: SubmitOptions,
) -> Result<Value> {
    let (store, config) = configured(paths)?;
    ensure!(
        git::common(paths, repo)? == config.common_git_dir,
        "submit from a worktree of Telete's configured repository"
    );
    let input = git::commit(paths, repo, revision)?;
    options.workflow.validate()?;
    ensure!(
        !options.workflow.integrate || config.accepted_baseline.is_some(),
        "integration requires an initialized accepted baseline"
    );
    let request_id = options
        .request_id
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    check_identity(&request_id, "submission request identity")?;
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    if let Some(previous) = store.by_request(&request_id)? {
        ensure!(
            previous.input == input && previous.workflow == options.workflow,
            "submission request identity belongs to different inputs"
        );
        return previous.output();
    }
    paths.require_capacity()?;
    let environment = invocation::capture_environment(paths)?;
    let id = JobId::new(uuid::Uuid::now_v7().to_string())?;
    let job = new_job(id, request_id, input, options.workflow, environment);
    git::advance(
        paths,
        repo,
        &git::private_ref(&job.id.0, "input")?,
        None,
        &job.input,
    )?;
    let admitted = store.transaction(|| store.insert(&job))?;
    admitted.output()
}

fn new_job(
    id: JobId,
    request_id: String,
    input: CommitId,
    workflow: Workflow,
    environment: BTreeMap<String, String>,
) -> Job {
    Job {
        id,
        sequence: 0,
        revision: 0,
        request_id,
        phase: Phase::Queued,
        cancel_requested: false,
        created: store::now(),
        updated: store::now(),
        input,
        workflow,
        environment,
        base: None,
        candidate: None,
        accepted: false,
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

pub(crate) fn status(paths: &Paths, id: Option<&str>) -> Result<Value> {
    let (store, config) = configured(paths)?;
    if let Some(id) = id {
        return store.job(id)?.output();
    }
    let active = store.active()?.map(|job| job.output()).transpose()?;
    let queue = store
        .jobs()?
        .into_iter()
        .filter(|job| job.phase == Phase::Queued)
        .map(|job| job.output())
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"schema_version":2,"version":env!("CARGO_PKG_VERSION"),"paused":store.paused()?,
        "config":config,"accepted":git::ref_value(paths,&config.repository,git::ACCEPTED)?,"active":active,"queue":queue,"maintenance_owners":store.holds()?}),
    )
}

pub(crate) async fn wait(paths: &Paths, id: &str, timeout_seconds: u64) -> Result<Value> {
    ensure!(timeout_seconds > 0, "wait timeout must be positive");
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let (store, _) = configured(paths)?;
        let job = store.job(id)?;
        if job.phase.terminal() || job.phase == Phase::Blocked {
            return Ok(json!({"observation":"terminal","job":job.output()?}));
        }
        if Instant::now() >= deadline {
            return Ok(json!({"observation":"timeout","job":job.output()?}));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
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
            active.phase != Phase::Blocked && !active.unresolved,
            "recover the unresolved Telete job before resuming"
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
            job.outcome_message = Some("Cancelled before queue admission".into());
            store.save(&mut job)?;
        }
        Ok(())
    })?;
    store.job(id)?.output()
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
        check_identity(owner, "maintenance owner")?;
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
    let drained = active.is_none() && store.jobs()?.iter().all(|job| !job.unresolved);
    Ok(json!({"held":!owners.is_empty(),"drained":drained,
        "owner_held":owner.map(|owner| owners.iter().any(|held| held == owner)),"owners":owners,"active_job":active.map(|job|job.id)}))
}

/// Recover only a command with correlated terminal process evidence. Recovery
/// consumes the saved result; it never admits the same command again.
pub(crate) fn recover(paths: &Paths, id: &str) -> Result<Value> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    let mut job = store.job(id)?;
    ensure!(
        job.phase == Phase::Blocked,
        "only a blocked Telete job can be recovered"
    );
    let phase = job
        .stopped_phase
        .context("blocked job has no retained operation phase")?;
    ensure!(
        matches!(phase, Phase::Checking | Phase::Preparing | Phase::Deploying),
        "recovery has no authoritative continuation for this phase"
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
    worker.store.job(id)?.output()
}

pub(crate) fn worker_once(paths: &Paths) -> Result<Value> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    let mut worker = Worker {
        paths,
        store,
        config,
    };
    worker.tick()
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
        tokio::time::sleep(Duration::from_secs(2)).await;
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
    fn candidate(job: &Job) -> Result<&CommitId> {
        job.candidate
            .as_ref()
            .context("active job has no candidate")
    }
    fn base(job: &Job) -> Result<&CommitId> {
        job.base.as_ref().context("active job has no fixed base")
    }

    fn save(&self, job: &mut Job, phase: Phase) -> Result<()> {
        job.phase = phase;
        self.store.save(job)
    }

    fn claim(&self) -> Result<Option<Job>> {
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
            self.paths.require_capacity()?;
            if job.workflow.integrate {
                job.base = Some(git::commit(
                    self.paths,
                    &self.config.repository,
                    git::ACCEPTED,
                )?);
            }
            self.save(&mut job, Phase::Integrating)?;
            Ok(Some(job))
        })
    }

    fn tick(&mut self) -> Result<Value> {
        self.paths.require_capacity()?;
        for mut job in self.store.jobs()? {
            if (job.phase.terminal() || job.outcome.is_some()) && !job.unresolved {
                self.cleanup(&mut job)?;
            }
        }
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
        self.store.job(&job.id.0)?.output()
    }

    fn step(&mut self, job: &mut Job) -> Result<()> {
        job.cancel_requested = self.store.job(&job.id.0)?.cancel_requested;
        if job.cancel_requested
            && job.operation.is_none()
            && !matches!(job.phase, Phase::Deploying | Phase::Blocked)
        {
            // Git promotion may have completed before the worker saved its
            // acceptance fact. Cancellation must retain that existing effect.
            if job.phase == Phase::Accepting
                && git::ref_value(self.paths, &self.config.repository, git::ACCEPTED)?.as_ref()
                    == Some(Self::candidate(job)?)
            {
                job.accepted = true;
            }
            return self.finish(
                job,
                Phase::Cancelled,
                "Cancelled before the next operation".into(),
                false,
            );
        }
        match job.phase {
            Phase::Integrating => self.integrating(job),
            Phase::Checking => self.workflow_stage(job, "check"),
            Phase::Preparing => self.workflow_stage(job, "prepare"),
            Phase::Accepting => self.workflow_accept(job),
            Phase::Deploying => self.workflow_stage(job, "deploy"),
            Phase::Blocked => Ok(()),
            _ => bail!("unexpected active Telete phase: {:?}", job.phase),
        }
    }

    fn finish(
        &self,
        job: &mut Job,
        outcome: Phase,
        message: String,
        unresolved: bool,
    ) -> Result<()> {
        ensure!(outcome.terminal(), "invalid terminal workflow outcome");
        job.stopped_phase = Some(job.phase);
        job.outcome = Some(outcome);
        job.outcome_message = Some(message);
        job.unresolved = unresolved;
        if outcome != Phase::Succeeded {
            self.store.set("paused", &true)?;
        }
        let phase = if unresolved { Phase::Blocked } else { outcome };
        self.save(job, phase)?;
        self.cleanup(job)
    }

    fn cleanup(&self, job: &mut Job) -> Result<()> {
        if job.outcome.is_none() || job.unresolved || job.cleanup.state == "removed" {
            return Ok(());
        }
        let worktree = self.worktree(job);
        if !job.cleanup.recorded {
            match git::registration(self.paths, &self.config.repository, &worktree) {
                Ok(registration) => {
                    job.cleanup.registration = registration;
                    job.cleanup.recorded = true;
                }
                Err(error) => {
                    job.cleanup.state = "failed".into();
                    job.cleanup.error = Some(format!("{error:#}"));
                    job.cleanup.updated = store::now();
                    return self.store.save(job);
                }
            }
            self.store.save(job)?;
        }
        match git::remove_worktree(
            self.paths,
            &self.config.repository,
            &worktree,
            job.cleanup.registration.as_deref(),
        ) {
            Ok(()) => {
                job.cleanup.state = "removed".into();
                job.cleanup.error = None;
            }
            Err(error) => {
                job.cleanup.state = "failed".into();
                job.cleanup.error = Some(format!("{error:#}"));
            }
        }
        job.cleanup.updated = store::now();
        self.store.save(job)
    }

    fn integrating(&self, job: &mut Job) -> Result<()> {
        if !job.workflow.integrate {
            let candidate = job.input.clone();
            job.candidate = Some(candidate.clone());
            self.store.save(job)?;
            git::advance(
                self.paths,
                &self.config.repository,
                &git::private_ref(&job.id.0, "candidate")?,
                None,
                &candidate,
            )?;
            git::ensure_worktree(
                self.paths,
                &self.config.repository,
                &self.worktree(job),
                &candidate,
            )?;
            return self.save(job, Phase::Checking);
        }
        let base = Self::base(job)?.clone();
        let worktree = self.worktree(job);
        git::ensure_worktree(self.paths, &self.config.repository, &worktree, &base)?;
        let merge = git::merge(self.paths, &worktree, &job.input)?;
        store::atomic_bytes(
            &self.directory(job)?.join("merge.log"),
            format!("{}{}", merge.stdout, merge.stderr).as_bytes(),
        )?;
        if !merge.success() {
            return self.finish(
                job,
                Phase::Failed,
                "Integration failed. Merge conflicts require source changes".into(),
                false,
            );
        }
        let tree = git::value(self.paths, &worktree, &["write-tree"])?;
        let parents = if base == job.input {
            vec![base]
        } else {
            vec![base, job.input.clone()]
        };
        let candidate = git::commit_tree(
            self.paths,
            &self.config.repository,
            &tree,
            &parents,
            &job.id.0,
            job.created,
            "Integrate Telete submission",
        )?;
        job.candidate = Some(candidate.clone());
        self.store.save(job)?;
        git::advance(
            self.paths,
            &self.config.repository,
            &git::private_ref(&job.id.0, "candidate")?,
            None,
            &candidate,
        )?;
        git::ensure_worktree(self.paths, &self.config.repository, &worktree, &candidate)?;
        self.save(job, Phase::Checking)
    }

    fn begin_operation(&self, job: &mut Job, name: String, result: PathBuf) -> Result<bool> {
        let candidate = Self::candidate(job)?.clone();
        if let Some(operation) = &job.operation {
            ensure!(
                operation.name == name
                    && operation.candidate == candidate
                    && operation.phase == job.phase
                    && operation.result == result,
                "retained operation does not match current stage"
            );
            ensure!(
                result.is_file(),
                "interrupted operation has no terminal receipt; recovery is required"
            );
            return Ok(false);
        }
        ensure!(
            !result.exists(),
            "unattributed operation receipt already exists"
        );
        job.operation = Some(Operation {
            phase: job.phase,
            name,
            candidate,
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
    use std::os::unix::fs::PermissionsExt;

    fn job(paths: &Paths, base: &CommitId, id: &str) -> Result<Job> {
        Ok(new_job(
            JobId::new(id)?,
            format!("request-{id}"),
            base.clone(),
            workflow_selection(paths),
            invocation::capture_environment(paths)?,
        ))
    }

    fn worker(paths: &Paths) -> Result<Worker<'_>> {
        let (store, config) = configured(paths)?;
        Ok(Worker {
            paths,
            store,
            config,
        })
    }

    #[test]
    fn store_preserves_cancellation_and_rejects_stale_history() -> Result<()> {
        let (_temporary, paths, _repo, base) = workflow_fixture(true)?;
        let store = Store::open(&paths.root, false)?;
        let queued = job(&paths, &base, "cas-fixture")?;
        let mut first = store.insert(&queued)?;
        let mut stale = first.clone();
        store.cancel(&first.id.0)?;
        first.last_error = Some("retained".into());
        store.save(&mut first)?;
        assert!(first.cancel_requested);
        stale.last_error = Some("must not overwrite".into());
        assert!(store.save(&mut stale).is_err());
        assert_eq!(
            store.job(&first.id.0)?.last_error.as_deref(),
            Some("retained")
        );
        assert!(store.insert(&queued).is_err());
        assert_eq!(
            store
                .by_request(&queued.request_id)?
                .context("saved request")?
                .id,
            queued.id
        );
        Ok(())
    }

    #[test]
    fn fresh_integration_creates_private_job_directory_before_git() -> Result<()> {
        let (_temporary, paths, repo, base) = workflow_fixture(true)?;
        fs::write(repo.join("payload.txt"), "submitted\n")?;
        git::value(&paths, &repo, &["add", "payload.txt"])?;
        let tree = git::value(&paths, &repo, &["write-tree"])?;
        let input = git::commit_tree(
            &paths,
            &repo,
            &tree,
            std::slice::from_ref(&base),
            "fixture",
            1_700_000_001,
            "Fixture submission",
        )?;
        git::value(&paths, &repo, &["reset", "--hard", &input.0])?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "fresh-integration")?;
        job.input = input.clone();
        job.workflow.integrate = true;
        job.base = Some(base.clone());
        job.phase = Phase::Integrating;
        let mut job = worker.store.insert(&job)?;
        let directory = paths.job(&job.id.0);
        assert!(!directory.exists());

        worker.integrating(&mut job)?;

        assert_eq!(job.phase, Phase::Checking);
        assert_eq!(
            fs::metadata(&directory)?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("merge.log"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let candidate = job.candidate.as_ref().context("integrated candidate")?;
        assert_eq!(
            git::commit(&paths, &worker.worktree(&job), "HEAD")?,
            *candidate
        );
        assert_eq!(
            git::ref_value(&paths, &repo, &git::private_ref(&job.id.0, "candidate")?)?,
            Some(candidate.clone())
        );
        assert_eq!(
            fs::read_to_string(worker.worktree(&job).join("payload.txt"))?,
            "submitted\n"
        );
        assert_eq!(git::ref_value(&paths, &repo, git::ACCEPTED)?, Some(base));
        assert_eq!(git::commit(&paths, &repo, "HEAD")?, input);
        Ok(())
    }

    #[test]
    fn cleanup_removes_only_settled_private_worktree_and_registration() -> Result<()> {
        let (_temporary, paths, repo, base) = workflow_fixture(true)?;
        let worker = worker(&paths)?;
        let mut job = job(&paths, &base, "cleanup-fixture")?;
        job.base = Some(base.clone());
        job.candidate = Some(base.clone());
        job.phase = Phase::Checking;
        let mut job = worker.store.insert(&job)?;
        store::private_directory(&paths.job(&job.id.0))?;
        git::ensure_worktree(&paths, &repo, &worker.worktree(&job), &base)?;
        fs::write(worker.worktree(&job).join("private-untracked"), "private")?;
        let registration = git::registration(&paths, &repo, &worker.worktree(&job))?
            .context("linked registration")?;
        worker.finish(&mut job, Phase::Failed, "fixture failure".into(), true)?;
        assert!(worker.worktree(&job).exists());
        assert!(registration.exists());
        job.unresolved = false;
        job.phase = Phase::Cancelled;
        job.outcome = Some(Phase::Cancelled);
        worker.store.save(&mut job)?;
        worker.cleanup(&mut job)?;
        assert_eq!(job.cleanup.state, "removed");
        assert!(!worker.worktree(&job).exists());
        assert!(!registration.exists());
        assert_eq!(git::commit(&paths, &repo, "HEAD")?, base);
        Ok(())
    }

    #[test]
    fn maintenance_drain_keeps_active_operations_visible() -> Result<()> {
        let (_temporary, paths, _repo, base) = workflow_fixture(true)?;
        let store = Store::open(&paths.root, false)?;
        let mut active = job(&paths, &base, "drain-fixture")?;
        active.phase = Phase::Preparing;
        active.base = Some(base.clone());
        active.candidate = Some(base);
        store.insert(&active)?;
        drop(store);
        let held = maintenance_hold(&paths, "fixture-owner")?;
        assert_eq!(held["held"], true);
        assert_eq!(held["drained"], false);
        assert_eq!(held["active_job"], "drain-fixture");
        Ok(())
    }

    include!("workflow_tests.rs");
}
