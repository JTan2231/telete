//! Publish the prepared source through the same queue that deploys it.

use super::{CommitId, Context, Job, Phase, Queue, QueueMode, Result, Worker, ensure, git, store};
use crate::paths::Paths;
use crate::workflow::IntegrationMode;
use serde_json::Value;

pub(crate) fn request_merge(paths: &Paths, id: &str) -> Result<Value> {
    let _admission = store::lock(&paths.root.join("admission.lock"), true)?;
    let (store, _) = super::configured(paths)?;
    if let Some(completion) = store.completion(id)? {
        ensure!(
            !completion["merged_commit"].is_null(),
            "job has no pending merge"
        );
        return Ok(completion);
    }
    let member = store.job(id)?;
    if member.phase.terminal() && member.merged_commit.is_some() {
        return member.output();
    }
    let mut leader = store.job(&member.release_id.as_ref().unwrap_or(&member.id).0)?;
    if leader.phase == Phase::WaitingMerge {
        ensure!(
            !leader.cancel_requested,
            "release {} is cancelling",
            leader.id
        );
        if !leader.merge_requested {
            leader.merge_requested = true;
            if super::queue_mode(&store)? == QueueMode::Global {
                let queue = Queue::open(false)?.context("shared queue state is missing")?;
                queue.enqueue(paths, |order| {
                    leader.shared_order = Some(order);
                    store.save(&mut leader)
                })?;
            } else {
                store.save(&mut leader)?;
            }
        }
    } else {
        ensure!(
            leader.merged_commit.is_some() || leader.phase == Phase::Merging,
            "release {} has no pending merge",
            leader.id
        );
    }
    super::job_status(&store, &leader.id.0)
}

impl Worker<'_> {
    pub(super) fn retain_checked(&self, job: &mut Job) -> Result<()> {
        if job.resulting_commit.is_none() {
            let checked = job.effective_commit().clone();
            git::advance(
                self.paths,
                &self.config.common_git_dir,
                &git::result_ref(&job.id.0)?,
                None,
                &checked,
            )?;
            job.resulting_commit = Some(checked);
        }
        Ok(())
    }

    pub(super) fn compose_single(&self, job: &mut Job) -> Result<()> {
        self.retain_checked(job)?;
        let integration = job
            .workflow
            .integration()
            .context("integration not selected")?;
        let repo = &self.config.common_git_dir;
        let base = git::branch_tip(self.paths, repo, &integration.branch)?;
        let checkout = self.worktree(job);
        git::ensure_worktree(self.paths, repo, &checkout, &base)?;
        git::merge_worktree_source(self.paths, repo, &checkout, job.effective_commit())
            .context("could not combine the checked source with the integration branch")?;
        let candidate = git::commit(self.paths, &checkout, "HEAD")?;
        let previous = git::ref_value(self.paths, repo, &git::release_ref(&job.id.0)?)?;
        git::retain_release(
            self.paths,
            repo,
            &checkout,
            &job.id.0,
            [&base, job.effective_commit()],
            previous.as_ref(),
            &candidate,
        )?;
        job.integration_base = Some(base);
        job.release_commit = Some(candidate);
        if job.workflow.prepare.is_some() {
            self.save(job, Phase::Preparing)
        } else {
            self.ready_to_merge(job)
        }
    }

    pub(super) fn ready_to_merge(&self, job: &mut Job) -> Result<()> {
        let mode = job
            .workflow
            .integration()
            .context("integration not selected")?
            .mode;
        if mode == IntegrationMode::Manual {
            job.merge_requested = false;
            job.outcome_message = Some(format!(
                "Run telete merge {} to integrate this release",
                job.id
            ));
            self.save(job, Phase::WaitingMerge)
        } else {
            self.save(job, Phase::Merging)
        }
    }

    pub(super) fn integrate(&self, job: &mut Job) -> Result<()> {
        let destination = job
            .workflow
            .integration()
            .context("integration not selected")?
            .branch
            .clone();
        let candidate = job
            .release_commit
            .clone()
            .context("merge has no release source")?;
        let base = job
            .integration_base
            .as_ref()
            .context("merge has no destination base")?;
        let repo = &self.config.common_git_dir;
        let head = git::branch_tip(self.paths, repo, &destination)?;
        if job.merged_commit.as_ref() != Some(&candidate)
            && head != *base
            && !git::is_ancestor(self.paths, repo, &candidate, &head)?
        {
            job.merge_requested = false;
            job.last_error = None;
            job.outcome_message = None;
            return if job.batch.is_some() {
                self.claim_release(job)
            } else {
                self.compose_single(job)
            };
        }
        if let Err(error) = git::publish_branch(self.paths, repo, &destination, base, &candidate) {
            job.last_error = Some(format!("{error:#}"));
            job.outcome_message = Some(format!(
                "Could not advance {}; run telete merge {} after resolving the issue",
                destination, job.id
            ));
            job.merge_requested = false;
            return self.save(job, Phase::WaitingMerge);
        }
        job.merged_commit = Some(candidate.clone());
        self.store.save(job)?;

        let mut warnings = Vec::new();
        if let Some(batch) = &job.batch {
            for id in &batch.members {
                let member = self.store.job(&id.0)?;
                self.cleanup_merged_worktree(&member, &destination, &candidate, &mut warnings);
            }
        } else {
            self.cleanup_merged_worktree(job, &destination, &candidate, &mut warnings);
        }
        job.merge_cleanup = warnings;
        job.merge_requested = false;
        job.outcome_message = None;
        job.last_error = None;
        self.save(job, Phase::Deploying)
    }

    fn cleanup_merged_worktree(
        &self,
        member: &Job,
        destination: &str,
        candidate: &CommitId,
        warnings: &mut Vec<String>,
    ) {
        if let Some(worktree) = &member.development_worktree
            && worktree.branch() != destination
            && let Err(error) = git::cleanup_development_worktree(
                self.paths,
                &self.config.common_git_dir,
                worktree,
                member.effective_commit(),
                candidate,
            )
        {
            warnings.push(format!("Job {} worktree cleanup: {error:#}", member.id));
        }
    }

    pub(super) fn confirm_integration(&self, job: &Job) -> Result<()> {
        if let Some(integration) = job.workflow.integration() {
            let candidate = job.execution_commit();
            let repo = &self.config.common_git_dir;
            let head = git::branch_tip(self.paths, repo, &integration.branch)?;
            ensure!(
                job.merged_commit.as_ref() == Some(candidate)
                    && git::is_ancestor(self.paths, repo, candidate, &head)?,
                "prepared release {candidate} is no longer integrated in {}",
                integration.branch
            );
            let checkout = self.worktree(job);
            git::ensure_detached(self.paths, &checkout)?;
            git::unchanged(self.paths, &checkout, candidate)?;
        }
        Ok(())
    }
}
