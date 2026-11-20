//! Serial execution of configured committed-source workflows.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::git;
use crate::model::{CommitId, JobId};
use crate::paths::Paths;
use crate::shared_queue::{Candidate, Queue, Slot};
use crate::store::{self, Cleanup, Config, Job, Operation, Phase, Store};
use crate::workflow::{ProjectConfig, QueueMode, StageRun, Workflow};

#[path = "batch_manager.rs"]
mod batch_manager;
#[path = "integration_manager.rs"]
mod integration_manager;
pub(crate) use integration_manager::request_merge as merge;
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
    mode: QueueMode,
) -> Result<Value> {
    let shared = shared_queue(mode, true)?;
    submit_in(paths, repo, revision, workflow, mode, shared.as_ref())
}

fn submit_in(
    paths: &Paths,
    repo: &Path,
    revision: &str,
    mut workflow: Workflow,
    mode: QueueMode,
    shared: Option<&Queue>,
) -> Result<Value> {
    workflow.validate()?;
    let (store, config) = configured(paths)?;
    ensure!(
        git::common(paths, repo)? == config.common_git_dir,
        "submit must select a checkout of the configured Git repository"
    );
    let commit = git::commit(paths, repo, revision)?;
    if let Some(integration) = workflow.integration() {
        git::branch_tip(paths, repo, &integration.branch)?;
    }
    if let Some(batch) = &mut workflow.batch {
        batch.base = git::commit(paths, repo, &batch.base)?.0;
    }
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    ensure!(
        !store.has_unfinished_jobs()? || queue_mode(&store)? == mode,
        "repository queue mode cannot change while it has unfinished work"
    );
    let mut job = new_job(
        JobId::new(uuid::Uuid::now_v7().to_string())?,
        commit,
        workflow,
    );
    if job.workflow.integration().is_some() {
        job.development_worktree = git::development_worktree(paths, repo, &job.commit)?;
    }
    let insert = |job: &Job| {
        store.transaction(|| {
            store.set("queue_mode", &mode)?;
            store.insert(job)
        })
    };
    let mut admitted = if mode == QueueMode::Global {
        shared
            .context("shared queue state is missing")?
            .enqueue(paths, |order| {
                job.shared_order = Some(order);
                insert(&job)
            })?
    } else {
        insert(&job)?
    };
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
        if let Some(batch) = &job.workflow.batch {
            git::advance(
                paths,
                &config.common_git_dir,
                &git::private_ref(&format!("base-{}", job.id))?,
                None,
                &CommitId::new(batch.base.clone())?,
            )?;
        }
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
        shared_order: None,
        revision: 0,
        phase: Phase::Queued,
        cancel_requested: false,
        created: store::now(),
        updated: store::now(),
        commit,
        resulting_commit: None,
        analyzed_commit: None,
        repaired_commit: None,
        release_id: None,
        batch: None,
        release_commit: None,
        integration_base: None,
        merged_commit: None,
        merge_requested: false,
        merge_cleanup: Vec::new(),
        development_worktree: None,
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

fn queue_mode(store: &Store) -> Result<QueueMode> {
    // Jobs admitted by older versions have no shared order and drain in
    // repository mode before this repository can use the shared queue.
    Ok(store.get("queue_mode")?.unwrap_or(QueueMode::Repository))
}

fn shared_queue(mode: QueueMode, create: bool) -> Result<Option<Queue>> {
    if mode == QueueMode::Global {
        return Queue::open(create);
    }
    Ok(None)
}

pub(crate) fn project_status(
    paths: &Paths,
    id: Option<&str>,
    selected: QueueMode,
) -> Result<Value> {
    let mut value = status(paths, id)?;
    if id.is_some() {
        return Ok(value);
    }
    let store = store::probe_config(&paths.root)?
        .map(|_| Store::open(&paths.root))
        .transpose()?;
    let mode = if let Some(store) = &store {
        store
            .get("queue_mode")?
            .unwrap_or(if store.has_unfinished_jobs()? {
                QueueMode::Repository
            } else {
                selected
            })
    } else {
        selected
    };
    value["queue_mode"] = json!(mode);
    if mode == QueueMode::Global {
        let holder = match shared_queue(mode, false)? {
            Some(queue) => queue.holder()?,
            None => None,
        };
        let waiting = if let Some(store) = &store {
            holder
                .as_ref()
                .is_some_and(|holder| holder.root != paths.root)
                && next_job(store, mode)?.is_some()
        } else {
            false
        };
        value["shared_slot"] =
            json!(holder.map(|holder| json!({"state_root":holder.root,"job":holder.job})));
        value["waiting_for_shared_slot"] = json!(waiting);
    }
    Ok(value)
}

fn job_status(store: &Store, id: &str) -> Result<Value> {
    if let Some(completion) = store.completion(id)? {
        return Ok(completion);
    }
    let job = store.job(id)?;
    let mut output = job.output()?;
    if job.phase == Phase::WaitingRelease
        && let Some(release) = &job.release_id
        && *release != job.id
    {
        let leader = store.job(&release.0)?;
        output["release_phase"] = json!(leader.phase);
        if let Some(commit) = leader.merged_commit {
            output["merged_commit"] = json!(commit);
        }
        if leader.phase == Phase::WaitingMerge {
            output["merge_requested"] = json!(leader.merge_requested);
        }
    }
    Ok(output)
}

pub(crate) fn status(paths: &Paths, id: Option<&str>) -> Result<Value> {
    if store::probe_config(&paths.root)?.is_none() {
        ensure!(id.is_none(), "no retained result for this job");
        return Ok(json!({"schema_version":3,"paused":false,"active":null,
            "queue":[],"awaiting_repair":[],"waiting_release":[],"waiting_merge":[],"failure":null,"completions":[],"maintenance_owners":[]}));
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
    let awaiting_repair = jobs
        .iter()
        .filter(|job| job.phase == Phase::AwaitingRepair)
        .map(Job::output)
        .collect::<Result<Vec<_>>>()?;
    let failure = jobs
        .iter()
        .rev()
        .find(|job| job.phase == Phase::Failed)
        .map(Job::output)
        .transpose()?;
    let waiting_release = jobs
        .iter()
        .filter(|job| job.phase == Phase::WaitingRelease)
        .map(Job::output)
        .collect::<Result<Vec<_>>>()?;
    let waiting_merge = jobs
        .iter()
        .filter(|job| job.phase == Phase::WaitingMerge)
        .map(Job::output)
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"schema_version":3,"version":env!("CARGO_PKG_VERSION"),
        "paused":store.paused()?,"config":config,
        "active":store.active()?.map(|job|job.output()).transpose()?,
        "queue":queue,"awaiting_repair":awaiting_repair,"waiting_release":waiting_release,
        "waiting_merge":waiting_merge,
        "failure":failure,"completions":store.completion_list()?,
        "maintenance_owners":store.holds()?}),
    )
}

