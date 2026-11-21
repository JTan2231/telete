//! Requeue every frozen input of a known failed release under fresh identities.

use super::{
    CommitId, Job, JobId, Phase, QueueMode, Result, Worker, configured, ensure, git, json,
};
use super::{new_job, queue_mode, shared_queue, store};
use crate::paths::Paths;
use crate::store::{RecheckSource, ReleaseBatch, ReleaseRetry, RetryMember, Store};
use anyhow::Context;
use serde_json::Value;

pub(super) fn awaiting_retry_recipe(store: &Store, jobs: &[Job], leader: &Job) -> Result<bool> {
    let Some(batch) = &leader.batch else {
        return Ok(false);
    };
    Ok(leader.phase == Phase::Failed
        && store.get::<u64>("latest_finished_sequence")? != Some(leader.sequence)
        && batch.members.iter().all(|id| {
            jobs.iter()
                .any(|job| job.id == *id && Worker::can_settle_release_member(job))
        })
        && !store.retry_available(&leader.id)?)
}

pub(crate) fn request_retry(paths: &Paths, id: &str) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = configured(paths)?;
    let (release, recipe) = store.snapshot(|| load_recipe(&store, id))?;
    ensure!(
        recipe.members.iter().any(|member| member.id.0 == id),
        "job is missing from the retained release membership"
    );
    if recipe.attempt.is_some() {
        return receipt(&release, &recipe);
    }
    let mode = queue_mode(&store)?;
    let admit = |order| {
        store.transaction(|| {
            let mut recipe = store
                .retry_recipe(&release)?
                .context("shared failure has no retained retry inputs")?;
            if recipe.attempt.is_none() {
                admit_members(&store, &release, &mut recipe, order)?;
                store.save_retry(&release, &recipe)?;
            }
            receipt(&release, &recipe)
        })
    };
    if mode == QueueMode::Global {
        shared_queue(mode, true)?
            .context("shared queue state is missing")?
            .enqueue(paths, |order| admit(Some(order)))
    } else {
        admit(None)
    }
}

fn load_recipe(store: &Store, id: &str) -> Result<(JobId, ReleaseRetry)> {
    let release = failed_release(store, id)?;
    if let Some(recipe) = store.retry_recipe(&release)? {
        return Ok((release, recipe));
    }
    let jobs = store.jobs()?;
    if let Some(leader) = jobs.iter().find(|job| job.id == release)
        && awaiting_retry_recipe(store, &jobs, leader)?
    {
        anyhow::bail!("shared failure is settling; retry when its receipts are ready");
    }
    anyhow::bail!("shared failure has no retained retry inputs");
}

fn failed_release(store: &Store, id: &str) -> Result<JobId> {
    if let Some(completion) = store.completion(id)? {
        ensure!(
            completion["phase"] == "failed",
            "only failed releases can be retried"
        );
        return JobId::new(
            completion["release_id"]
                .as_str()
                .context("job is not a shared release member")?,
        );
    }
    let member = store.job(id)?;
    ensure!(
        member.phase == Phase::Failed && !member.unresolved && member.operation.is_none(),
        "only settled failed releases can be retried"
    );
    member
        .release_id
        .context("job is not a shared release member")
}

fn admit_members(
    store: &Store,
    previous: &JobId,
    recipe: &mut ReleaseRetry,
    order: Option<u64>,
) -> Result<()> {
    let leader_index = recipe.leader_index(previous)?;
    let ids = recipe
        .members
        .iter()
        .map(|_| JobId::new(uuid::Uuid::now_v7().to_string()))
        .collect::<Result<Vec<_>>>()?;
    let leader = &ids[leader_index];
    let combined = recipe.members[leader_index].workflow.combined_release();
    for (index, (member, id)) in recipe.members.iter().zip(&ids).enumerate() {
        let mut job = new_job(id.clone(), member.commit.clone(), member.workflow.clone());
        job.release_id = Some(leader.clone());
        job.retry_of = Some(member.id.clone());
        job.shared_order = order;
        job.development_worktree
            .clone_from(&member.development_worktree);
        if combined {
            job.phase = Phase::WaitingRelease;
        } else {
            job.recheck.clone_from(&member.checked);
        }
        if index == leader_index {
            job.batch = Some(ReleaseBatch {
                members: ids.clone(),
                base: recipe.base.clone(),
                commit: None,
                composition_head: None,
                merge_conflict: None,
            });
        }
        store.insert(&job)?;
    }
    recipe.attempt = Some(ids);
    Ok(())
}

fn receipt(previous: &JobId, recipe: &ReleaseRetry) -> Result<Value> {
    let ids = recipe
        .attempt
        .as_ref()
        .context("release retry is not admitted")?;
    ensure!(
        ids.len() == recipe.members.len(),
        "release retry lost a member"
    );
    let release = &ids[recipe.leader_index(previous)?];
    let members = recipe
        .members
        .iter()
        .zip(ids)
        .map(|(member, id)| json!({"id":id,"retry_of":member.id,"commit":member.commit}))
        .collect::<Vec<_>>();
    Ok(json!({"id":release,"release_id":release,"retry_of":previous,"members":members}))
}

fn inputs(recipe: &ReleaseRetry) -> impl Iterator<Item = &CommitId> {
    std::iter::once(&recipe.base).chain(recipe.members.iter().map(|member| {
        member
            .checked
            .as_ref()
            .map_or(&member.commit, |checked| &checked.commit)
    }))
}

fn pin_name(release: &JobId, index: usize) -> String {
    format!("retry-{release}-{index}")
}

impl Worker<'_> {
    pub(super) fn retain_failed_release(&self, leader: &Job, members: &[Job]) -> Result<()> {
        let batch = leader.batch.as_ref().context("release has no membership")?;
        if leader.phase != Phase::Failed
            || self.store.retry_available(&leader.id)?
            || members.len() != batch.members.len()
        {
            return Ok(());
        }
        let base = CommitId::new(
            leader
                .workflow
                .batch
                .as_ref()
                .context("batch not selected")?
                .base
                .clone(),
        )?;
        let combined = leader.workflow.combined_release();
        let members = members
            .iter()
            .map(|job| RetryMember {
                id: job.id.clone(),
                commit: job.commit.clone(),
                checked: if combined {
                    None
                } else if job.runs.iter().any(|run| {
                    run.stage == "check"
                        && run.commit == *job.effective_commit()
                        && run.result.success()
                }) {
                    Some(RecheckSource {
                        commit: job.effective_commit().clone(),
                        analyzed: job.analyzed_commit.clone(),
                        repaired: job.repaired_commit.clone(),
                    })
                } else {
                    job.recheck.clone()
                },
                workflow: job.workflow.clone(),
                development_worktree: job.development_worktree.clone(),
            })
            .collect();
        let recipe = ReleaseRetry {
            base,
            members,
            attempt: None,
        };
        // Keep exact source available across normal cleanup and Git GC. The
        // deterministic pins can be joined after an interrupted receipt write.
        for (index, commit) in inputs(&recipe).enumerate() {
            git::advance(
                self.paths,
                &self.config.common_git_dir,
                &git::private_ref(&pin_name(&leader.id, index))?,
                None,
                commit,
            )?;
        }
        self.store.save_retry(&leader.id, &recipe)
    }

    pub(super) fn prune_retry_inputs(&self) -> Result<()> {
        self.store.prune_retries(|release, recipe| {
            for (index, commit) in inputs(recipe).enumerate() {
                git::forget(
                    self.paths,
                    &self.config.common_git_dir,
                    &pin_name(release, index),
                    commit,
                )?;
            }
            Ok(())
        })
    }
}
