//! A closed group of checked sources shares preparation and deployment.

use super::{CommitId, Context, Job, Phase, Result, Worker, ensure, git, json, store};
use crate::store::ReleaseBatch;

fn compatible(left: &Job, right: &Job) -> bool {
    match (&left.workflow.batch, &right.workflow.batch) {
        (Some(left_batch), Some(right_batch)) => {
            left_batch.target == right_batch.target
                && left_batch.compose == right_batch.compose
                && left.workflow.prepare == right.workflow.prepare
                && left.workflow.deploy == right.workflow.deploy
                && left.workflow.parameters == right.workflow.parameters
        }
        _ => false,
    }
}

impl Worker<'_> {
    pub(super) fn defer_release(&self, job: &mut Job) -> Result<()> {
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
        let _admission = store::lock(&self.paths.root.join("admission.lock"), true)?;
        self.store.transaction(|| {
            if self.store.job(&job.id.0)?.cancel_requested {
                job.cancel_requested = true;
                return self.finish(
                    job,
                    Phase::Cancelled,
                    "Cancelled before release grouping".into(),
                    false,
                );
            }
            if job.release_id.is_none() {
                let selected = job.workflow.batch.as_ref().context("batch not selected")?;
                let base = CommitId::new(selected.base.clone())?;
                let jobs = self.store.jobs()?;
                // An incompatible request for this target is an ordering
                // boundary. Other targets and CI-only jobs remain independent.
                let mut candidates = Vec::new();
                for member in jobs {
                    if member.phase.terminal()
                        || member.cancel_requested
                        || member
                            .workflow
                            .batch
                            .as_ref()
                            .is_none_or(|batch| batch.target != selected.target)
                    {
                        continue;
                    }
                    if member.release_id.is_some() || !compatible(job, &member) {
                        if candidates.iter().any(|member: &Job| member.id == job.id) {
                            break;
                        }
                        candidates.clear();
                    } else {
                        candidates.push(member);
                    }
                }
                let mut members = Vec::new();
                for mut member in candidates {
                    if member.id == job.id {
                        members.push(job.id.clone());
                    } else {
                        members.push(member.id.clone());
                        member.release_id = Some(job.id.clone());
                        self.store.save(&mut member)?;
                    }
                }
                job.release_id = Some(job.id.clone());
                job.batch = Some(ReleaseBatch {
                    members,
                    base,
                    commit: None,
                });
            }
            self.save(job, Phase::WaitingRelease)
        })
    }

    /// Called under the admission lock and transaction. Closed membership never
    /// grows while the selected jobs finish their own CI.
    pub(super) fn claim_release(&self) -> Result<Option<Job>> {
        let jobs = self.store.jobs()?;
        for mut leader in jobs.iter().filter(|job| job.batch.is_some()).cloned() {
            if leader.phase != Phase::WaitingRelease {
                continue;
            }
            let selected = leader
                .workflow
                .batch
                .as_ref()
                .context("batch not selected")?;
            let earlier = jobs.iter().any(|job| {
                job.sequence < leader.sequence
                    && !job.phase.terminal()
                    && job.release_id.as_ref() != Some(&leader.id)
                    && job
                        .workflow
                        .batch
                        .as_ref()
                        .is_some_and(|batch| batch.target == selected.target)
            });
            if earlier {
                continue;
            }
            let batch = leader.batch.as_ref().context("release has no membership")?;
            let ready = batch.members.iter().all(|id| {
                jobs.iter().any(|job| {
                    job.id == *id
                        && job.phase == Phase::WaitingRelease
                        && !job.cancel_requested
                        && job.runs.iter().any(|run| {
                            run.stage == "check"
                                && run.commit == *job.effective_commit()
                                && run.result.success()
                        })
                })
            });
            if !ready {
                continue;
            }
            // The previous group may have deployed after this group closed.
            // Fix the cumulative base before creating any command identity.
            if let Some(base) = git::ref_value(
                self.paths,
                &self.config.common_git_dir,
                &git::release_target_ref(&selected.target)?,
            )? {
                leader
                    .batch
                    .as_mut()
                    .context("release has no membership")?
                    .base = base;
            }
            let base = leader
                .batch
                .as_ref()
                .context("release has no membership")?
                .base
                .clone();
            git::ensure_worktree(
                self.paths,
                &self.config.common_git_dir,
                &self.worktree(&leader),
                &base,
            )?;
            self.save(&mut leader, Phase::Composing)?;
            return Ok(Some(leader));
        }
        Ok(None)
    }

    pub(super) fn batch_input(&self, job: &Job) -> Result<serde_json::Value> {
        let batch = job.batch.as_ref().context("release has no membership")?;
        let mut members = Vec::new();
        for id in &batch.members {
            let member = self.store.job(&id.0)?;
            ensure!(
                member.release_id.as_ref() == Some(&job.id),
                "release member identity changed"
            );
            members.push(json!({
                "job_id": member.id,
                "commit": member.effective_commit(),
                "submitted_commit": member.commit,
                "analyzed_commit": member.analyzed_commit,
                "repaired_commit": member.repaired_commit,
                "parameters": member.workflow.parameters,
            }));
        }
        Ok(json!({"base":batch.base,"members":members}))
    }

    pub(super) fn retain_composition(
        &self,
        job: &mut Job,
        run: &crate::workflow::StageRun,
    ) -> Result<()> {
        let directory = run
            .command
            .context
            .as_ref()
            .context("compose has no context")?
            .value["outputs"]["directory"]
            .as_str()
            .context("compose has no output directory")?;
        let commit = CommitId::new(
            std::fs::read_to_string(std::path::Path::new(directory).join("commit"))?.trim(),
        )?;
        let input = self.batch_input(job)?;
        let members = input["members"]
            .as_array()
            .context("release has no members")?
            .iter()
            .map(|member| CommitId::new(member["commit"].as_str().context("member has no commit")?))
            .collect::<Result<Vec<_>>>()?;
        let batch = job.batch.as_ref().context("release has no membership")?;
        git::retain_release(
            self.paths,
            &self.config.common_git_dir,
            &self.worktree(job),
            &job.id.0,
            &batch.base,
            &members,
            &commit,
        )?;
        job.batch
            .as_mut()
            .context("release has no membership")?
            .commit = Some(commit.clone());
        job.release_commit = Some(commit);
        Ok(())
    }

    pub(super) fn record_release(&self, job: &Job) -> Result<()> {
        let batch = job.batch.as_ref().context("release has no membership")?;
        let selected = job.workflow.batch.as_ref().context("batch not selected")?;
        let commit = job
            .release_commit
            .as_ref()
            .context("release has no composed source")?;
        let reference = git::release_target_ref(&selected.target)?;
        let previous = git::ref_value(self.paths, &self.config.common_git_dir, &reference)?;
        ensure!(
            previous
                .as_ref()
                .is_none_or(|value| *value == batch.base || value == commit),
            "latest deployed source changed during release"
        );
        git::advance(
            self.paths,
            &self.config.common_git_dir,
            &reference,
            previous.as_ref(),
            commit,
        )
    }

    pub(super) fn settle_releases(&self) -> Result<()> {
        for mut leader in self
            .store
            .jobs()?
            .into_iter()
            .filter(|job| job.batch.is_some())
        {
            let batch = leader.batch.clone().context("release has no membership")?;
            let jobs = self.store.jobs()?;
            let mut members = Vec::new();
            for id in &batch.members {
                if let Some(member) = jobs.iter().find(|job| job.id == *id) {
                    members.push(member.clone());
                } else {
                    ensure!(leader.phase.terminal(), "pending release lost a member");
                }
            }
            // Never settle other members around an uncertain or active command.
            if members.iter().any(|job| {
                job.unresolved
                    || job.operation.is_some()
                    || (!job.phase.terminal()
                        && !matches!(
                            job.phase,
                            Phase::WaitingRelease | Phase::Queued | Phase::AwaitingRepair
                        ))
            }) {
                continue;
            }
            if !leader.phase.terminal() {
                let cancelled = members
                    .iter()
                    .any(|job| job.cancel_requested || job.phase == Phase::Cancelled);
                let failed = members.iter().any(|job| job.phase == Phase::Failed)
                    && members
                        .iter()
                        .all(|job| job.phase.terminal() || job.phase == Phase::WaitingRelease);
                if cancelled || failed {
                    let stopped = members
                        .iter()
                        .find(|job| job.cancel_requested || job.phase.terminal())
                        .map_or_else(|| leader.id.to_string(), |job| job.id.to_string());
                    self.finish(
                        &mut leader,
                        if cancelled {
                            Phase::Cancelled
                        } else {
                            Phase::Failed
                        },
                        format!("Shared release stopped by member {stopped}"),
                        false,
                    )?;
                }
            }
            if !leader.phase.terminal() {
                continue;
            }
            // Publish all receipts together; cleanup cannot prune a dependency
            // before every member has its shared outcome.
            self.store.transaction(|| {
                for mut member in members {
                    if member.id == leader.id || member.phase.terminal() {
                        continue;
                    }
                    member.release_commit.clone_from(&leader.release_commit);
                    self.finish(
                        &mut member,
                        leader.phase,
                        format!("Shared release {} {}", leader.id, leader.phase.name()),
                        false,
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    pub(super) fn release_retains(&self, job: &Job) -> Result<bool> {
        let Some(id) = &job.release_id else {
            return Ok(false);
        };
        let jobs = self.store.jobs()?;
        let Some(leader) = jobs.iter().find(|job| job.id == *id) else {
            return Ok(false);
        };
        if !leader.phase.terminal() {
            return Ok(true);
        }
        if leader.phase == Phase::Failed
            && job.id != leader.id
            && (job.last_error.is_some() || job.runs.iter().any(|run| !run.result.success()))
            && self.store.get::<u64>("latest_finished_sequence")? == Some(leader.sequence)
        {
            return Ok(true);
        }
        Ok(leader.batch.as_ref().is_some_and(|batch| {
            batch.members.iter().any(|id| {
                jobs.iter()
                    .any(|member| member.id == *id && !member.phase.terminal())
            })
        }))
    }
}
