//! A closed group of checked sources shares preparation and deployment.

use super::{CommitId, Context, Job, Phase, Result, Worker, ensure, git, json, store};
use crate::store::{MergeConflict, ReleaseBatch};

fn compatible(left: &Job, right: &Job) -> bool {
    match (&left.workflow.batch, &right.workflow.batch) {
        (Some(left_batch), Some(right_batch)) => {
            left_batch.target == right_batch.target
                && left_batch.compose == right_batch.compose
                && left_batch.merge_repair == right_batch.merge_repair
                && left.workflow.prepare == right.workflow.prepare
                && left.workflow.deploy == right.workflow.deploy
                && left.workflow.integration() == right.workflow.integration()
                && left.workflow.parameters == right.workflow.parameters
        }
        _ => false,
    }
}

impl Worker<'_> {
    pub(super) fn defer_release(&self, job: &mut Job) -> Result<()> {
        self.retain_checked(job)?;
        let _admission = store::lock(&self.paths.root.join("admission.lock"), true)?;
        let global = job.shared_order.is_some();
        let mut defer = |order| {
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
                        composition_head: None,
                        merge_conflict: None,
                    });
                }
                job.shared_order = order;
                self.save(job, Phase::WaitingRelease)
            })
        };
        if global {
            let shared = self
                .shared
                .as_ref()
                .context("shared queue state is missing")?;
            shared.enqueue(self.paths, |order| defer(Some(order)))?;
            if job.phase == Phase::WaitingRelease
                && job
                    .batch
                    .as_ref()
                    .is_some_and(|batch| batch.members.len() == 1)
                && Self::ready_releases(&self.store.jobs()?)?
                    .iter()
                    .any(|ready| ready.id == job.id)
            {
                self.claim_release(job)?;
            }
            Ok(())
        } else {
            defer(None)
        }
    }

    /// Closed membership never grows while the selected jobs finish their CI.
    pub(super) fn ready_releases(jobs: &[Job]) -> Result<Vec<Job>> {
        let mut releases = Vec::new();
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
            // The last member to finish makes composition ready at its turn.
            leader.shared_order = jobs
                .iter()
                .filter(|member| batch.members.contains(&member.id))
                .filter_map(|member| member.shared_order)
                .max();
            releases.push(leader);
        }
        Ok(releases)
    }

    /// Select the cumulative base before creating any command identity.
    pub(super) fn claim_release(&self, leader: &mut Job) -> Result<()> {
        let selected = leader
            .workflow
            .batch
            .as_ref()
            .context("batch not selected")?;
        let batch = leader.batch.as_mut().context("release has no membership")?;
        if let Some(base) = git::ref_value(
            self.paths,
            &self.config.common_git_dir,
            &git::release_target_ref(&selected.target)?,
        )? {
            batch.base = base;
        }
        let base = batch.base.clone();
        let repo = &self.config.common_git_dir;
        let branch_tip = leader
            .workflow
            .integration()
            .map(|integration| git::branch_tip(self.paths, repo, &integration.branch))
            .transpose()?;
        let checkout = self.worktree(leader);
        git::ensure_worktree(
            self.paths,
            repo,
            &checkout,
            branch_tip.as_ref().unwrap_or(&base),
        )?;
        let batch = leader.batch.as_mut().context("release has no membership")?;
        batch.composition_head = if branch_tip.is_some() {
            git::merge_worktree_source(self.paths, repo, &checkout, &base)
                .context("could not combine the integration branch with the preceding release")?;
            Some(git::commit(self.paths, &checkout, "HEAD")?)
        } else {
            None
        };
        batch.merge_conflict = None;
        leader.integration_base = branch_tip;
        leader.release_commit = None;
        self.save(leader, Phase::Composing)
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
            std::iter::once(&batch.base)
                .chain(&batch.composition_head)
                .chain(&job.integration_base)
                .chain(&members),
            batch.commit.as_ref(),
            &commit,
        )?;
        job.batch
            .as_mut()
            .context("release has no membership")?
            .commit = Some(commit.clone());
        job.release_commit = Some(commit);
        Ok(())
    }

    pub(super) fn merge_conflict(&self, job: &Job) -> Result<Option<MergeConflict>> {
        let checkout = self.worktree(job);
        let Some(member) = git::merge_head(self.paths, &checkout)? else {
            return Ok(None);
        };
        if git::value(self.paths, &checkout, &["ls-files", "--unmerged"])?.is_empty() {
            return Ok(None);
        }
        git::ensure_detached(self.paths, &checkout)?;
        let head = git::commit(self.paths, &checkout, "HEAD")?;
        let input = self.batch_input(job)?;
        ensure!(
            input["members"]
                .as_array()
                .context("release has no members")?
                .iter()
                .any(|entry| entry["commit"] == member.0),
            "merge conflict is not with a checked release member: {member}"
        );
        ensure!(
            git::is_ancestor(self.paths, &checkout, job.execution_commit(), &head)?
                && !git::is_ancestor(self.paths, &checkout, &member, &head)?,
            "merge conflict does not advance composition"
        );
        Ok(Some(MergeConflict { head, member }))
    }

    pub(super) fn accept_merge_repair(&self, job: &mut Job) -> Result<()> {
        let checkout = self.worktree(job);
        let batch = job.batch.as_mut().context("release has no membership")?;
        let conflict = batch
            .merge_conflict
            .as_ref()
            .context("merge repair has no conflict")?;
        ensure!(
            git::common(self.paths, &checkout)? == self.config.common_git_dir,
            "merge repair checkout belongs to another repository"
        );
        git::ensure_detached(self.paths, &checkout)?;
        let commit = git::commit(self.paths, &checkout, "HEAD")?;
        let parents = git::value(
            self.paths,
            &checkout,
            &["rev-list", "--parents", "-n", "1", &commit.0],
        )?;
        ensure!(
            parents == format!("{commit} {} {}", conflict.head, conflict.member),
            "merge repair must commit the merge of {} and {}",
            conflict.head,
            conflict.member
        );
        ensure!(
            git::merge_head(self.paths, &checkout)?.is_none(),
            "merge repair left a merge in progress"
        );
        git::unchanged(self.paths, &checkout, &commit)?;
        batch.composition_head = Some(commit);
        batch.merge_conflict = None;
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
                            Phase::WaitingRelease
                                | Phase::Queued
                                | Phase::AwaitingRepair
                                | Phase::WaitingMerge
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
                    member.merged_commit.clone_from(&leader.merged_commit);
                    member.merge_cleanup.clone_from(&leader.merge_cleanup);
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