pub(crate) async fn wait(paths: &Paths, id: &str, timeout_seconds: Option<u64>) -> Result<Value> {
    ensure!(timeout_seconds != Some(0), "wait timeout must be positive");
    let started = Instant::now();
    loop {
        let value = status(paths, Some(id))?;
        let phase = value["phase"].as_str().context("job has no phase")?;
        if matches!(
            phase,
            "succeeded" | "failed" | "cancelled" | "blocked" | "awaiting_repair"
        ) || ((phase == "waiting_merge" || value["release_phase"] == "waiting_merge")
            && value["merge_requested"] != true)
        {
            return Ok(json!({"observation":"terminal","job":value}));
        }
        if timeout_seconds.is_some_and(|seconds| started.elapsed() >= Duration::from_secs(seconds))
        {
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
        if job.phase.terminal() {
            return Ok(());
        }
        if let Some(release) = &job.release_id {
            let leader = store.job(&release.0)?;
            if let Some(batch) = &leader.batch {
                for member in &batch.members {
                    let member = store.job(&member.0)?;
                    ensure!(
                        member.phase != Phase::AwaitingRepair
                            || member.repaired_commit.is_none()
                            || member.resulting_commit == member.repaired_commit,
                        "complete the interrupted repair before cancelling this release"
                    );
                }
                for member in &batch.members {
                    store.cancel(&member.0)?;
                }
            }
        }
        ensure!(
            job.phase != Phase::AwaitingRepair
                || job.repaired_commit.is_none()
                || job.resulting_commit == job.repaired_commit,
            "complete the interrupted repair before cancelling this job"
        );
        store.cancel(id)?;
        if matches!(
            job.phase,
            Phase::Queued | Phase::AwaitingRepair | Phase::WaitingRelease | Phase::WaitingMerge
        ) {
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

/// An explicit repair accepts a new source identity without repeating analysis.
pub(crate) fn repair(paths: &Paths, repo: &Path, id: &str, revision: &str) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, config) = configured(paths)?;
    ensure!(
        git::common(paths, repo)? == config.common_git_dir,
        "repair must select a checkout of the configured Git repository"
    );
    let mut job = store.job(id)?;
    ensure!(
        job.phase == Phase::AwaitingRepair && job.operation.is_none() && !job.unresolved,
        "only a job awaiting agent repair can accept repairs"
    );
    ensure!(!job.cancel_requested, "cancelled job cannot accept repairs");
    let commit = git::commit(paths, repo, revision)?;
    validate_repair(paths, &config, &job, &commit)?;
    accept_repair(paths, &store, &config, &mut job, commit, None)?;
    job.output()
}

fn validate_repair(paths: &Paths, config: &Config, job: &Job, commit: &CommitId) -> Result<()> {
    let id = &job.id.0;
    let previous = job.effective_commit();
    ensure!(
        git::is_ancestor(paths, &config.common_git_dir, previous, commit)?,
        "repair commit must descend from the current job source"
    );
    ensure!(
        commit != previous,
        "repair requires a new committed source revision"
    );
    if let Some(selected) = &job.repaired_commit
        && job.resulting_commit.as_ref() != Some(selected)
    {
        ensure!(
            selected == commit,
            "an interrupted repair already selected another commit"
        );
    }
    let checkout = paths.job(id).join("worktree");
    ensure!(
        git::common(paths, &checkout)? == config.common_git_dir,
        "repair checkout belongs to another repository"
    );
    git::ensure_detached(paths, &checkout)?;
    let head = git::commit(paths, &checkout, "HEAD")?;
    ensure!(
        &head == previous || &head == commit,
        "repair checkout HEAD changed unexpectedly"
    );
    let pending = job.repaired_commit.as_ref() == Some(commit)
        && job.resulting_commit.as_ref() != Some(commit);
    let retained = git::ref_value(paths, &config.common_git_dir, &git::result_ref(id)?)?;
    if !pending || retained.as_ref() != Some(commit) {
        git::unchanged(paths, &checkout, &head)?;
    }
    Ok(())
}

fn accept_repair(
    paths: &Paths,
    store: &Store,
    config: &Config,
    job: &mut Job,
    commit: CommitId,
    run: Option<StageRun>,
) -> Result<()> {
    let previous = job.effective_commit().clone();
    let checkout = paths.job(&job.id.0).join("worktree");
    // Persist the accepted identity before changing its ref or checkout. A
    // repeated repair with the same revision completes an interrupted handoff.
    job.repaired_commit = Some(commit.clone());
    store.save(job)?;
    git::advance(
        paths,
        &config.common_git_dir,
        &git::result_ref(&job.id.0)?,
        Some(&previous),
        &commit,
    )?;
    git::reset_private(paths, &checkout, &commit)?;
    let mut queued = job.clone();
    queued.resulting_commit = Some(commit);
    queued.runs.extend(run);
    queued.operation = None;
    queued.phase = Phase::Queued;
    queued.stopped_phase = None;
    queued.outcome = None;
    queued.outcome_message = None;
    queued.last_error = None;
    if job.phase == Phase::AwaitingRepair && queue_mode(store)? == QueueMode::Global {
        let shared = Queue::open(false)?.context("shared queue state is missing")?;
        shared.enqueue(paths, |order| {
            queued.shared_order = Some(order);
            store.save(&mut queued)
        })?;
    } else {
        store.save(&mut queued)?;
    }
    *job = queued;
    Ok(())
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
    let _worker = store::lock(&paths.root.join("worker.lock"), true)?;
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
        matches!(
            phase,
            Phase::Fixing
                | Phase::Analyzing
                | Phase::Checking
                | Phase::Repairing
                | Phase::Composing
                | Phase::RepairingMerge
                | Phase::Preparing
                | Phase::Deploying
        ),
        "blocked job has no command to reconcile"
    );
    let mut worker = Worker::new(paths, store, config);
    worker.reconcile_workflow(&job)?;
    job.phase = phase;
    job.outcome = None;
    job.outcome_message = None;
    job.unresolved = false;
    job.last_error = None;
    worker.store.save(&mut job)?;
    if queue_mode(&worker.store)? == QueueMode::Repository {
        worker.tick()?;
    }
    job_status(&worker.store, id)
}

#[cfg(test)]
pub(crate) fn worker_once(paths: &Paths) -> Result<Value> {
    let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
    let (store, config) = configured(paths)?;
    Worker::new(paths, store, config).tick()
}

pub(crate) async fn worker(paths: &Paths) -> Result<()> {
    let owner = store::lock(&paths.root.join("runner.lock"), true)?;
    owner.close_on_exec()?;
    let (store, config) = configured(paths)?;
    let mut worker = Worker::new(paths, store, config);
    loop {
        {
            // Keep this lock inheritable: after a worker crash, an admitted
            // command must settle before replacement or recovery takes over.
            let _execution = store::lock(&paths.root.join("worker.lock"), true)?;
            worker.tick()?;
            if !needs_worker_in(&worker.store)? {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Retained failure details alone do not keep execution alive. Other settled
/// records still need cleanup before the runtime may stop.
pub(crate) fn needs_worker(paths: &Paths) -> Result<bool> {
    let (store, _) = configured(paths)?;
    needs_worker_in(&store)
}

fn needs_worker_in(store: &Store) -> Result<bool> {
    let latest = store.get::<u64>("latest_finished_sequence")?;
    let jobs = store.jobs()?;
    Ok(jobs.iter().any(|job| {
        let retained_failure = job.phase == Phase::Failed
            && (Some(job.sequence) == latest
                || job.release_id.as_ref().is_some_and(|id| {
                    jobs.iter().any(|leader| {
                        leader.id == *id
                            && leader.phase == Phase::Failed
                            && Some(leader.sequence) == latest
                    })
                }));
        job.phase != Phase::AwaitingRepair && !retained_failure
    }))
}

fn next_job(store: &Store, mode: QueueMode) -> Result<Option<Job>> {
    let jobs = store.jobs()?;
    if let Some(job) = Store::active_job(&jobs)? {
        return Ok(
            (mode == QueueMode::Repository || job.phase != Phase::Blocked).then(|| job.clone()),
        );
    }
    if store.paused()? || !store.holds()?.is_empty() {
        return Ok(None);
    }
    let releases = Worker::ready_releases(&jobs)?;
    let merges = jobs
        .iter()
        .filter(|job| job.phase == Phase::WaitingMerge && job.merge_requested)
        .cloned()
        .collect::<Vec<_>>();
    let mut queued = jobs.into_iter().filter(|job| job.phase == Phase::Queued);
    if mode == QueueMode::Repository {
        return Ok(merges
            .into_iter()
            .chain(releases)
            .next()
            .or_else(|| queued.next()));
    }
    Ok(releases
        .into_iter()
        .chain(merges)
        .chain(queued)
        .min_by_key(|job| job.shared_order))
}

fn ready_candidate(job: &Job) -> Result<Candidate> {
    Ok(Candidate::Ready {
        job: job.id.clone(),
        order: job.shared_order.context("global job has no shared order")?,
    })
}

fn shared_candidate(paths: &Paths) -> Result<Candidate> {
    if !crate::paths::private_directory_exists(&paths.root)? {
        return Ok(Candidate::Idle);
    }
    let store = Store::open_for_scheduler(&paths.root)?;
    if queue_mode(&store)? != QueueMode::Global || !store.has_unfinished_jobs()? {
        return Ok(Candidate::Idle);
    }
    // A worker can outlive its supervisor. A repository with neither owner
    // remains registered, but cannot take the next turn until restarted.
    if let Some(_runner) = crate::paths::try_lock(&paths.root.join("runner.lock"))?
        && let Some(_supervisor) = crate::paths::try_lock(&paths.root.join("supervisor.lock"))?
    {
        return Ok(Candidate::Pending);
    }
    next_job(&store, QueueMode::Global)?
        .as_ref()
        .map_or(Ok(Candidate::Pending), ready_candidate)
}

struct Worker<'a> {
    paths: &'a Paths,
    store: Store,
    config: Config,
    shared: Option<Queue>,
    slot: Option<Slot>,
}

impl Worker<'_> {
    fn new(paths: &Paths, store: Store, config: Config) -> Worker<'_> {
        Worker {
            paths,
            store,
            config,
            shared: None,
            slot: None,
        }
    }

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

    fn claim(&mut self) -> Result<Option<Job>> {
        let _admission = store::lock(&self.paths.root.join("admission.lock"), true)?;
        let mode = queue_mode(&self.store)?;
        let next = next_job(&self.store, mode)?;
        if self.slot.as_ref().is_some_and(|slot| {
            mode != QueueMode::Global || next.as_ref().is_none_or(|job| slot.job() != &job.id)
        }) {
            self.release_slot()?;
        }
        let Some(mut job) = next else {
            return Ok(None);
        };
        if mode == QueueMode::Global && self.slot.is_none() {
            if self.shared.is_none() {
                self.shared = Queue::open(false)?;
            }
            let shared = self
                .shared
                .as_ref()
                .context("shared queue state is missing")?;
            self.slot = shared.try_claim(self.paths, &job.id, |paths| {
                if paths.root == self.paths.root {
                    return ready_candidate(&job);
                }
                // This repository will report its own state errors. It cannot
                // take a turn while its readiness cannot be read.
                Ok(shared_candidate(paths).unwrap_or(Candidate::Pending))
            })?;
            if self.slot.is_none() {
                return Ok(None);
            }
        }
        if job.phase == Phase::Queued {
            self.save(&mut job, Phase::Starting)?;
        } else if job.phase == Phase::WaitingMerge {
            self.save(&mut job, Phase::Merging)?;
        } else if job.phase == Phase::WaitingRelease
            && let Err(error) = self.claim_release(&mut job)
        {
            let message = format!("Could not start shared release: {error:#}");
            job.last_error = Some(message.clone());
            self.finish(&mut job, Phase::Failed, message, false)?;
        }
        Ok(Some(job))
    }

    fn release_slot(&mut self) -> Result<()> {
        if let Some(slot) = self.slot.take() {
            slot.release()?;
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<Value> {
        self.settle_releases()?;
        self.cleanup_settled()?;
        let Some(mut job) = self.claim()? else {
            self.forget_idle()?;
            return Ok(json!({"state":"idle","paused":self.store.paused()?}));
        };
        if !job.phase.terminal()
            && let Err(error) = self.step(&mut job)
        {
            let message = format!("{error:#}");
            job.last_error = Some(message.clone());
            let unresolved = job.operation.is_some();
            self.finish(&mut job, Phase::Failed, message, unresolved)?;
        }
        if job.phase.terminal()
            || matches!(
                job.phase,
                Phase::Blocked
                    | Phase::AwaitingRepair
                    | Phase::WaitingRelease
                    | Phase::WaitingMerge
            )
        {
            self.release_slot()?;
        }
        self.settle_releases()?;
        self.cleanup_settled()?;
        if job.phase.terminal() {
            self.forget_idle()?;
        }
        job_status(&self.store, &job.id.0)
    }

    fn forget_idle(&self) -> Result<()> {
        if let Some(shared) = &self.shared
            && !self.store.has_unfinished_jobs()?
        {
            shared.forget_idle(self.paths, || Ok(!self.store.has_unfinished_jobs()?))?;
        }
        Ok(())
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
                    job.effective_commit(),
                )?;
                self.save(
                    job,
                    if job.workflow.fix.is_some() && !job.runs.iter().any(|run| run.stage == "fix")
                    {
                        Phase::Fixing
                    } else if job.workflow.analyze.is_some()
                        && !job.runs.iter().any(|run| run.stage == "analyze")
                    {
                        Phase::Analyzing
                    } else {
                        Phase::Checking
                    },
                )
            }
            Phase::Fixing => self.workflow_stage(job, "fix"),
            Phase::Analyzing => self.workflow_stage(job, "analyze"),
            Phase::Checking => self.workflow_stage(job, "check"),
            Phase::Repairing => self.workflow_stage(job, "repair"),
            Phase::Composing => self.workflow_stage(job, "compose"),
            Phase::RepairingMerge => self.workflow_stage(job, "merge_repair"),
            Phase::Preparing => self.workflow_stage(job, "prepare"),
            Phase::Merging => self.integrate(job),
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
            .filter(|job| job.release_id.as_ref().is_none_or(|id| *id == job.id))
            .map(|job| job.sequence)
            .chain(
                completions
                    .iter()
                    .filter(|value| matches!(value["phase"].as_str(), Some("failed" | "succeeded")))
                    .filter(|value| {
                        value["release_id"].is_null() || value["release_id"] == value["id"]
                    })
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
                && !self.release_retains(&job)?
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
            if let Some(batch) = &job.workflow.batch {
                git::forget(
                    self.paths,
                    &self.config.common_git_dir,
                    &format!("base-{}", job.id),
                    &CommitId::new(batch.base.clone())?,
                )?;
            }
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
                    && operation.commit == *job.execution_commit()
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
            commit: job.execution_commit().clone(),
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
    include!("batch_tests.rs");

    #[test]
    fn default_repositories_share_one_slot_across_workflow_stages() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let shared = Queue::at(root.join("shared"), true)?.context("test shared queue")?;
        let (_a, mut first, base) = fixture()?;
        batch_workflow(&mut first, &base);
        let (_b, second, _) = fixture()?;
        let (_c, independent, _) = fixture()?;
        let a = select(&first, true)?;
        let b = select(&second, true)?;
        for (paths, project) in [(&a, &first), (&b, &second)] {
            submit_in(
                paths,
                &project.repo,
                "HEAD",
                project.workflow.clone(),
                QueueMode::Global,
                Some(&shared),
            )?;
        }
        let (c, independent_job) = enqueue(&independent, "HEAD")?;
        let _runner_a = store::lock(&a.root.join("runner.lock"), false)?;
        let _runner_b = store::lock(&b.root.join("runner.lock"), false)?;
        let _worker_a = store::lock(&a.root.join("worker.lock"), false)?;
        let _worker_b = store::lock(&b.root.join("worker.lock"), false)?;
        let (store_a, config_a) = configured(&a)?;
        let (store_b, config_b) = configured(&b)?;
        let mut worker_a = Worker::new(&a, store_a, config_a);
        let mut worker_b = Worker::new(&b, store_b, config_b);
        worker_a.shared = Queue::at(root.join("shared"), false)?;
        worker_b.shared = Queue::at(root.join("shared"), false)?;

        assert_eq!(worker_b.tick()?["state"], "idle");
        assert_eq!(worker_a.tick()?["phase"], "checking");
        assert_eq!(worker_b.tick()?["state"], "idle");
        assert_eq!(finish_run(&c, &independent_job)?["phase"], "succeeded");
        for phase in ["composing", "preparing", "deploying"] {
            assert_eq!(worker_a.tick()?["phase"], phase);
            assert_eq!(worker_b.tick()?["state"], "idle");
        }
        assert_eq!(worker_a.tick()?["phase"], "succeeded");
        assert_eq!(worker_b.tick()?["phase"], "checking");
        assert_eq!(worker_b.tick()?["phase"], "succeeded");
        assert!(shared.holder()?.is_none());
        Ok(())
    }

    #[test]
    fn a_repository_can_change_queue_mode_after_its_work_settles() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().canonicalize()?;
        let shared = Queue::at(root.join("shared"), true)?.context("test shared queue")?;
        let (_repository, project, _) = fixture()?;
        let (paths, repository_job) = enqueue(&project, "HEAD")?;
        let submit_global = || {
            submit_in(
                &paths,
                &project.repo,
                "HEAD",
                project.workflow.clone(),
                QueueMode::Global,
                Some(&shared),
            )
        };
        let error = submit_global().expect_err("unfinished jobs keep their queue mode");
        assert!(error.to_string().contains("cannot change"));
        assert_eq!(finish_run(&paths, &repository_job)?["phase"], "succeeded");
        assert_eq!(submit_global()?["phase"], "queued");
        let (store, config) = configured(&paths)?;
        let _worker = store::lock(&paths.root.join("worker.lock"), false)?;
        let mut worker = Worker::new(&paths, store, config);
        worker.shared = Some(shared);
        assert_eq!(worker.tick()?["phase"], "checking");
        assert_eq!(worker.tick()?["phase"], "succeeded");
        Ok(())
    }
}
